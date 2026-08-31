//! Bounded, argv-only child-process execution.
//!
//! This module owns the process lifecycle needed by foreground and background
//! callers. It deliberately uses `std::process::Command` with an argv vector;
//! command strings and shell expansion are outside this API.

#[cfg(all(test, unix))]
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(windows)]
use super::windows_process_tree::ProcessJob;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Maximum number of argv entries accepted by [`BoundedProcessRequest`].
pub const MAX_ARG_COUNT: usize = 256;
/// Maximum byte length of one argv entry.
pub const MAX_ARG_ITEM_BYTES: usize = 16 * 1024;
/// Maximum combined byte length of all argv entries.
pub const MAX_ARG_TOTAL_BYTES: usize = 256 * 1024;
/// Maximum number of explicitly supplied environment entries.
pub const MAX_ENV_COUNT: usize = 128;
/// Maximum byte length of an environment key.
pub const MAX_ENV_KEY_BYTES: usize = 256;
/// Maximum byte length of an environment value.
pub const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
/// Maximum combined byte length of environment keys and values.
pub const MAX_ENV_TOTAL_BYTES: usize = 256 * 1024;
/// Maximum initial stdin payload.
pub const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;
/// Maximum one-shot stdin write payload.
pub const MAX_STDIN_WRITE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum accepted timeout or relative deadline.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Maximum retained output across both streams.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-stream and total output limit.
pub const DEFAULT_OUTPUT_BYTES: usize = 1024 * 1024;
/// Polling slice used by cancellable stdin/drainer/wait loops.
const WAIT_SLICE: Duration = Duration::from_millis(5);
/// Bound for joining workers and reaping a killed child after SIGKILL/Job.
const CLEANUP_GRACE: Duration = Duration::from_millis(200);

static NEXT_PROCESS_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Cooperative cancellation shared by a request and its owner.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Creates a clear cancellation token.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests cancellation. Repeated calls are harmless.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// A validated request for a native argv-only child process.
#[derive(Clone)]
pub struct BoundedProcessRequest {
    /// Program followed by its literal arguments.
    pub argv: Vec<String>,
    /// Optional explicit working directory. Must be absolute when present.
    pub cwd: Option<PathBuf>,
    /// Workspace root used as the child cwd when `cwd` is omitted.
    pub workspace_root: Option<PathBuf>,
    /// Explicit environment entries. They are allowlisted; inheritance is
    /// forbidden.
    pub env: BTreeMap<String, String>,
    /// Whether the child inherits the host environment in addition to `env`.
    /// Must remain false; [`ValidationError::InheritEnvForbidden`] rejects it.
    pub inherit_env: bool,
    /// Initial stdin bytes. The foreground helper closes stdin after writing.
    pub stdin: Vec<u8>,
    /// Relative execution timeout. At least one of `timeout` or `deadline` is
    /// required; `new` supplies the bounded default.
    pub timeout: Option<Duration>,
    /// Absolute execution deadline. When both are present, the earlier one is
    /// used.
    pub deadline: Option<Instant>,
    /// Maximum retained stdout bytes.
    pub stdout_limit: usize,
    /// Maximum retained stderr bytes.
    pub stderr_limit: usize,
    /// Maximum retained bytes across stdout and stderr.
    pub total_limit: usize,
    /// Optional owner cancellation token.
    pub cancellation_token: Option<CancellationToken>,
}

impl fmt::Debug for BoundedProcessRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedProcessRequest")
            .field("argv_count", &self.argv.len())
            .field("cwd_present", &self.cwd.is_some())
            .field("workspace_root_present", &self.workspace_root.is_some())
            .field("env_count", &self.env.len())
            .field("inherit_env", &self.inherit_env)
            .field("stdin_len", &self.stdin.len())
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("total_limit", &self.total_limit)
            .field("cancellation_present", &self.cancellation_token.is_some())
            .finish()
    }
}

impl Default for BoundedProcessRequest {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

pub(crate) fn validate_argv(argv: &[String]) -> Result<(), ValidationError> {
    if argv.is_empty() {
        return Err(ValidationError::EmptyArgv);
    }
    if argv.len() > MAX_ARG_COUNT {
        return Err(ValidationError::ArgCountExceeded);
    }
    if argv[0].is_empty() {
        return Err(ValidationError::EmptyProgram);
    }
    let mut argv_total = 0usize;
    for (index, item) in argv.iter().enumerate() {
        if item.as_bytes().contains(&0) {
            return Err(ValidationError::ArgContainsNul { index });
        }
        if item.len() > MAX_ARG_ITEM_BYTES {
            return Err(ValidationError::ArgItemTooLong { index });
        }
        argv_total = argv_total
            .checked_add(item.len())
            .ok_or(ValidationError::ArgTotalTooLarge)?;
        if argv_total > MAX_ARG_TOTAL_BYTES {
            return Err(ValidationError::ArgTotalTooLarge);
        }
    }
    Ok(())
}

impl BoundedProcessRequest {
    /// Creates a request with bounded defaults.
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            workspace_root: None,
            env: BTreeMap::new(),
            inherit_env: false,
            stdin: Vec::new(),
            timeout: Some(DEFAULT_TIMEOUT),
            deadline: None,
            stdout_limit: DEFAULT_OUTPUT_BYTES,
            stderr_limit: DEFAULT_OUTPUT_BYTES,
            total_limit: DEFAULT_OUTPUT_BYTES,
            cancellation_token: None,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_env_map(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_inherit_env(mut self, inherit_env: bool) -> Self {
        self.inherit_env = inherit_env;
        self
    }

    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = stdin.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    pub fn with_output_limits(
        mut self,
        stdout_limit: usize,
        stderr_limit: usize,
        total_limit: usize,
    ) -> Self {
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        self.total_limit = total_limit;
        self
    }

    /// Validates every bounded request field without spawning a process.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_argv(&self.argv)?;

        if let Some(cwd) = self.resolved_cwd() {
            let cwd_len = os_string_len(cwd.as_os_str());
            if cwd_len == 0 {
                return Err(ValidationError::EmptyCwd);
            }
            if cwd_len > MAX_ARG_TOTAL_BYTES {
                return Err(ValidationError::CwdTooLong);
            }
            if os_string_has_nul(cwd.as_os_str()) {
                return Err(ValidationError::CwdContainsNul);
            }
            if !cwd.is_absolute() {
                return Err(ValidationError::CwdNotAbsolute);
            }
        } else {
            return Err(ValidationError::CwdRequired);
        }

        if self.inherit_env {
            return Err(ValidationError::InheritEnvForbidden);
        }

        if self.env.len() > MAX_ENV_COUNT {
            return Err(ValidationError::EnvCountExceeded);
        }
        let mut env_total = 0usize;
        for (key, value) in &self.env {
            if !valid_env_key(key) {
                return Err(ValidationError::InvalidEnvKey);
            }
            if key.len() > MAX_ENV_KEY_BYTES {
                return Err(ValidationError::EnvKeyTooLong);
            }
            if value.as_bytes().contains(&0) {
                return Err(ValidationError::EnvValueContainsNul);
            }
            if value.len() > MAX_ENV_VALUE_BYTES {
                return Err(ValidationError::EnvValueTooLong);
            }
            env_total = env_total
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or(ValidationError::EnvTotalTooLarge)?;
            if env_total > MAX_ENV_TOTAL_BYTES {
                return Err(ValidationError::EnvTotalTooLarge);
            }
        }

        if self.stdin.len() > MAX_STDIN_BYTES {
            return Err(ValidationError::StdinTooLarge);
        }
        match self.timeout {
            Some(timeout) if timeout.is_zero() => return Err(ValidationError::TimeoutNonPositive),
            Some(timeout) if timeout > MAX_TIMEOUT => return Err(ValidationError::TimeoutTooLarge),
            Some(_) | None => {}
        }
        if self.timeout.is_none() && self.deadline.is_none() {
            return Err(ValidationError::TimeoutMissing);
        }
        if let Some(deadline) = self.deadline {
            let now = Instant::now();
            if deadline <= now {
                return Err(ValidationError::DeadlineElapsed);
            }
            if deadline > now + MAX_TIMEOUT {
                return Err(ValidationError::DeadlineTooFar);
            }
        }

        validate_output_limit(self.stdout_limit, "stdout")?;
        validate_output_limit(self.stderr_limit, "stderr")?;
        validate_output_limit(self.total_limit, "total")?;
        Ok(())
    }

    fn effective_deadline(&self, now: Instant) -> Result<Instant, BoundedProcessError> {
        self.validate()
            .map_err(BoundedProcessError::InvalidRequest)?;
        let timeout_deadline = self.timeout.map(|timeout| now + timeout);
        let deadline = match (timeout_deadline, self.deadline) {
            (Some(timeout), Some(deadline)) => timeout.min(deadline),
            (Some(timeout), None) => timeout,
            (None, Some(deadline)) => deadline,
            (None, None) => return Err(BoundedProcessError::DeadlineElapsed),
        };
        if deadline <= now {
            return Err(BoundedProcessError::DeadlineElapsed);
        }
        Ok(deadline)
    }

