//! Per-task timing fixtures: no process environment, external DNS or global locks.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

tokio::task_local! {
    pub(super) static FIXTURE: Arc<Fixture>;
}

#[derive(Default)]
pub(super) struct Fixture {
    preflight: Duration,
    connect: Duration,
    started: AtomicUsize,
    completed: AtomicUsize,
    active: AtomicUsize,
    started_notify: tokio::sync::Notify,
    retired_notify: tokio::sync::Notify,
}

pub(super) async fn delay_preflight() {
    let delay = FIXTURE.try_with(|f| f.preflight).unwrap_or_default();
    tokio::time::sleep(delay).await;
}

pub(super) fn delay_connect<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let fixture = FIXTURE.try_with(Arc::clone).ok();
    async move {
        struct Active(Arc<Fixture>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
                self.0.retired_notify.notify_one();
            }
        }
        let _active = fixture.as_ref().map(|f| {
            f.started.fetch_add(1, Ordering::SeqCst);
            f.active.fetch_add(1, Ordering::SeqCst);
            f.started_notify.notify_one();
            Active(Arc::clone(f))
        });
        if let Some(f) = &fixture {
            tokio::time::sleep(f.connect).await;
        }
        let result = future.await;
        if let Some(f) = &fixture {
            f.completed.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

struct Server {
    url: url::Url,
    accepted: Arc<AtomicUsize>,
    received: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(header_delay: Duration, redirects: usize) -> Self {
        Self::start(header_delay, redirects, None).await
    }
    async fn start(
        header_delay: Duration,
        redirects: usize,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        let responses = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(tokio::sync::Notify::new());
        let notify = Arc::clone(&received);
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        count.fetch_add(1, Ordering::SeqCst);
                        let responses = Arc::clone(&responses);
                        let notify = Arc::clone(&notify);
                        let gate = gate.clone();
                        tasks.spawn(async move {
                            loop {
                                let mut head = Vec::new();
                                while !head.ends_with(b"\r\n\r\n") {
                                    let mut byte = [0];
                                    if stream.read_exact(&mut byte).await.is_err() { return; }
                                    head.push(byte[0]);
                                }
                                let index = responses.fetch_add(1, Ordering::SeqCst);
                                notify.notify_one();
                                if index == 0 && let Some(gate) = &gate {
                                    gate.acquire().await.unwrap().forget();
                                }
                                tokio::time::sleep(header_delay).await;
                                let response = if index < redirects {
                                    "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                } else {
                                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Type: text/event-stream\r\n\r\n"
                                };
                                if stream.write_all(response.as_bytes()).await.is_err() { return; }
                                if response.contains("Connection: close") { return; }
                            }
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Self {
            url,
            accepted,
            received,
            task,
        }
    }
    fn config(&self, connect_timeout: Duration) -> HttpConfig {
        HttpConfig {
            allowed_schemes: vec!["http".into()],
            allowed_hosts: vec!["127.0.0.1".into()],
            allowed_ports: vec![self.url.port().unwrap()],
            allow_private_ips: true,
            connect_timeout,
            ..HttpConfig::default()
        }
    }
    fn request(&self) -> HttpRequest {
        HttpRequest {
            method: hyper::Method::GET,
            url: self.url.clone(),
            headers: vec![],
            body: None,
        }
    }
}

async fn perform(
    stream: bool,
    client: &HttpClient,
    config: &HttpConfig,
    request: &HttpRequest,
    deadline: Instant,
) -> VmResult<()> {
    if stream {
        let mut sent = 0;
        with_deadline(
            deadline,
            open_stream_response(client, config, request, deadline, &mut sent),
        )
        .await?;
    } else {
        perform_buffered_request(client, config, request, deadline).await?;
    }
    Ok(())
}

async fn combined_phase_bound(stream: bool) {
    let server = Server::new(Duration::ZERO, 0).await;
    let config = server.config(Duration::from_millis(200));
    let client = build_client(&config);
    let fixture = Arc::new(Fixture {
        preflight: Duration::from_millis(120),
        connect: Duration::from_millis(120),
        ..Default::default()
    });
    let result = FIXTURE
        .scope(
            Arc::clone(&fixture),
            perform(
                stream,
                &client,
                &config,
                &server.request(),
                Instant::now() + Duration::from_secs(3),
            ),
        )
        .await;
    assert!(
        result.is_err(),
        "preflight + actual connector exceeded the original connection budget, but production request succeeded"
    );
    assert!(result.unwrap_err().to_string().contains("deadline"));
    assert_eq!(
        fixture.started.load(Ordering::SeqCst),
        1,
        "actual production connector must be exercised"
    );
    assert_eq!(fixture.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.active.load(Ordering::SeqCst),
        0,
        "timed out connector future must retire"
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn buffered_combined_phase_uses_original_deadline() {
    combined_phase_bound(false).await;
}
#[tokio::test]
async fn sse_combined_phase_uses_original_deadline() {
    combined_phase_bound(true).await;
}

#[tokio::test]
async fn same_origin_concurrent_buffered_and_sse_keep_distinct_budgets() {
    for short_stream in [false, true] {
        let server = Server::new(Duration::ZERO, 0).await;
        let long_config = server.config(Duration::from_secs(1));
        let short_config = server.config(Duration::from_millis(200));
        let client = build_client(&long_config);
        let short = Arc::new(Fixture {
            preflight: Duration::from_millis(120),
            connect: Duration::from_millis(120),
            ..Default::default()
        });
        let long = Arc::new(Fixture {
            connect: Duration::from_millis(300),
            ..Default::default()
        });
        let request = server.request();
        let deadline = Instant::now() + Duration::from_secs(3);
        let (short_result, long_result) = tokio::join!(
            FIXTURE.scope(
                Arc::clone(&short),
                perform(short_stream, &client, &short_config, &request, deadline)
            ),
            FIXTURE.scope(
                Arc::clone(&long),
                perform(!short_stream, &client, &long_config, &request, deadline)
            ),
        );
        assert!(short_result.unwrap_err().to_string().contains("deadline"));
        long_result.unwrap();
        assert_eq!(short.completed.load(Ordering::SeqCst), 0);
        assert_eq!(long.completed.load(Ordering::SeqCst), 1);
        assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn fast_connection_allows_slow_headers_and_pool_reuse_for_both_callers() {
    let server = Server::new(Duration::from_millis(300), 0).await;
    let config = server.config(Duration::from_millis(200));
    let client = build_client(&config);
    let request = server.request();
    // Fresh connection completes promptly; only headers exceed connect_timeout.
    perform(
        false,
        &client,
        &config,
        &request,
        Instant::now() + Duration::from_secs(3),
    )
    .await
    .unwrap();
    let fixture = Arc::new(Fixture {
        connect: Duration::from_secs(1),
        ..Default::default()
    });
    for stream in [false, true] {
        FIXTURE
            .scope(
                Arc::clone(&fixture),
                perform(
                    stream,
                    &client,
                    &config,
                    &request,
                    Instant::now() + Duration::from_secs(3),
                ),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        fixture.started.load(Ordering::SeqCst),
        0,
        "reuse must not start a connector or inherit the old deadline"
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn redirect_hops_get_new_phase_budget_but_keep_the_original_total_cap() {
    for stream in [false, true] {
        for limited in [false, true] {
            let server = Server::new(Duration::ZERO, 1).await;
            let config = server.config(Duration::from_millis(200));
            let client = build_client(&config);
            let fixture = Arc::new(Fixture {
                preflight: Duration::from_millis(120),
                ..Default::default()
            });
            let budget = if limited {
                Duration::from_millis(180)
            } else {
                Duration::from_secs(3)
            };
            let result = FIXTURE
                .scope(
                    Arc::clone(&fixture),
                    perform(
                        stream,
                        &client,
                        &config,
                        &server.request(),
                        Instant::now() + budget,
                    ),
                )
                .await;
            if limited {
                assert!(result.unwrap_err().to_string().contains("deadline"));
                assert_eq!(fixture.completed.load(Ordering::SeqCst), 1);
            } else {
                result.unwrap();
                assert_eq!(fixture.completed.load(Ordering::SeqCst), 2);
            }
        }
    }
}

#[tokio::test]
async fn overall_cap_also_bounds_new_connector_after_preflight() {
    for stream in [false, true] {
        let server = Server::new(Duration::ZERO, 0).await;
        let config = server.config(Duration::from_secs(1));
        let client = build_client(&config);
        let fixture = Arc::new(Fixture {
            preflight: Duration::from_millis(120),
            connect: Duration::from_millis(120),
            ..Default::default()
        });
        let result = FIXTURE
            .scope(
                Arc::clone(&fixture),
                perform(
                    stream,
                    &client,
                    &config,
                    &server.request(),
                    Instant::now() + Duration::from_millis(200),
                ),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("deadline"));
        assert_eq!(fixture.started.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.completed.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cancel_during_connector_retires_future_and_releases_permit() {
    for stream in [false, true] {
        let server = Server::new(Duration::ZERO, 0).await;
        let config = server.config(Duration::from_secs(3));
        let client = build_client(&config);
        let request = server.request();
        let admission = super::super::policy::ConnectionAdmission::new(1);
        let permit = admission.acquire().unwrap();
        let fixture = Arc::new(Fixture {
            connect: Duration::from_secs(1),
            ..Default::default()
        });
        let task_fixture = Arc::clone(&fixture);
        let task = tokio::spawn(async move {
            let _permit = permit;
            FIXTURE
                .scope(
                    task_fixture,
                    perform(
                        stream,
                        &client,
                        &config,
                        &request,
                        Instant::now() + Duration::from_secs(5),
                    ),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), fixture.started_notify.notified())
            .await
            .unwrap();
        assert!(admission.acquire().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.completed.load(Ordering::SeqCst), 0);
        assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
        drop(admission.acquire().unwrap());
    }
}

#[tokio::test]
async fn pool_checkout_winner_leaves_background_connector_on_original_deadline() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let server = Server::start(Duration::ZERO, 0, Some(Arc::clone(&gate))).await;
    let config = server.config(Duration::from_millis(200));
    let client = build_client(&config);
    let request = server.request();
    let first_client = client.clone();
    let first_config = config.clone();
    let first_request = request.clone();
    let first = tokio::spawn(async move {
        perform(
            false,
            &first_client,
            &first_config,
            &first_request,
            Instant::now() + Duration::from_secs(3),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), server.received.notified())
        .await
        .unwrap();
    // No idle connection yet: force Hyper to begin its new-connection race.
    let fixture = Arc::new(Fixture {
        connect: Duration::from_millis(300),
        ..Default::default()
    });
    let task_fixture = Arc::clone(&fixture);
    let second = tokio::spawn(async move {
        FIXTURE
            .scope(
                task_fixture,
                perform(
                    true,
                    &client,
                    &config,
                    &request,
                    Instant::now() + Duration::from_secs(3),
                ),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), fixture.started_notify.notified())
        .await
        .unwrap();
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture.retired_notify.notified())
        .await
        .unwrap();
    assert_eq!(fixture.started.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.completed.load(Ordering::SeqCst),
        0,
        "spawned losing connection must not restart or lose its phase deadline"
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn executor_captures_each_submission_context_and_missing_context_fails_closed() {
    use hyper::rt::Executor;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let now = Instant::now();
    for deadline in [
        now + Duration::from_millis(200),
        now + Duration::from_secs(2),
    ] {
        let tx = tx.clone();
        let gate = Arc::clone(&gate);
        CONNECT_DEADLINE
            .scope(deadline, async {
                HttpExecutor.execute(async move {
                    gate.acquire().await.unwrap().forget();
                    tx.send(CONNECT_DEADLINE.try_with(|d| *d).unwrap()).unwrap();
                });
            })
            .await;
    }
    assert!(CONNECT_DEADLINE.try_with(|d| *d).is_err());
    gate.add_permits(2);
    let mut observed = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    observed.sort();
    assert_eq!(
        observed,
        vec![
            now + Duration::from_millis(200),
            now + Duration::from_secs(2)
        ]
    );
    let server = Server::new(Duration::ZERO, 0).await;
    let client = build_client(&server.config(Duration::from_secs(1)));
    let error = client
        .get(server.url.as_str().parse().unwrap())
        .await
        .unwrap_err();
    assert!(
        error
            .source()
            .unwrap()
            .to_string()
            .contains("deadline context missing"),
        "{error:?}"
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
}
