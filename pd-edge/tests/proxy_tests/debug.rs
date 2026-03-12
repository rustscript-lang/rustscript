use super::support::*;

#[tokio::test]
async fn debug_session_lifecycle_endpoints_work() {
    let (_data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();
    let debug_addr = reserve_tcp_addr();

    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let status_response = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = status_response
        .text()
        .await
        .expect("status body should read");
    assert!(status_body.contains(r#""active":true"#));
    assert!(status_body.contains(r#""header_name":"x-debug""#));

    let stop_response = client
        .delete(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("stop request should complete");
    assert_eq!(stop_response.status(), StatusCode::NO_CONTENT);

    let status_after_stop = client
        .get(format!("http://{admin_addr}/debug/session"))
        .send()
        .await
        .expect("status request should complete");
    assert_eq!(status_after_stop.status(), StatusCode::OK);
    let status_after_stop_body = status_after_stop
        .text()
        .await
        .expect("status body should read");
    assert!(status_after_stop_body.contains(r#""active":false"#));

    data_handle.abort();
    admin_handle.abort();
}

#[tokio::test]
async fn debug_attached_request_does_not_block_non_debug_requests() {
    struct AbortProxyOnDrop {
        data: JoinHandle<()>,
        admin: JoinHandle<()>,
    }

    impl Drop for AbortProxyOnDrop {
        fn drop(&mut self) {
            self.data.abort();
            self.admin.abort();
        }
    }

    struct AbortTaskOnDrop<T> {
        handle: Option<JoinHandle<T>>,
    }

    impl<T> AbortTaskOnDrop<T> {
        fn new(handle: JoinHandle<T>) -> Self {
            Self {
                handle: Some(handle),
            }
        }

        async fn join(mut self) -> Result<T, tokio::task::JoinError> {
            self.handle.take().expect("join handle should exist").await
        }
    }

    impl<T> Drop for AbortTaskOnDrop<T> {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let _abort_proxy = AbortProxyOnDrop {
        data: data_handle,
        admin: admin_handle,
    };
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = AbortTaskOnDrop::new(tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    }));

    tokio::time::sleep(Duration::from_millis(150)).await;

    let non_debug = timeout(
        Duration::from_secs(2),
        client.get(format!("http://{data_addr}/normal")).send(),
    )
    .await
    .expect("non-debug request timed out")
    .expect("non-debug request should complete");
    assert_eq!(non_debug.status(), StatusCode::OK);
    assert_eq!(non_debug.text().await.expect("body should read"), "ok");

    send_pdb_continue(debug_addr).await;

    let (status, body) = timeout(Duration::from_secs(2), debug_request.join())
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn debug_session_is_removed_after_debugger_disconnects() {
    timeout(Duration::from_secs(10), async {
    let (data_addr, admin_addr, data_handle, admin_handle) = spawn_proxy(1024 * 1024).await;
    let client = reqwest::Client::new();

    let program = build_short_circuit_program("ok", None);
    let upload = upload_program(&client, admin_addr, &program).await;
    assert_eq!(upload.status(), StatusCode::NO_CONTENT);

    let debug_addr = reserve_tcp_addr();
    let start_response = client
        .put(format!("http://{admin_addr}/debug/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"header_name":"x-debug","header_value":"on","tcp_addr":"{debug_addr}","stop_on_entry":true}}"#
        ))
        .send()
        .await
        .expect("start debug request should complete");
    assert_eq!(start_response.status(), StatusCode::CREATED);

    let debug_client = client.clone();
    let debug_request = tokio::spawn(async move {
        let response = debug_client
            .get(format!("http://{data_addr}/debug-target"))
            .header("x-debug", "on")
            .send()
            .await
            .expect("debug request should complete");
        let status = response.status();
        let body = response.text().await.expect("body should read");
        (status, body)
    });

    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(debug_addr).expect("debugger tcp should accept");
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let mut buffer = [0u8; 256];
        let _ = stream.read(&mut buffer);
    })
    .await
    .expect("debugger helper should not panic");

    let (status, body) = timeout(Duration::from_secs(2), debug_request)
        .await
        .expect("debug request timed out")
        .expect("debug task should join");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let mut inactive = false;
    for _ in 0..20 {
        let response = client
            .get(format!("http://{admin_addr}/debug/session"))
            .send()
            .await
            .expect("status request should complete");
        let body = response.text().await.expect("status body should read");
        if body.contains(r#""active":false"#) {
            inactive = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(inactive, "debug session should be cleaned up after detach");

    data_handle.abort();
    admin_handle.abort();
    })
    .await
    .expect("test timed out");
}