    fn resolved_cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref().or(self.workspace_root.as_ref())
    }
}

fn validate_output_limit(value: usize, name: &'static str) -> Result<(), ValidationError> {
    if value == 0 {
        return Err(ValidationError::OutputLimitNonPositive { name });
    }
    if value > MAX_OUTPUT_BYTES {
        return Err(ValidationError::OutputLimitTooLarge { name });
    }
    Ok(())
}

fn valid_env_key(value: &str) -> bool {
    let mut chars = value.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(unix)]
fn os_string_len(value: &std::ffi::OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().len()
}

#[cfg(windows)]
fn os_string_len(value: &std::ffi::OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().count().saturating_mul(2)
}

#[cfg(not(any(unix, windows)))]
fn os_string_len(value: &std::ffi::OsStr) -> usize {
    value.to_string_lossy().len()
}

#[cfg(unix)]
fn os_string_has_nul(value: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}

#[cfg(windows)]
fn os_string_has_nul(value: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().any(|unit| unit == 0)
}

#[cfg(not(any(unix, windows)))]
fn os_string_has_nul(value: &std::ffi::OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

/// Validation failures do not retain user-supplied argv, environment, or stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    EmptyArgv,
    EmptyProgram,
    ArgCountExceeded,
    ArgContainsNul { index: usize },
    ArgItemTooLong { index: usize },
    ArgTotalTooLarge,
    EmptyCwd,
    CwdRequired,
    CwdNotAbsolute,
    CwdTooLong,
    CwdContainsNul,
    EnvCountExceeded,
    InvalidEnvKey,
    EnvKeyTooLong,
    EnvValueContainsNul,
    EnvValueTooLong,
    EnvTotalTooLarge,
    InheritEnvForbidden,
    StdinTooLarge,
    TimeoutMissing,
    TimeoutNonPositive,
    TimeoutTooLarge,
    DeadlineElapsed,
    DeadlineTooFar,
    OutputLimitNonPositive { name: &'static str },
    OutputLimitTooLarge { name: &'static str },
}

pub type ProcessValidationError = ValidationError;

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::EmptyArgv => "argv must contain a program",
            Self::EmptyProgram => "program must not be empty",
            Self::ArgCountExceeded => "argv count exceeds the configured bound",
            Self::ArgContainsNul { .. } => "argv contains a NUL byte",
            Self::ArgItemTooLong { .. } => "argv item exceeds the configured bound",
            Self::ArgTotalTooLarge => "argv total length exceeds the configured bound",
            Self::EmptyCwd => "cwd must not be empty",
            Self::CwdRequired => "an explicit cwd or workspace root is required",
            Self::CwdNotAbsolute => "cwd must be an absolute path",
            Self::CwdTooLong => "cwd exceeds the configured bound",
            Self::CwdContainsNul => "cwd contains a NUL byte",
            Self::EnvCountExceeded => "environment entry count exceeds the configured bound",
            Self::InvalidEnvKey => "environment key has invalid grammar",
            Self::EnvKeyTooLong => "environment key exceeds the configured bound",
            Self::EnvValueContainsNul => "environment value contains a NUL byte",
            Self::EnvValueTooLong => "environment value exceeds the configured bound",
            Self::EnvTotalTooLarge => "environment total length exceeds the configured bound",
            Self::InheritEnvForbidden => "inheriting the ambient environment is forbidden",
            Self::StdinTooLarge => "stdin exceeds the configured bound",
            Self::TimeoutMissing => "a timeout or absolute deadline is required",
            Self::TimeoutNonPositive => "timeout must be positive",
            Self::TimeoutTooLarge => "timeout exceeds the configured bound",
            Self::DeadlineElapsed => "absolute deadline has elapsed",
            Self::DeadlineTooFar => "absolute deadline exceeds the configured bound",
            Self::OutputLimitNonPositive { name } => {
                return write!(formatter, "{name} output limit must be positive");
            }
            Self::OutputLimitTooLarge { name } => {
                return write!(
                    formatter,
                    "{name} output limit exceeds the configured bound"
                );
            }
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for ValidationError {}

/// Coarse spawn failure classification with an optional OS error number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnErrorKind {
    NotFound,
    PermissionDenied,
    InvalidInput,
    ResourceExhausted,
    Other,
}

/// Bounded spawn failure. It never contains the attempted argv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnError {
    pub kind: SpawnErrorKind,
    pub os_code: Option<i32>,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "spawn failed ({:?}", self.kind)?;
        if let Some(code) = self.os_code {
            write!(formatter, ", os error {code}")?;
        }
        formatter.write_str(")")
    }
}

impl std::error::Error for SpawnError {}

/// Process lifecycle failures. Error variants retain only bounded metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundedProcessError {
    InvalidRequest(ValidationError),
    Spawn(SpawnError),
    DeadlineElapsed,
    Cancelled,
    WaitFailed { os_code: Option<i32> },
    StdinClosed,
    StdinTooLarge,
    StdinWriteFailed { os_code: Option<i32> },
    DrainFailed { stream: LogStream },
    WorkerFailed,
}

impl fmt::Display for BoundedProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(error) => write!(formatter, "invalid process request: {error}"),
            Self::Spawn(error) => error.fmt(formatter),
            Self::DeadlineElapsed => formatter.write_str("process deadline elapsed"),
            Self::Cancelled => formatter.write_str("process was cancelled"),
            Self::WaitFailed { os_code } => write_os_error(formatter, "wait failed", *os_code),
            Self::StdinClosed => formatter.write_str("process stdin is closed"),
            Self::StdinTooLarge => formatter.write_str("stdin write exceeds the configured bound"),
            Self::StdinWriteFailed { os_code } => {
                write_os_error(formatter, "stdin write failed", *os_code)
            }
            Self::DrainFailed { stream } => write!(formatter, "{} drain failed", stream.name()),
            Self::WorkerFailed => formatter.write_str("process worker failed"),
        }
    }
}

impl std::error::Error for BoundedProcessError {}

fn write_os_error(
    formatter: &mut fmt::Formatter<'_>,
    label: &str,
    os_code: Option<i32>,
) -> fmt::Result {
    write!(formatter, "{label}")?;
    if let Some(code) = os_code {
        write!(formatter, " (os error {code})")?;
    }
    Ok(())
}

fn spawn_error(error: &std::io::Error) -> SpawnError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => SpawnErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => SpawnErrorKind::PermissionDenied,
        std::io::ErrorKind::InvalidInput => SpawnErrorKind::InvalidInput,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ResourceBusy => {
            SpawnErrorKind::ResourceExhausted
        }
        _ => SpawnErrorKind::Other,
    };
    SpawnError {
        kind,
        os_code: error.raw_os_error(),
    }
}

/// Terminal status retained by a [`BoundedProcess`] after reaping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    Exited { code: Option<i32> },
    Signaled { signal: i32 },
    Unknown,
}

impl ProcessStatus {
    fn from_exit_status(status: ExitStatus) -> Self {
        #[cfg(unix)]
        if let Some(signal) = status.signal() {
            return Self::Signaled { signal };
        }
        Self::Exited {
            code: status.code(),
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Exited { code: Some(0) })
    }

    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited { code } => code,
            Self::Signaled { .. } | Self::Unknown => None,
        }
    }

    pub fn signal(self) -> Option<i32> {
        match self {
            Self::Signaled { signal } => Some(signal),
            Self::Exited { .. } | Self::Unknown => None,
        }
    }
}

/// Which bounded output stream a log snapshot describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// A bounded byte snapshot with offsets into the complete stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSnapshot {
    /// Retained bytes, at most the stream and total limits.
    pub bytes: Vec<u8>,
    /// Absolute offset of the first retained byte.
    pub offset: u64,
    /// Absolute offset immediately after all bytes observed so far.
    pub next_offset: u64,
    /// Whether any bytes from this stream were discarded.
    pub truncated: bool,
    /// Whether a requested offset preceded the retained range.
    pub gap: bool,
    /// Whether the reader reached EOF.
    pub eof: bool,
}

impl LogSnapshot {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn data(&self) -> &[u8] {
        self.as_bytes()
    }

    pub fn start_offset(&self) -> u64 {
        self.offset
    }

    pub fn end_offset(&self) -> u64 {
        self.next_offset
    }
}

struct RingLog {
    limit: usize,
    bytes: VecDeque<u8>,
    next_offset: u64,
    truncated: bool,
    eof: bool,
    oldest_seq: u64,
}

