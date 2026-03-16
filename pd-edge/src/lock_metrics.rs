use std::{
    env,
    ops::{Deref, DerefMut},
    sync::{
        Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum LockMetricKey {
    VmRequestHead = 0,
    VmDownstream = 1,
    VmExchanges = 2,
    VmTransport = 3,
    VmProxy = 4,
    VmEdgeIo = 5,
    VmAsyncOps = 6,
    UpstreamClientCache = 7,
    TlsSessionCache = 8,
    RateLimiter = 9,
    Http2UpstreamSessionStore = 10,
    Http2UpstreamSessionDag = 11,
    Http2DownstreamSessionStore = 12,
    Http3UpstreamSessionStore = 13,
    Http3UpstreamSessionDag = 14,
}

impl LockMetricKey {
    pub(crate) const COUNT: usize = 15;

    const ALL: [Self; Self::COUNT] = [
        Self::VmRequestHead,
        Self::VmDownstream,
        Self::VmExchanges,
        Self::VmTransport,
        Self::VmProxy,
        Self::VmEdgeIo,
        Self::VmAsyncOps,
        Self::UpstreamClientCache,
        Self::TlsSessionCache,
        Self::RateLimiter,
        Self::Http2UpstreamSessionStore,
        Self::Http2UpstreamSessionDag,
        Self::Http2DownstreamSessionStore,
        Self::Http3UpstreamSessionStore,
        Self::Http3UpstreamSessionDag,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::VmRequestHead => "vm_request_head",
            Self::VmDownstream => "vm_downstream",
            Self::VmExchanges => "vm_exchanges",
            Self::VmTransport => "vm_transport",
            Self::VmProxy => "vm_proxy",
            Self::VmEdgeIo => "vm_edge_io",
            Self::VmAsyncOps => "vm_async_ops",
            Self::UpstreamClientCache => "upstream_client_cache",
            Self::TlsSessionCache => "tls_session_cache",
            Self::RateLimiter => "rate_limiter",
            Self::Http2UpstreamSessionStore => "http2_upstream_session_store",
            Self::Http2UpstreamSessionDag => "http2_upstream_session_dag",
            Self::Http2DownstreamSessionStore => "http2_downstream_session_store",
            Self::Http3UpstreamSessionStore => "http3_upstream_session_store",
            Self::Http3UpstreamSessionDag => "http3_upstream_session_dag",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LockMetricSnapshot {
    pub name: String,
    pub acquisitions_total: u64,
    pub wait_ns_total: u64,
    pub hold_ns_total: u64,
    pub wait_ns_max: u64,
    pub hold_ns_max: u64,
}

struct LockMetricCounter {
    acquisitions_total: AtomicU64,
    wait_ns_total: AtomicU64,
    hold_ns_total: AtomicU64,
    wait_ns_max: AtomicU64,
    hold_ns_max: AtomicU64,
}

impl LockMetricCounter {
    const fn new() -> Self {
        Self {
            acquisitions_total: AtomicU64::new(0),
            wait_ns_total: AtomicU64::new(0),
            hold_ns_total: AtomicU64::new(0),
            wait_ns_max: AtomicU64::new(0),
            hold_ns_max: AtomicU64::new(0),
        }
    }
}

static LOCK_METRICS_ENABLED: OnceLock<bool> = OnceLock::new();
static LOCK_METRICS_DISABLED: AtomicBool = AtomicBool::new(false);
static LOCK_METRICS: [LockMetricCounter; LockMetricKey::COUNT] = [
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
    LockMetricCounter::new(),
];

pub(crate) struct ProfiledMutexGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    key: LockMetricKey,
    hold_started_at: Option<Instant>,
}

pub(crate) struct ProfiledRwLockReadGuard<'a, T> {
    guard: RwLockReadGuard<'a, T>,
    key: LockMetricKey,
    hold_started_at: Option<Instant>,
}

pub(crate) struct ProfiledRwLockWriteGuard<'a, T> {
    guard: RwLockWriteGuard<'a, T>,
    key: LockMetricKey,
    hold_started_at: Option<Instant>,
}

impl<'a, T> Deref for ProfiledMutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> DerefMut for ProfiledMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<'a, T> Deref for ProfiledRwLockReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> Deref for ProfiledRwLockWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> DerefMut for ProfiledRwLockWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<T> Drop for ProfiledMutexGuard<'_, T> {
    fn drop(&mut self) {
        let Some(hold_started_at) = self.hold_started_at.take() else {
            return;
        };
        let hold_ns = elapsed_ns(hold_started_at);
        let counter = &LOCK_METRICS[self.key as usize];
        counter.hold_ns_total.fetch_add(hold_ns, Ordering::Relaxed);
        update_max(&counter.hold_ns_max, hold_ns);
    }
}

impl<T> Drop for ProfiledRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let Some(hold_started_at) = self.hold_started_at.take() else {
            return;
        };
        let hold_ns = elapsed_ns(hold_started_at);
        let counter = &LOCK_METRICS[self.key as usize];
        counter.hold_ns_total.fetch_add(hold_ns, Ordering::Relaxed);
        update_max(&counter.hold_ns_max, hold_ns);
    }
}

impl<T> Drop for ProfiledRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let Some(hold_started_at) = self.hold_started_at.take() else {
            return;
        };
        let hold_ns = elapsed_ns(hold_started_at);
        let counter = &LOCK_METRICS[self.key as usize];
        counter.hold_ns_total.fetch_add(hold_ns, Ordering::Relaxed);
        update_max(&counter.hold_ns_max, hold_ns);
    }
}