impl RingLog {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: VecDeque::with_capacity(limit.min(8192)),
            next_offset: 0,
            truncated: false,
            eof: false,
            oldest_seq: 0,
        }
    }

    fn append(&mut self, bytes: &[u8], seq_start: u64) {
        let added = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.next_offset = self.next_offset.saturating_add(added);
        if self.bytes.is_empty() {
            self.oldest_seq = seq_start;
        }
        self.bytes.extend(bytes.iter().copied());
        while self.bytes.len() > self.limit {
            self.evict_one();
        }
    }

    fn evict_one(&mut self) -> bool {
        let removed = self.bytes.pop_front().is_some();
        if removed {
            self.truncated = true;
            self.oldest_seq = self.oldest_seq.saturating_add(1);
        }
        removed
    }

    fn start_offset(&self) -> u64 {
        self.next_offset
            .saturating_sub(u64::try_from(self.bytes.len()).unwrap_or(u64::MAX))
    }

    fn snapshot_from(&self, requested_offset: Option<u64>) -> LogSnapshot {
        let start = self.start_offset();
        let gap = requested_offset.is_some_and(|offset| offset < start);
        let copy_from = requested_offset
            .unwrap_or(start)
            .max(start)
            .min(self.next_offset);
        let skip = usize::try_from(copy_from.saturating_sub(start)).unwrap_or(self.bytes.len());
        let bytes = self.bytes.iter().skip(skip).copied().collect();
        LogSnapshot {
            bytes,
            offset: copy_from,
            next_offset: self.next_offset,
            truncated: self.truncated,
            gap,
            eof: self.eof,
        }
    }
}

struct LogStore {
    stdout: RingLog,
    stderr: RingLog,
    total_limit: usize,
    stdout_error: bool,
    stderr_error: bool,
    next_seq: u64,
}

impl LogStore {
    fn new(stdout_limit: usize, stderr_limit: usize, total_limit: usize) -> Self {
        Self {
            stdout: RingLog::new(stdout_limit),
            stderr: RingLog::new(stderr_limit),
            total_limit,
            stdout_error: false,
            stderr_error: false,
            next_seq: 0,
        }
    }

    fn append(&mut self, stream: LogStream, bytes: &[u8]) {
        let seq_start = self.next_seq;
        self.next_seq = self
            .next_seq
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
        match stream {
            LogStream::Stdout => self.stdout.append(bytes, seq_start),
            LogStream::Stderr => self.stderr.append(bytes, seq_start),
        }
        while self.stdout.bytes.len() + self.stderr.bytes.len() > self.total_limit {
            let evict_stdout = match (self.stdout.bytes.front(), self.stderr.bytes.front()) {
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(_), Some(_)) => self.stdout.oldest_seq <= self.stderr.oldest_seq,
                (None, None) => break,
            };
            if evict_stdout {
                self.stdout.evict_one();
            } else {
                self.stderr.evict_one();
            }
        }
    }

    fn mark_eof(&mut self, stream: LogStream) {
        match stream {
            LogStream::Stdout => self.stdout.eof = true,
            LogStream::Stderr => self.stderr.eof = true,
        }
    }

    fn mark_error(&mut self, stream: LogStream) {
        match stream {
            LogStream::Stdout => self.stdout_error = true,
            LogStream::Stderr => self.stderr_error = true,
        }
    }

    fn snapshot(&self, stream: LogStream, requested_offset: Option<u64>) -> LogSnapshot {
        match stream {
            LogStream::Stdout => self.stdout.snapshot_from(requested_offset),
            LogStream::Stderr => self.stderr.snapshot_from(requested_offset),
        }
    }

    fn has_error(&self, stream: LogStream) -> bool {
        match stream {
            LogStream::Stdout => self.stdout_error,
            LogStream::Stderr => self.stderr_error,
        }
    }
}

fn retryable_write(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}

struct DrainFinish(Arc<DrainDone>);

impl Drop for DrainFinish {
    fn drop(&mut self) {
        self.0.signal();
    }
}

struct StdinState {
    writer: InterruptibleWriter<StdinPipe>,
    #[cfg(windows)]
    windows_write: super::windows_stdio::CancellableWrite,
    closed: AtomicBool,
    initial_payload: bool,
    close_after_initial: bool,
    initial_writer: Mutex<Option<JoinHandle<()>>>,
    initial_error: Mutex<Option<BoundedProcessError>>,
    cancellation: CancellationToken,
    deadline: Instant,
    done: Arc<DrainDone>,
}

#[cfg(not(windows))]
type StdinPipe = std::process::ChildStdin;
#[cfg(windows)]
type StdinPipe = super::windows_stdio::CancellableWrite;

impl StdinState {
    fn new(
        writer: std::process::ChildStdin,
        initial: Vec<u8>,
        close_after_initial: bool,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<Arc<Self>, std::io::Error> {
        #[cfg(unix)]
        super::shared::set_pipe_nonblocking(&writer)?;
        #[cfg(windows)]
        let windows_write = super::windows_stdio::CancellableWrite::from_stdin(writer);
        #[cfg(windows)]
        let writer = windows_write.clone();
        #[cfg(not(windows))]
        let writer = writer;
        let initial_payload = !initial.is_empty();
        let done = DrainDone::new();
        let state = Arc::new(Self {
            writer: InterruptibleWriter::new(writer),
            #[cfg(windows)]
            windows_write,
            closed: AtomicBool::new(false),
            initial_payload,
            close_after_initial,
            initial_writer: Mutex::new(None),
            initial_error: Mutex::new(None),
            cancellation,
            deadline,
            done,
        });
        if !initial.is_empty() {
            let worker_state = Arc::clone(&state);
            let handle = thread::Builder::new()
                .name("bounded-process-stdin".to_owned())
                .spawn(move || {
                    let _done = DrainFinish(Arc::clone(&worker_state.done));
                    let result = worker_state.write_payload(&initial);
                    if worker_state.close_after_initial {
                        worker_state.force_close_writer();
                    }
                    if let Err(error) = result
                        && !matches!(error, BoundedProcessError::StdinClosed)
                    {
                        *worker_state
                            .initial_error
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(error);
                    }
                })?;
            *state
                .initial_writer
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(handle);
        } else {
            state.done.signal();
        }
        Ok(state)
    }

    fn force_close_writer(&self) {
        self.writer.close();
        #[cfg(windows)]
        self.windows_write.close();
    }

    fn has_initial_payload(&self) -> bool {
        self.initial_payload
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, BoundedProcessError> {
        if bytes.len() > MAX_STDIN_WRITE_BYTES {
            return Err(BoundedProcessError::StdinTooLarge);
        }
        self.write_payload(bytes)?;
        Ok(bytes.len())
    }

    fn write_payload(&self, bytes: &[u8]) -> Result<(), BoundedProcessError> {
        let mut written = 0;
        while written < bytes.len() {
            if self.closed.load(Ordering::Acquire) {
                return Err(BoundedProcessError::StdinClosed);
            }
            if self.cancellation.is_cancelled() {
                return Err(BoundedProcessError::Cancelled);
            }
            let now = Instant::now();
            if now >= self.deadline {
                return Err(BoundedProcessError::DeadlineElapsed);
            }
            let result = self.writer.write_bytes(&bytes[written..]);
            match result {
                Ok(0) => {
                    return Err(BoundedProcessError::StdinWriteFailed { os_code: None });
                }
                Ok(count) => written += count,
                Err(error) if retryable_write(&error) => {
                    thread::sleep(self.deadline.saturating_duration_since(now).min(WAIT_SLICE));
                }
                Err(error) => {
                    return Err(BoundedProcessError::StdinWriteFailed {
                        os_code: error.raw_os_error(),
                    });
                }
            }
        }
        Ok(())
    }

    fn close(&self) -> Result<(), BoundedProcessError> {
        self.closed.store(true, Ordering::Release);
        self.force_close_writer();
        self.done.wait_until(Instant::now() + CLEANUP_GRACE);
        let _ = self
            .initial_writer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(error) = self
            .initial_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for StdinState {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.force_close_writer();
        self.done.wait_until(Instant::now() + CLEANUP_GRACE);
        let _ = self
            .initial_writer
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

#[cfg(target_os = "linux")]
mod linux_pidfd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::ProcessStatus;

    pub(super) struct PidFd {
        fd: OwnedFd,
    }

    impl PidFd {
        pub(super) fn open(pid: u32) -> std::io::Result<Self> {
            let pid = libc::pid_t::try_from(pid).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid out of range")
            })?;
            // SAFETY: `pid` is a kernel pid we just spawned or observed; flags 0
            // request a new pidfd. On success the syscall returns a fresh fd
            // exclusively owned by the caller.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = i32::try_from(fd).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "pidfd out of range")
            })?;
            Ok(Self {
                // SAFETY: `fd` is a newly opened pidfd; from_raw_fd takes exclusive
                // ownership and will close it on Drop.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        }

        pub(super) fn send_signal(&self, signal: libc::c_int) -> std::io::Result<()> {
            // SAFETY: `self.fd` is a live pidfd we exclusively own. A null
            // siginfo with flags 0 sends `signal` to that process.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::c_void>(),
                    0,
                )
            };
            if rc < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn observe_exit_wnowait(&self) -> std::io::Result<Option<ProcessStatus>> {
            super::waitid_wnowait(libc::P_PIDFD, self.fd.as_raw_fd() as libc::id_t)
        }
    }
}

#[cfg(unix)]
fn waitid_wnowait(
    idtype: libc::idtype_t,
    id: libc::id_t,
) -> std::io::Result<Option<ProcessStatus>> {
    // SAFETY: siginfo_t is a C union POD; zeroing is the documented
    // pre-waitid initialization so kernel-filled fields can be distinguished.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a writable zeroed siginfo. `idtype`/`id` name a live
    // pid or pidfd. WNOHANG|WNOWAIT observe without reaping.
    let rc = unsafe {
        libc::waitid(
            idtype,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded. si_pid is defined for WEXITED results; 0 means
    // no child changed state.
    let pid = unsafe { info.si_pid() };
    if pid <= 0 {
        return Ok(None);
    }
    // SAFETY: si_pid > 0 so the kernel filled CLD_* status; si_status is the
    // exit code or signal.
    let status = unsafe { info.si_status() };
    Ok(Some(match info.si_code {
        libc::CLD_EXITED => ProcessStatus::Exited { code: Some(status) },
        libc::CLD_KILLED | libc::CLD_DUMPED => ProcessStatus::Signaled { signal: status },
        _ => ProcessStatus::Unknown,
    }))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn observe_pid_wnowait(pid: u32) -> std::io::Result<Option<ProcessStatus>> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid out of range"))?;
    waitid_wnowait(libc::P_PID, pid as libc::id_t)
}

#[cfg(windows)]
fn observe_handle_without_reaping(child: &Child) -> std::io::Result<Option<ProcessStatus>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let handle = child.as_raw_handle() as HANDLE;
    let waited = unsafe { WaitForSingleObject(handle, 0) };
    match waited {
        WAIT_OBJECT_0 => {
            let mut code = 0u32;
            if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // WAIT_OBJECT_0 means the process is signaled/exited. 259 is a
            // valid exit code (STILL_ACTIVE is only meaningful before wait).
            Ok(Some(ProcessStatus::Exited {
                code: Some(code as i32),
            }))
        }
        WAIT_TIMEOUT => Ok(None),
        _ => Err(std::io::Error::last_os_error()),
    }
}

fn retryable_read(error: &std::io::Error) -> bool {
    retryable_write(error)
}

struct DrainDone {
    completed: AtomicBool,
    lock: Mutex<bool>,
    cv: Condvar,
}

impl DrainDone {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            completed: AtomicBool::new(false),
            lock: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn signal(&self) {
        self.completed.store(true, Ordering::Release);
        let mut done = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        *done = true;
        self.cv.notify_all();
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        if self.is_completed() {
            return true;
        }
        let mut done = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*done {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, timed) = self
                .cv
                .wait_timeout(done, deadline.saturating_duration_since(now))
                .unwrap_or_else(|error| error.into_inner());
            done = guard;
            if timed.timed_out() && Instant::now() >= deadline {
                return *done;
            }
        }
        true
    }
}

struct InterruptibleReader<R> {
    inner: Mutex<Option<R>>,
    closed: AtomicBool,
}

impl<R: Read> InterruptibleReader<R> {
    fn new(reader: R) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(reader)),
            closed: AtomicBool::new(false),
        })
    }

    fn take_io(&self) -> Option<R> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn put_io(&self, io: R) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        *guard = Some(io);
    }

    fn read_bytes(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some(mut reader) = self.take_io() else {
            return Ok(0);
        };
        let result = reader.read(buffer);
        self.put_io(reader);
        result
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

struct InterruptibleWriter<W> {
    inner: Mutex<Option<W>>,
    closed: AtomicBool,
}

impl<W: Write> InterruptibleWriter<W> {
    fn new(writer: W) -> Self {
        Self {
            inner: Mutex::new(Some(writer)),
            closed: AtomicBool::new(false),
        }
    }

    fn take_io(&self) -> Option<W> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn put_io(&self, io: W) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        *guard = Some(io);
    }

    fn write_bytes(&self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(mut writer) = self.take_io() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin is closed",
            ));
        };
        let result = writer.write(buffer);
        self.put_io(writer);
        result
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

struct StreamDrainer {
    join: Option<JoinHandle<()>>,
    done: Arc<DrainDone>,
    closer: Arc<dyn Fn() + Send + Sync>,
}

impl StreamDrainer {
    fn close(&self) {
        (self.closer)();
    }
}

fn spawn_drainer<R: Read + Send + 'static>(
    name: &'static str,
    reader: R,
    stream: LogStream,
    logs: Arc<Mutex<LogStore>>,
    stop: Arc<AtomicBool>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<StreamDrainer, std::io::Error> {
    let reader = InterruptibleReader::new(reader);
    let done = DrainDone::new();
    let thread_reader = Arc::clone(&reader);
    let thread_done = Arc::clone(&done);
    let join = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            let finish = |error: bool| {
                let mut logs = logs.lock().unwrap_or_else(|error| error.into_inner());
                if error {
                    logs.mark_error(stream);
                }
                logs.mark_eof(stream);
            };
            let mut stop_deadline = None;
            loop {
                if stop_deadline.is_none()
                    && (stop.load(Ordering::Acquire)
                        || cancellation.is_cancelled()
                        || Instant::now() >= deadline)
                {
                    stop_deadline = Some(Instant::now() + CLEANUP_GRACE);
                }
                if let Some(stop_deadline) = stop_deadline
                    && Instant::now() >= stop_deadline
                {
                    finish(false);
                    thread_done.signal();
                    return;
                }
                match thread_reader.read_bytes(&mut buffer) {
                    Ok(0) => {
                        finish(false);
                        thread_done.signal();
                        return;
                    }
                    Ok(read) => {
                        logs.lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .append(stream, &buffer[..read]);
                    }
                    Err(error) if retryable_read(&error) => {
                        if stop_deadline.is_some() {
                            finish(false);
                            thread_done.signal();
                            return;
                        }
                        thread::sleep(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(WAIT_SLICE),
                        );
                    }
                    Err(_) => {
                        finish(true);
                        thread_done.signal();
                        return;
                    }
                }
            }
        })?;
    Ok(StreamDrainer {
        join: Some(join),
        done,
        closer: Arc::new(move || reader.close()),
    })
}

fn join_stream_drainers(drainers: &mut [StreamDrainer]) -> Result<(), BoundedProcessError> {
    let wait_deadline = Instant::now() + CLEANUP_GRACE;
    let mut result = Ok(());
    for drainer in drainers.iter_mut() {
        if !drainer.done.wait_until(wait_deadline) {
            drainer.close();
            if !drainer.done.wait_until(Instant::now() + CLEANUP_GRACE) && result.is_ok() {
                result = Err(BoundedProcessError::WorkerFailed);
                continue;
            }
        }
        if drainer.done.is_completed()
            && let Some(join) = drainer.join.take()
            && join.join().is_err()
            && result.is_ok()
        {
            result = Err(BoundedProcessError::WorkerFailed);
        }
    }
    result
}

fn wait_child_until(
    child: &mut Child,
    deadline: Instant,
) -> Result<ProcessStatus, BoundedProcessError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(ProcessStatus::from_exit_status(status)),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(BoundedProcessError::DeadlineElapsed);
                }
                thread::sleep(deadline.saturating_duration_since(now).min(WAIT_SLICE));
            }
            Err(error) => {
                return Err(BoundedProcessError::WaitFailed {
                    os_code: error.raw_os_error(),
                });
            }
        }
    }
}

fn abort_spawned_child(mut child: Child, pid: u32) {
    terminate_process_tree(pid);
    let _ = child.kill();
    match wait_child_until(&mut child, Instant::now() + CLEANUP_GRACE) {
        Ok(_) => {}
        Err(_) => std::mem::forget(child),
    }
}