pub fn disable_collection() {
    LOCK_METRICS_DISABLED.store(true, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    if LOCK_METRICS_DISABLED.load(Ordering::Relaxed) {
        return false;
    }

    *LOCK_METRICS_ENABLED.get_or_init(|| {
        env::var("PD_EDGE_LOCK_METRICS")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
    })
}

pub(crate) fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    key: LockMetricKey,
    poison_message: &'static str,
) -> ProfiledMutexGuard<'a, T> {
    if !enabled() {
        return ProfiledMutexGuard {
            guard: mutex.lock().expect(poison_message),
            key,
            hold_started_at: None,
        };
    }

    let wait_started_at = Instant::now();
    let guard = mutex.lock().expect(poison_message);
    let wait_ns = elapsed_ns(wait_started_at);
    let counter = &LOCK_METRICS[key as usize];
    counter.acquisitions_total.fetch_add(1, Ordering::Relaxed);
    counter.wait_ns_total.fetch_add(wait_ns, Ordering::Relaxed);
    update_max(&counter.wait_ns_max, wait_ns);
    ProfiledMutexGuard {
        guard,
        key,
        hold_started_at: Some(Instant::now()),
    }
}

pub(crate) fn read_lock<'a, T>(
    lock: &'a RwLock<T>,
    key: LockMetricKey,
    poison_message: &'static str,
) -> ProfiledRwLockReadGuard<'a, T> {
    if !enabled() {
        return ProfiledRwLockReadGuard {
            guard: lock.read().expect(poison_message),
            key,
            hold_started_at: None,
        };
    }

    let wait_started_at = Instant::now();
    let guard = lock.read().expect(poison_message);
    let wait_ns = elapsed_ns(wait_started_at);
    let counter = &LOCK_METRICS[key as usize];
    counter.acquisitions_total.fetch_add(1, Ordering::Relaxed);
    counter.wait_ns_total.fetch_add(wait_ns, Ordering::Relaxed);
    update_max(&counter.wait_ns_max, wait_ns);
    ProfiledRwLockReadGuard {
        guard,
        key,
        hold_started_at: Some(Instant::now()),
    }
}

pub(crate) fn write_lock<'a, T>(
    lock: &'a RwLock<T>,
    key: LockMetricKey,
    poison_message: &'static str,
) -> ProfiledRwLockWriteGuard<'a, T> {
    if !enabled() {
        return ProfiledRwLockWriteGuard {
            guard: lock.write().expect(poison_message),
            key,
            hold_started_at: None,
        };
    }

    let wait_started_at = Instant::now();
    let guard = lock.write().expect(poison_message);
    let wait_ns = elapsed_ns(wait_started_at);
    let counter = &LOCK_METRICS[key as usize];
    counter.acquisitions_total.fetch_add(1, Ordering::Relaxed);
    counter.wait_ns_total.fetch_add(wait_ns, Ordering::Relaxed);
    update_max(&counter.wait_ns_max, wait_ns);
    ProfiledRwLockWriteGuard {
        guard,
        key,
        hold_started_at: Some(Instant::now()),
    }
}

pub(crate) fn snapshot() -> Vec<LockMetricSnapshot> {
    if !enabled() {
        return vec![];
    }

    let mut snapshots = LockMetricKey::ALL
        .into_iter()
        .map(|key| {
            let counter = &LOCK_METRICS[key as usize];
            LockMetricSnapshot {
                name: key.as_str().to_string(),
                acquisitions_total: counter.acquisitions_total.load(Ordering::Relaxed),
                wait_ns_total: counter.wait_ns_total.load(Ordering::Relaxed),
                hold_ns_total: counter.hold_ns_total.load(Ordering::Relaxed),
                wait_ns_max: counter.wait_ns_max.load(Ordering::Relaxed),
                hold_ns_max: counter.hold_ns_max.load(Ordering::Relaxed),
            }
        })
        .filter(|entry| entry.acquisitions_total > 0)
        .collect::<Vec<_>>();
    snapshots.sort_by(|left, right| {
        right
            .wait_ns_total
            .cmp(&left.wait_ns_total)
            .then_with(|| right.hold_ns_total.cmp(&left.hold_ns_total))
            .then_with(|| right.acquisitions_total.cmp(&left.acquisitions_total))
    });
    snapshots
}

pub(crate) fn metrics_text() -> String {
    let snapshots = snapshot();
    if snapshots.is_empty() {
        return String::new();
    }

    let mut text = String::new();
    for entry in snapshots {
        text.push_str(&format!(
            concat!(
                "pd_proxy_lock_acquisitions_total{{lock=\"{}\"}} {}\n",
                "pd_proxy_lock_wait_ns_total{{lock=\"{}\"}} {}\n",
                "pd_proxy_lock_hold_ns_total{{lock=\"{}\"}} {}\n",
                "pd_proxy_lock_wait_ns_max{{lock=\"{}\"}} {}\n",
                "pd_proxy_lock_hold_ns_max{{lock=\"{}\"}} {}\n"
            ),
            entry.name,
            entry.acquisitions_total,
            entry.name,
            entry.wait_ns_total,
            entry.name,
            entry.hold_ns_total,
            entry.name,
            entry.wait_ns_max,
            entry.name,
            entry.hold_ns_max,
        ));
    }
    text
}

fn elapsed_ns(started_at: Instant) -> u64 {
    started_at.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn update_max(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}