fn allocate_process_handle() -> ProcessHandle {
    loop {
        let id = NEXT_PROCESS_HANDLE.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return ProcessHandle { id, generation: id };
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeCleanup {
    Pending,
    Cleaning,
    Cleaned,
}

struct ChildState {
    child: Option<Child>,
    terminal: Option<ProcessStatus>,
    reaping: bool,
    tree_cleanup: TreeCleanup,
}

#[cfg(all(test, unix))]
#[derive(Default)]
struct TreeCleanupTestHooks {
    before_group_kill: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_wait_for_cleanup: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_reap: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    last_tree_kill_retained_zombie: AtomicBool,
}

struct ProcessInner {
    handle: ProcessHandle,
    pid: u32,
    #[cfg(windows)]
    job: Option<ProcessJob>,
    #[cfg(target_os = "linux")]
    pidfd: Option<linux_pidfd::PidFd>,
    deadline: Instant,
    child: Mutex<ChildState>,
    child_wake: Condvar,
    logs: Arc<Mutex<LogStore>>,
    stdin: Arc<StdinState>,
    drainers: Mutex<Vec<StreamDrainer>>,
    drainers_result: OnceLock<Result<(), BoundedProcessError>>,
    drain_stop: Arc<AtomicBool>,
    cancellation: CancellationToken,
    #[cfg(all(test, unix))]
    test_hooks: TreeCleanupTestHooks,
}

impl ProcessInner {
    fn map_wait_error(error: std::io::Error) -> BoundedProcessError {
        BoundedProcessError::WaitFailed {
            os_code: error.raw_os_error(),
        }
    }

    fn observe_root_exit_locked(
        &self,
        child: Option<&Child>,
    ) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        #[cfg(target_os = "linux")]
        {
            let _ = child;
            let pidfd = self
                .pidfd
                .as_ref()
                .ok_or(BoundedProcessError::WaitFailed { os_code: None })?;
            pidfd.observe_exit_wnowait().map_err(Self::map_wait_error)
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let _ = child;
            observe_pid_wnowait(self.pid).map_err(Self::map_wait_error)
        }
        #[cfg(windows)]
        {
            let child = child.ok_or(BoundedProcessError::WaitFailed { os_code: None })?;
            observe_handle_without_reaping(child).map_err(Self::map_wait_error)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(None)
        }
    }

    fn lock_child(&self) -> MutexGuard<'_, ChildState> {
        self.child.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn wait_while_cleaning<'a>(
        &'a self,
        mut state: MutexGuard<'a, ChildState>,
    ) -> MutexGuard<'a, ChildState> {
        if state.tree_cleanup == TreeCleanup::Cleaning {
            #[cfg(all(test, unix))]
            self.run_on_wait_for_cleanup_hook();
        }
        while state.tree_cleanup == TreeCleanup::Cleaning {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (guard, wait_result) = self
                .child_wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = guard;
            if wait_result.timed_out() && Instant::now() >= self.deadline {
                break;
            }
        }
        state
    }

    fn cleanup_tree_once(&self) {
        {
            let mut state = self.lock_child();
            loop {
                match state.tree_cleanup {
                    TreeCleanup::Cleaned => return,
                    TreeCleanup::Cleaning => {
                        state = self.wait_while_cleaning(state);
                        if state.tree_cleanup != TreeCleanup::Cleaned {
                            return;
                        }
                    }
                    TreeCleanup::Pending => {
                        state.tree_cleanup = TreeCleanup::Cleaning;
                        break;
                    }
                }
            }
        }
        self.drain_stop.store(true, Ordering::Release);
        #[cfg(all(test, unix))]
        self.run_before_group_kill_hook();
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = &self.pidfd {
            let _ = pidfd.send_signal(libc::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(unix)]
        {
            #[cfg(test)]
            {
                record_tree_kill_identity(self.pid);
                self.test_hooks
                    .last_tree_kill_retained_zombie
                    .store(proc_is_our_zombie(self.pid), Ordering::Release);
            }
            super::shared::terminate_process_group(self.pid);
        }
        let mut state = self.lock_child();
        if let Some(child) = state.child.as_mut() {
            let _ = child.kill();
        }
        state.tree_cleanup = TreeCleanup::Cleaned;
        self.child_wake.notify_all();
    }

    fn harvest_if_exited(&self) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        {
            let state = self.lock_child();
            if let Some(status) = state.terminal {
                return Ok(Some(status));
            }
            if state.reaping {
                return Ok(None);
            }
            if self
                .observe_root_exit_locked(state.child.as_ref())?
                .is_none()
            {
                return Ok(None);
            }
        }
        self.cleanup_tree_once();
        let mut state = self.lock_child();
        if let Some(status) = state.terminal {
            return Ok(Some(status));
        }
        if state.reaping {
            return Ok(None);
        }
        state = self.wait_while_cleaning(state);
        if state.tree_cleanup != TreeCleanup::Cleaned {
            return Ok(None);
        }
        if let Some(status) = state.terminal {
            return Ok(Some(status));
        }
        if state.reaping {
            return Ok(None);
        }
        #[cfg(all(test, unix))]
        self.run_on_reap_hook();
        let reaped = match state.child.as_mut() {
            Some(child) => child.try_wait().map_err(Self::map_wait_error)?,
            None => return Ok(state.terminal),
        };
        let Some(status) = reaped.map(ProcessStatus::from_exit_status) else {
            return Ok(None);
        };
        state.child.take();
        state.terminal = Some(status);
        self.child_wake.notify_all();
        Ok(Some(status))
    }

    fn try_wait(&self) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        let status = self.harvest_if_exited()?;
        if status.is_some() {
            let _ = self.join_drainers();
        }
        Ok(status)
    }

    fn poll(&self) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        if self.cancellation.is_cancelled() {
            self.kill_process_tree()?;
            let _ = self.reap();
            return Err(BoundedProcessError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            self.kill_process_tree()?;
            let _ = self.reap();
            return Err(BoundedProcessError::DeadlineElapsed);
        }
        if let Some(status) = self.try_wait()? {
            return Ok(Some(status));
        }
        Ok(None)
    }

    fn wait_until_with_hook<F>(
        &self,
        deadline: Instant,
        is_cancelled: F,
    ) -> Result<ProcessStatus, BoundedProcessError>
    where
        F: Fn() -> bool,
    {
        loop {
            if self.cancellation.is_cancelled() || is_cancelled() {
                self.kill_process_tree()?;
                let _ = self.reap();
                return Err(BoundedProcessError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                self.kill_process_tree()?;
                let _ = self.reap();
                return Err(BoundedProcessError::DeadlineElapsed);
            }
            if let Some(status) = self.try_wait()? {
                self.reap()?;
                return Ok(status);
            }
            thread::sleep(deadline.saturating_duration_since(now).min(WAIT_SLICE));
        }
    }

    fn reap(&self) -> Result<ProcessStatus, BoundedProcessError> {
        self.cleanup_tree_once();
        let status = loop {
            {
                let state = self.child.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(status) = state.terminal {
                    break status;
                }
                if state.reaping {
                    let remaining = self.deadline.saturating_duration_since(Instant::now());
                    let (guard, wait_result) = self
                        .child_wake
                        .wait_timeout(state, remaining.max(WAIT_SLICE))
                        .unwrap_or_else(|error| error.into_inner());
                    drop(guard);
                    if wait_result.timed_out() && Instant::now() >= self.deadline {
                        return Err(BoundedProcessError::DeadlineElapsed);
                    }
                    continue;
                }
            }

            if self.cancellation.is_cancelled() {
                self.kill_process_tree()?;
            }
            if Instant::now() >= self.deadline {
                self.kill_process_tree()?;
            }

            if let Some(status) = self.harvest_if_exited()? {
                break status;
            }

            if Instant::now() >= self.deadline {
                let child = {
                    let mut state = self.lock_child();
                    if let Some(status) = state.terminal {
                        break status;
                    }
                    if state.tree_cleanup == TreeCleanup::Cleaning {
                        state = self.wait_while_cleaning(state);
                    }
                    if let Some(status) = state.terminal {
                        break status;
                    }
                    if state.tree_cleanup != TreeCleanup::Cleaned {
                        return Err(BoundedProcessError::DeadlineElapsed);
                    }
                    if state.reaping {
                        let remaining = self.deadline.saturating_duration_since(Instant::now());
                        let (guard, wait_result) = self
                            .child_wake
                            .wait_timeout(state, remaining.max(WAIT_SLICE))
                            .unwrap_or_else(|error| error.into_inner());
                        drop(guard);
                        if wait_result.timed_out() && Instant::now() >= self.deadline {
                            return Err(BoundedProcessError::DeadlineElapsed);
                        }
                        continue;
                    }
                    state.reaping = true;
                    state.child.take()
                };
                if let Some(mut child) = child {
                    let result = wait_child_until(&mut child, Instant::now() + CLEANUP_GRACE);
                    let mut state = self.child.lock().unwrap_or_else(|error| error.into_inner());
                    state.reaping = false;
                    match result {
                        Ok(status) => {
                            state.terminal = Some(status);
                            self.child_wake.notify_all();
                            break status;
                        }
                        Err(error) => {
                            state.child = Some(child);
                            self.child_wake.notify_all();
                            return Err(error);
                        }
                    }
                }
            }

            thread::sleep(
                self.deadline
                    .saturating_duration_since(Instant::now())
                    .min(WAIT_SLICE),
            );
        };

        let stdin_result = self.stdin.close();
        self.drain_stop.store(true, Ordering::Release);
        let drain_result = self.join_drainers();
        match (stdin_result, drain_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(status),
        }
    }

    fn join_drainers(&self) -> Result<(), BoundedProcessError> {
        self.drain_stop.store(true, Ordering::Release);
        self.drainers_result
            .get_or_init(|| {
                let mut drainers = self
                    .drainers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .drain(..)
                    .collect::<Vec<_>>();
                let mut result = join_stream_drainers(&mut drainers);
                if result.is_ok() {
                    for stream in [LogStream::Stdout, LogStream::Stderr] {
                        if self
                            .logs
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .has_error(stream)
                        {
                            result = Err(BoundedProcessError::DrainFailed { stream });
                            break;
                        }
                    }
                }
                result
            })
            .clone()
    }

    fn kill_process_tree(&self) -> Result<(), BoundedProcessError> {
        self.cleanup_tree_once();
        Ok(())
    }

    fn snapshot(&self, stream: LogStream, offset: Option<u64>) -> LogSnapshot {
        self.logs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot(stream, offset)
    }

    #[cfg(all(test, unix))]
    fn run_hook(hook: &Mutex<Option<Arc<dyn Fn() + Send + Sync>>>) {
        let hook = hook
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(all(test, unix))]
    fn run_before_group_kill_hook(&self) {
        Self::run_hook(&self.test_hooks.before_group_kill);
    }

    #[cfg(all(test, unix))]
    fn run_on_wait_for_cleanup_hook(&self) {
        Self::run_hook(&self.test_hooks.on_wait_for_cleanup);
    }

    #[cfg(all(test, unix))]
    fn run_on_reap_hook(&self) {
        Self::run_hook(&self.test_hooks.on_reap);
    }
}

impl Drop for ProcessInner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.drain_stop.store(true, Ordering::Release);
        let _ = self.kill_process_tree();
        let _ = self.stdin.close();
        let _ = self.reap();
        let state = self
            .child
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(mut child) = state.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => std::mem::forget(child),
            }
        }
        let _ = self.join_drainers();
    }
}

/// Stable opaque process identity containing a non-reused generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessHandle {
    id: u64,
    generation: u64,
}

impl ProcessHandle {
    pub fn id(self) -> u64 {
        self.id
    }

    pub fn generation(self) -> u64 {
        self.generation
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn run_job_attach_and_resume<A, R, T>(
    attach: A,
    resume: R,
    terminate_on_failure: T,
) -> std::io::Result<()>
where
    A: FnOnce() -> std::io::Result<()>,
    R: FnOnce() -> std::io::Result<()>,
    T: FnOnce(),
{
    if let Err(error) = attach() {
        terminate_on_failure();
        return Err(error);
    }
    if let Err(error) = resume() {
        terminate_on_failure();
        return Err(error);
    }
    Ok(())
}

fn unbounded_deadline() -> Instant {
    Instant::now() + Duration::from_secs(365 * 24 * 60 * 60)
}

/// A background-capable bounded process object.
pub struct BoundedProcess {
    handle: BoundedProcessHandle,
}

impl Deref for BoundedProcess {
    type Target = BoundedProcessHandle;
    fn deref(&self) -> &BoundedProcessHandle {
        &self.handle
    }
}

impl fmt::Debug for BoundedProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedProcess")
            .field("handle", &self.handle.process.handle)
            .field("pid", &self.handle.process.pid)
            .finish_non_exhaustive()
    }
}

impl BoundedProcess {
    /// Spawns a process with native argv semantics and bounded piped stdio.
    pub fn spawn(request: BoundedProcessRequest) -> Result<Self, BoundedProcessError> {
        Self::spawn_internal(request, false)
    }

    fn spawn_for_exec(request: BoundedProcessRequest) -> Result<Self, BoundedProcessError> {
        Self::spawn_internal(request, true)
    }

    fn spawn_internal(
        request: BoundedProcessRequest,
        close_after_initial: bool,
    ) -> Result<Self, BoundedProcessError> {
        request
            .validate()
            .map_err(BoundedProcessError::InvalidRequest)?;
        let now = Instant::now();
        let deadline = request.effective_deadline(now)?;
        if request
            .cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(BoundedProcessError::Cancelled);
        }

        let cwd = request
            .resolved_cwd()
            .cloned()
            .ok_or(BoundedProcessError::InvalidRequest(
                ValidationError::CwdRequired,
            ))?;
        let mut command = std::process::Command::new(&request.argv[0]);
        command.args(&request.argv[1..]);
        command.current_dir(cwd);
        command.env_clear();
        command.envs(&request.env);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);

        let mut child = command
            .spawn()
            .map_err(|error| BoundedProcessError::Spawn(spawn_error(&error)))?;
        let pid = child.id();
        #[cfg(target_os = "linux")]
        let pidfd = match linux_pidfd::PidFd::open(pid) {
            Ok(pidfd) => Some(pidfd),
            Err(error) => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        };
        #[cfg(windows)]
        let job = match ProcessJob::attach_and_resume(&child) {
            Ok(job) => Some(job),
            Err(error) => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(SpawnError {
                    kind: SpawnErrorKind::Other,
                    os_code: None,
                }));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(SpawnError {
                    kind: SpawnErrorKind::Other,
                    os_code: None,
                }));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(SpawnError {
                    kind: SpawnErrorKind::Other,
                    os_code: None,
                }));
            }
        };
        #[cfg(unix)]
        if let Err(error) = super::shared::set_pipe_nonblocking(&stdout) {
            abort_spawned_child(child, pid);
            return Err(BoundedProcessError::Spawn(spawn_error(&error)));
        }
        #[cfg(unix)]
        if let Err(error) = super::shared::set_pipe_nonblocking(&stderr) {
            abort_spawned_child(child, pid);
            return Err(BoundedProcessError::Spawn(spawn_error(&error)));
        }
        #[cfg(windows)]
        let stdout = match super::windows_stdio::CancellableRead::from_stdout(stdout) {
            Ok(stdout) => stdout,
            Err(error) => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        };
        #[cfg(windows)]
        let stderr = match super::windows_stdio::CancellableRead::from_stderr(stderr) {
            Ok(stderr) => stderr,
            Err(error) => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        };
        #[cfg(not(any(unix, windows)))]
        {
            abort_spawned_child(child, pid);
            return Err(BoundedProcessError::Spawn(SpawnError {
                kind: SpawnErrorKind::Other,
                os_code: None,
            }));
        }

        let cancellation = request.cancellation_token.unwrap_or_default();
        let drain_stop = Arc::new(AtomicBool::new(false));
        let stdin_state = match StdinState::new(
            stdin,
            request.stdin,
            close_after_initial,
            cancellation.clone(),
            deadline,
        ) {
            Ok(state) => state,
            Err(error) => {
                abort_spawned_child(child, pid);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        };
        let logs = Arc::new(Mutex::new(LogStore::new(
            request.stdout_limit,
            request.stderr_limit,
            request.total_limit,
        )));
        let mut drainers = Vec::with_capacity(2);
        match spawn_drainer(
            "bounded-process-stdout",
            stdout,
            LogStream::Stdout,
            Arc::clone(&logs),
            Arc::clone(&drain_stop),
            cancellation.clone(),
            deadline,
        ) {
            Ok(drainer) => drainers.push(drainer),
            Err(error) => {
                abort_spawned_child(child, pid);
                let _ = stdin_state.close();
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        }
        match spawn_drainer(
            "bounded-process-stderr",
            stderr,
            LogStream::Stderr,
            Arc::clone(&logs),
            Arc::clone(&drain_stop),
            cancellation.clone(),
            deadline,
        ) {
            Ok(drainer) => drainers.push(drainer),
            Err(error) => {
                drain_stop.store(true, Ordering::Release);
                abort_spawned_child(child, pid);
                let _ = stdin_state.close();
                let _ = join_stream_drainers(&mut drainers);
                return Err(BoundedProcessError::Spawn(spawn_error(&error)));
            }
        }

        let inner = Arc::new(ProcessInner {
            handle: allocate_process_handle(),
            pid,
            #[cfg(windows)]
            job,
            #[cfg(target_os = "linux")]
            pidfd,
            deadline,
            child: Mutex::new(ChildState {
                child: Some(child),
                terminal: None,
                reaping: false,
                tree_cleanup: TreeCleanup::Pending,
            }),
            child_wake: Condvar::new(),
            logs,
            stdin: stdin_state,
            drainers: Mutex::new(drainers),
            drainers_result: OnceLock::new(),
            drain_stop,
            cancellation,
            #[cfg(all(test, unix))]
            test_hooks: TreeCleanupTestHooks::default(),
        });
        Ok(Self {
            handle: BoundedProcessHandle { process: inner },
        })
    }

    fn inner(&self) -> &ProcessInner {
        &self.handle.process
    }
}

/// A cloneable lifecycle reference that keeps a background process alive.
#[derive(Clone)]
pub struct BoundedProcessHandle {
    process: Arc<ProcessInner>,
}

impl fmt::Debug for BoundedProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedProcessHandle")
            .field("handle", &self.process.handle)
            .finish_non_exhaustive()
    }
}

impl BoundedProcess {
    pub fn lifecycle_handle(&self) -> BoundedProcessHandle {
        self.handle.clone()
    }
}

impl BoundedProcessHandle {
    pub fn handle(&self) -> ProcessHandle {
        self.process.handle
    }

    pub fn pid(&self) -> u32 {
        self.process.pid
    }

    pub fn deadline(&self) -> Instant {
        self.process.deadline
    }

    pub fn try_wait(&self) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        self.process.try_wait()
    }

    pub fn poll(&self) -> Result<Option<ProcessStatus>, BoundedProcessError> {
        self.process.poll()
    }

    pub fn wait_until(&self, deadline: Instant) -> Result<ProcessStatus, BoundedProcessError> {
        self.process
            .wait_until_with_hook(deadline.min(self.process.deadline), || false)
    }

    pub fn wait(&self, deadline: Option<Instant>) -> Result<ProcessStatus, BoundedProcessError> {
        match deadline {
            Some(deadline) => self.wait_until(deadline),
            None => self.wait_until(self.process.deadline),
        }
    }

    pub fn wait_forever(&self) -> Result<ProcessStatus, BoundedProcessError> {
        // No caller deadline: wait until the child exits or is cancelled.
        self.process
            .wait_until_with_hook(unbounded_deadline(), || false)
    }

    pub fn reap(&self) -> Result<ProcessStatus, BoundedProcessError> {
        self.process.reap()
    }

    pub fn terminal_status(&self) -> Option<ProcessStatus> {
        self.process
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .terminal
    }

    pub fn stdout_snapshot(&self) -> LogSnapshot {
        self.process.snapshot(LogStream::Stdout, None)
    }

    pub fn stderr_snapshot(&self) -> LogSnapshot {
        self.process.snapshot(LogStream::Stderr, None)
    }

    pub fn stdout_snapshot_from(&self, offset: u64) -> LogSnapshot {
        self.process.snapshot(LogStream::Stdout, Some(offset))
    }

    pub fn stderr_snapshot_from(&self, offset: u64) -> LogSnapshot {
        self.process.snapshot(LogStream::Stderr, Some(offset))
    }

    pub fn write_stdin(&self, bytes: &[u8]) -> Result<usize, BoundedProcessError> {
        self.process.stdin.write(bytes)
    }

    pub fn close_stdin(&self) -> Result<(), BoundedProcessError> {
        self.process.stdin.close()
    }

    pub fn kill_process_tree(&self) -> Result<(), BoundedProcessError> {
        self.process.kill_process_tree()
    }

    pub fn cancel(&self) {
        self.process.cancellation.cancel();
        let _ = self.process.kill_process_tree();
    }

    pub fn shutdown(&self) -> Result<(), BoundedProcessError> {
        self.cancel();
        let close_result = self.close_stdin();
        let reap_result = self.reap();
        match (close_result, reap_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(_)) => Err(error),
            (Ok(()), Ok(_)) => Ok(()),
        }
    }
}

/// Bounded foreground execution result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedExecOutput {
    pub status: ProcessStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_offset: u64,
    pub stdout_next_offset: u64,
    pub stdout_truncated: bool,
    pub stdout_gap: bool,
    pub stderr_offset: u64,
    pub stderr_next_offset: u64,
    pub stderr_truncated: bool,
    pub stderr_gap: bool,
}

/// Foreground failures preserve bounded output for timeout and cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundedExecError {
    Spawn(BoundedProcessError),
    TimedOut(BoundedExecOutput),
    Cancelled(BoundedExecOutput),
    Failed(BoundedProcessError),
}

impl fmt::Display for BoundedExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "process spawn error: {error}"),
            Self::TimedOut(_) => formatter.write_str("process timed out"),
            Self::Cancelled(_) => formatter.write_str("process was cancelled"),
            Self::Failed(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BoundedExecError {}

fn make_exec_output(process: &BoundedProcess, status: ProcessStatus) -> BoundedExecOutput {
    let stdout = process.stdout_snapshot();
    let stderr = process.stderr_snapshot();
    BoundedExecOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_offset: stdout.offset,
        stdout_next_offset: stdout.next_offset,
        stdout_truncated: stdout.truncated,
        stdout_gap: stdout.gap,
        stderr_offset: stderr.offset,
        stderr_next_offset: stderr.next_offset,
        stderr_truncated: stderr.truncated,
        stderr_gap: stderr.gap,
    }
}

fn empty_exec_output() -> BoundedExecOutput {
    BoundedExecOutput {
        status: ProcessStatus::Unknown,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_offset: 0,
        stdout_next_offset: 0,
        stdout_truncated: false,
        stdout_gap: false,
        stderr_offset: 0,
        stderr_next_offset: 0,
        stderr_truncated: false,
        stderr_gap: false,
    }
}

fn exec_bounded_with_cancel_hook<F>(
    request: BoundedProcessRequest,
    is_cancelled: F,
) -> Result<BoundedExecOutput, BoundedExecError>
where
    F: Fn() -> bool,
{
    let process = match BoundedProcess::spawn_for_exec(request) {
        Ok(process) => process,
        Err(BoundedProcessError::Cancelled) => {
            return Err(BoundedExecError::Cancelled(empty_exec_output()));
        }
        Err(BoundedProcessError::DeadlineElapsed) => {
            return Err(BoundedExecError::TimedOut(empty_exec_output()));
        }
        Err(error) => return Err(BoundedExecError::Spawn(error)),
    };
    if !process.inner().stdin.has_initial_payload()
        && let Err(error) = process.close_stdin()
    {
        return Err(BoundedExecError::Failed(error));
    }
    let status = match process
        .inner()
        .wait_until_with_hook(process.deadline(), is_cancelled)
    {
        Ok(status) => status,
        Err(BoundedProcessError::DeadlineElapsed) => {
            let status = process.terminal_status().unwrap_or(ProcessStatus::Unknown);
            return Err(BoundedExecError::TimedOut(make_exec_output(
                &process, status,
            )));
        }
        Err(BoundedProcessError::Cancelled) => {
            let status = process.terminal_status().unwrap_or(ProcessStatus::Unknown);
            return Err(BoundedExecError::Cancelled(make_exec_output(
                &process, status,
            )));
        }
        Err(error) => return Err(BoundedExecError::Failed(error)),
    };
    if let Err(error) = process.reap() {
        return Err(BoundedExecError::Failed(error));
    }
    Ok(make_exec_output(&process, status))
}

/// Executes a bounded request in the foreground without invoking a shell.
pub fn exec_bounded(request: BoundedProcessRequest) -> Result<BoundedExecOutput, BoundedExecError> {
    exec_bounded_with_cancel_hook(request, || false)
}

/// Executes a request while an embedding-owned cancellation hook is polled.
pub(crate) fn exec_bounded_with_cancel_hook_for_host<F>(
    request: BoundedProcessRequest,
    is_cancelled: F,
) -> Result<BoundedExecOutput, BoundedExecError>
where
    F: Fn() -> bool,
{
    exec_bounded_with_cancel_hook(request, is_cancelled)
}

fn terminate_process_tree(process_id: u32) {
    #[cfg(all(test, unix))]
    record_tree_kill_identity(process_id);
    super::shared::terminate_process_group(process_id);
}

#[cfg(all(test, unix))]
fn proc_is_our_zombie(pid: u32) -> bool {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let Some(close) = text.rfind(')') else {
        return false;
    };
    let mut fields = text[close + 1..].split_whitespace();
    let Some(state) = fields.next() else {
        return false;
    };
    let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    state == "Z" && ppid == std::process::id()
}

#[cfg(all(test, unix))]
fn record_tree_kill_identity(pid: u32) {
    LAST_TREE_KILL_RETAINED_ZOMBIE.with(|flag| flag.set(proc_is_our_zombie(pid)));
}

#[cfg(all(test, unix))]
thread_local! {
    static LAST_TREE_KILL_RETAINED_ZOMBIE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(all(test, unix))]
pub struct TreeKillZombieProbe {
    _private: (),
}

#[cfg(all(test, unix))]
impl TreeKillZombieProbe {
    pub fn install() -> Self {
        LAST_TREE_KILL_RETAINED_ZOMBIE.with(|flag| flag.set(false));
        Self { _private: () }
    }

    pub fn retained(&self) -> bool {
        LAST_TREE_KILL_RETAINED_ZOMBIE.with(|flag| flag.get())
    }
}

#[cfg(all(test, unix))]
impl Drop for TreeKillZombieProbe {
    fn drop(&mut self) {
        LAST_TREE_KILL_RETAINED_ZOMBIE.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_grammar_is_strict_and_errors_are_bounded() {
        let request = BoundedProcessRequest::new(vec!["program".to_owned()])
            .with_workspace_root(std::env::temp_dir())
            .with_env("BAD=KEY", "secret")
            .with_stdin(vec![1, 2, 3]);
        let error = request.validate().expect_err("invalid key should fail");
        assert_eq!(error, ValidationError::InvalidEnvKey);
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn ring_snapshots_keep_the_tail_and_report_gaps() {
        let mut ring = RingLog::new(4);
        ring.append(b"0123456789", 0);
        let snapshot = ring.snapshot_from(Some(0));
        assert_eq!(snapshot.bytes, b"6789");
        assert_eq!(snapshot.offset, 6);
        assert_eq!(snapshot.next_offset, 10);
        assert!(snapshot.truncated);
        assert!(snapshot.gap);
    }

    #[test]
    fn ring_snapshot_beyond_eof_clamps_to_the_observed_end() {
        let mut ring = RingLog::new(4);
        ring.append(b"0123456789", 0);
        let snapshot = ring.snapshot_from(Some(99));
        assert!(snapshot.bytes.is_empty());
        assert_eq!(snapshot.offset, 10);
        assert_eq!(snapshot.next_offset, 10);
        assert!(!snapshot.gap);
    }

    #[test]
    fn ring_store_evicts_by_global_arrival_order() {
        let mut store = LogStore::new(8, 8, 10);
        store.append(LogStream::Stdout, b"AAAAAAAA");
        store.append(LogStream::Stderr, b"BBBBBBBB");
        let stdout = store.snapshot(LogStream::Stdout, None);
        let stderr = store.snapshot(LogStream::Stderr, None);
        assert_eq!(stdout.bytes.len() + stderr.bytes.len(), 10);
        assert_eq!(stderr.bytes, b"BBBBBBBB");
        assert_eq!(stdout.bytes, b"AA");
        assert!(stdout.truncated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_open_fails_closed_for_missing_pid() {
        assert!(linux_pidfd::PidFd::open(u32::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn normal_exit_descendant_cleanup_retains_root_zombie_identity() {
        let probe = TreeKillZombieProbe::install();
        let marker = std::env::temp_dir().join(format!(
            "rustscript-bounded-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_text = marker.display().to_string();
        let result = exec_bounded(
            BoundedProcessRequest::new(vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sleep 60 & echo $! > \"$1\"; exit 0".to_owned(),
                "bounded-identity-test".to_owned(),
                marker_text,
            ])
            .with_workspace_root(std::env::temp_dir())
            .with_timeout(Duration::from_secs(2)),
        )
        .expect("root process should exit normally");
        assert!(result.status.is_success());
        assert!(
            probe.retained(),
            "descendant cleanup must target the process group while the root zombie still anchors the pid"
        );
        let descendant_pid = std::fs::read_to_string(&marker)
            .expect("child pid marker should be written")
            .trim()
            .parse::<libc::pid_t>()
            .expect("child pid marker should contain a pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if unsafe { libc::kill(descendant_pid, 0) } != 0 {
                let _ = std::fs::remove_file(marker);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&marker);
        panic!("descendant {descendant_pid} is still present");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_harvest_cannot_reap_before_group_kill_identity() {
        use std::sync::Barrier;
        use std::sync::mpsc;

        let probe = TreeKillZombieProbe::install();
        let process = BoundedProcess::spawn(
            BoundedProcessRequest::new(vec!["/bin/true".to_owned()])
                .with_workspace_root(std::env::temp_dir())
                .with_timeout(Duration::from_secs(2)),
        )
        .expect("true should spawn");
        let pid = process.pid();
        let zombie_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < zombie_deadline && !proc_is_our_zombie(pid) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            proc_is_our_zombie(pid),
            "root must remain a zombie before the cleanup/reap race window"
        );

        let claimed = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (decision_tx, decision_rx) = mpsc::sync_channel::<&'static str>(4);
        {
            let claimed = Arc::clone(&claimed);
            let release = Arc::clone(&release);
            *process
                .inner()
                .test_hooks
                .before_group_kill
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(move || {
                claimed.wait();
                release.wait();
            }));
        }
        {
            let decision_tx = decision_tx.clone();
            *process
                .inner()
                .test_hooks
                .on_wait_for_cleanup
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(move || {
                let _ = decision_tx.send("wait");
            }));
        }
        {
            let decision_tx = decision_tx.clone();
            *process
                .inner()
                .test_hooks
                .on_reap
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(move || {
                let _ = decision_tx.send("reap");
            }));
        }

        struct ReleaseGuard {
            release: Arc<Barrier>,
            released: bool,
        }
        impl Drop for ReleaseGuard {
            fn drop(&mut self) {
                if !self.released {
                    self.release.wait();
                }
            }
        }
        impl ReleaseGuard {
            fn release(&mut self) {
                self.released = true;
                self.release.wait();
            }
        }

        let process = Arc::new(process);
        let cleaner = {
            let process = Arc::clone(&process);
            thread::spawn(move || process.kill_process_tree())
        };
        claimed.wait();
        let mut release_guard = ReleaseGuard {
            release: Arc::clone(&release),
            released: false,
        };
        assert!(
            proc_is_our_zombie(pid),
            "numeric group kill must not have run yet"
        );

        let harvester = {
            let process = Arc::clone(&process);
            thread::spawn(move || process.try_wait())
        };
        let decision = decision_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("harvest must wait for cleanup or attempt reap");
        assert_eq!(
            decision, "wait",
            "concurrent harvest must not call try_wait/reap before group kill returns"
        );
        assert!(
            proc_is_our_zombie(pid),
            "pid identity must stay our zombie until group kill returns"
        );

        release_guard.release();
        cleaner
            .join()
            .expect("cleanup thread")
            .expect("kill process tree");
        let harvested = harvester
            .join()
            .expect("harvest thread")
            .expect("try_wait after cleanup");
        assert!(
            harvested.is_some(),
            "harvest should reap only after group kill"
        );
        assert!(
            probe.retained()
                || process
                    .inner()
                    .test_hooks
                    .last_tree_kill_retained_zombie
                    .load(Ordering::Acquire),
            "group kill must target the original zombie identity before reap"
        );
    }

    #[test]
    fn close_does_not_wait_for_in_flight_read() {
        use std::sync::Barrier;
        struct BlockingReader {
            started: Arc<Barrier>,
            release: Arc<Barrier>,
        }
        impl Read for BlockingReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                self.started.wait();
                self.release.wait();
                Ok(0)
            }
        }
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let reader = InterruptibleReader::new(BlockingReader {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        let reader_thread = Arc::clone(&reader);
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 8];
            reader_thread.read_bytes(&mut buf)
        });
        started.wait();
        let started_at = Instant::now();
        reader.close();
        let elapsed = started_at.elapsed();
        release.wait();
        handle.join().expect("reader thread").expect("read");
        assert!(
            elapsed < Duration::from_millis(200),
            "close blocked on in-flight read for {elapsed:?}"
        );
    }

    #[test]
    fn job_attach_failure_terminates_child() {
        let terminated = AtomicBool::new(false);
        let result = run_job_attach_and_resume(
            || Err(std::io::Error::other("attach failed")),
            || Ok(()),
            || terminated.store(true, Ordering::Release),
        );
        assert!(result.is_err());
        assert!(terminated.load(Ordering::Acquire));
    }

    #[test]
    fn job_resume_failure_terminates_child() {
        let terminated = AtomicBool::new(false);
        let attached = AtomicBool::new(false);
        let result = run_job_attach_and_resume(
            || {
                attached.store(true, Ordering::Release);
                Ok(())
            },
            || Err(std::io::Error::other("resume failed")),
            || terminated.store(true, Ordering::Release),
        );
        assert!(result.is_err());
        assert!(attached.load(Ordering::Acquire));
        assert!(terminated.load(Ordering::Acquire));
    }

    #[test]
    fn job_attach_success_does_not_terminate() {
        let terminated = AtomicBool::new(false);
        run_job_attach_and_resume(
            || Ok(()),
            || Ok(()),
            || terminated.store(true, Ordering::Release),
        )
        .expect("success");
        assert!(!terminated.load(Ordering::Acquire));
    }

    #[test]
    fn rss_and_request_share_argv_validation() {
        assert_eq!(validate_argv(&[]).unwrap_err(), ValidationError::EmptyArgv);
        let too_long = vec!["x".to_owned(); MAX_ARG_COUNT + 1];
        assert_eq!(
            validate_argv(&too_long).unwrap_err(),
            ValidationError::ArgCountExceeded
        );
        assert_eq!(
            BoundedProcessRequest::new(vec![]).validate().unwrap_err(),
            ValidationError::EmptyArgv
        );
    }
}
