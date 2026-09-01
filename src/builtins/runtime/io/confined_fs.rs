//! Root-confined filesystem capability for host integrations.
//!
//! [`ConfinedFsRoot`] retains an operating-system directory handle and resolves
//! every later path relative to that handle. Relative paths are validated
//! before they reach the operating system. On Unix, Linux uses `openat2` with
//! `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS` when the
//! kernel provides it; the fallback walks each component with `openat`,
//! `O_DIRECTORY | O_NOFOLLOW`, and never canonicalizes a path before opening
//! it. The root handle is never reopened through a path, including through
//! `/proc/self/fd`.
//!
//! Regular files with more than one hard link are rejected. This deliberately
//! conservative policy prevents a capability path from reaching an inode that
//! also has an unrelated directory entry. Atomic replacement is a Linux-only
//! `renameat2` capability (`RENAME_NOREPLACE` / identity-checked
//! `RENAME_EXCHANGE`). Other Unix targets fail closed with
//! [`ConfinedFsErrorKind::UnsupportedPlatform`] rather than creating a
//! temporary that can never be published. Publication returns a
//! [`ConfinedPublication`] once the destination name contains the retained
//! inode; parent-directory durability and staging cleanup are recorded on that
//! outcome instead of being reported as pre-publication failures.
//!
//! Windows and targets without a Unix descriptor API fail closed with
//! [`ConfinedFsErrorKind::UnsupportedPlatform`]. Reparse-point-safe handle
//! operations are not silently emulated by path-based calls.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::Path;

#[cfg(all(unix, test))]
use std::cell::Cell;
#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum accepted relative path length in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Maximum accepted single-component length in bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Maximum temporary-file prefix length, leaving room for a generated suffix.
pub const MAX_TEMP_PREFIX_BYTES: usize = 192;
/// Hard upper bound for a single read budget.
pub const MAX_READ_BYTES: usize = 64 * 1024 * 1024;
/// Hard upper bound for a single write budget.
pub const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;
/// Hard upper bound for directory enumeration entries.
pub const MAX_ENUM_ENTRIES: usize = 1_000_000;
/// Hard upper bound for temporary-file name attempts.
pub const MAX_TEMP_ATTEMPTS: u32 = 128;

#[cfg(all(unix, test))]
thread_local! {
    static TEST_PARTIAL_WRITE_FAIL_AFTER: Cell<u64> = const { Cell::new(u64::MAX) };
}

#[cfg(all(unix, test))]
struct PartialWriteFailGuard {
    previous: u64,
}

#[cfg(all(unix, test))]
impl PartialWriteFailGuard {
    fn new(fail_after: u64) -> Self {
        let previous = TEST_PARTIAL_WRITE_FAIL_AFTER.with(|slot| slot.replace(fail_after));
        Self { previous }
    }
}

#[cfg(all(unix, test))]
impl Drop for PartialWriteFailGuard {
    fn drop(&mut self) {
        TEST_PARTIAL_WRITE_FAIL_AFTER.with(|slot| slot.set(self.previous));
    }
}

/// Stable categories returned by the confined filesystem API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfinedFsErrorKind {
    /// The operation received a path or configuration that is not permitted.
    InvalidPath,
    /// The path was empty where a file path was required.
    EmptyPath,
    /// The path is absolute or has a host-specific root.
    AbsolutePath,
    /// The path contains a parent traversal component.
    ParentTraversal,
    /// The path contains an embedded NUL byte.
    NulByte,
    /// The path exceeds the configured hard bound.
    PathTooLong,
    /// A path component exceeds the configured hard bound.
    ComponentTooLong,
    /// A platform separator or drive-prefix character was supplied.
    InvalidSeparator,
    /// A path prefix such as a drive or UNC prefix was supplied.
    PathPrefix,
    /// The requested operation is unavailable on this target.
    UnsupportedPlatform,
    /// The retained root or a requested entry could not be found.
    NotFound,
    /// The operating system denied the operation.
    PermissionDenied,
    /// A symlink or reparse-like indirection was encountered.
    SymlinkDenied,
    /// A regular file has more than one hard link.
    HardlinkDenied,
    /// An entry is not the type required by the operation.
    WrongType,
    /// A bounded operation would exceed its byte or entry budget.
    BudgetExceeded,
    /// The bounded temporary-name retry budget was exhausted.
    TempCollision,
    /// A checked directory entry changed before the operation completed.
    RaceDetected,
    /// A destination or other entry already exists where it cannot be used.
    AlreadyExists,
    /// A temporary file was used after it was published or cleaned up.
    TempCompleted,
    /// The supplied limits are not valid.
    InvalidConfiguration,
    /// A temporary file belongs to a different retained root capability.
    CapabilityMismatch,
    /// The content is not valid for the requested representation.
    InvalidData,
    /// An operating-system error did not fit a more specific category.
    Io,
}

impl ConfinedFsErrorKind {
    /// Returns the stable machine-readable spelling of this category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::EmptyPath => "empty_path",
            Self::AbsolutePath => "absolute_path",
            Self::ParentTraversal => "parent_traversal",
            Self::NulByte => "nul_byte",
            Self::PathTooLong => "path_too_long",
            Self::ComponentTooLong => "component_too_long",
            Self::InvalidSeparator => "invalid_separator",
            Self::PathPrefix => "path_prefix",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::SymlinkDenied => "symlink_denied",
            Self::HardlinkDenied => "hardlink_denied",
            Self::WrongType => "wrong_type",
            Self::BudgetExceeded => "budget_exceeded",
            Self::TempCollision => "temp_collision",
            Self::RaceDetected => "race_detected",
            Self::AlreadyExists => "already_exists",
            Self::TempCompleted => "temp_completed",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::CapabilityMismatch => "capability_mismatch",
            Self::InvalidData => "invalid_data",
            Self::Io => "io",
        }
    }
}

/// Whether this target can atomically publish a confined temporary file.
///
/// Atomic publication requires Linux `renameat2`. Other Unix descriptor APIs
/// can open and read through a retained root, but they cannot complete the
/// publication protocol implemented here.
pub const fn publication_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Identity and type of a directory entry observed after a publication race.
///
/// Recorded on [`ConfinedPublicationState::Indeterminate`] so a caller can
/// inspect the destination and staging names without treating the race as a
/// successful publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfinedObservedIdentity {
    file_type: ConfinedFileType,
    device: u64,
    inode: u64,
}

impl ConfinedObservedIdentity {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    const fn new(file_type: ConfinedFileType, device: u64, inode: u64) -> Self {
        Self {
            file_type,
            device,
            inode,
        }
    }

    /// Returns the observed entry type.
    pub const fn file_type(self) -> ConfinedFileType {
        self.file_type
    }

    /// Returns the observed device identity.
    pub const fn device(self) -> u64 {
        self.device
    }

    /// Returns the observed inode identity.
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Confirmed publication of a retained temporary inode to a destination name.
///
/// This value is returned only after the destination directory entry contains
/// the retained inode. Parent-directory `fsync` and staging-name cleanup are
/// recorded separately so a durability or cleanup issue cannot be mistaken for
/// a pre-publication failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfinedPublication {
    durable: bool,
    staging_cleaned: bool,
}

impl ConfinedPublication {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    const fn new(durable: bool, staging_cleaned: bool) -> Self {
        Self {
            durable,
            staging_cleaned,
        }
    }

    /// Destination contains the retained inode.
    pub const fn is_published(self) -> bool {
        true
    }

    /// Parent directory contents were synchronized after publication.
    pub const fn is_durable(self) -> bool {
        self.durable
    }

    /// The same-directory staging name was unlinked after publication.
    pub const fn staging_cleaned(self) -> bool {
        self.staging_cleaned
    }

    /// Returns the corresponding publication state.
    pub const fn state(self) -> ConfinedPublicationState {
        ConfinedPublicationState::Published {
            durable: self.durable,
            staging_cleaned: self.staging_cleaned,
        }
    }
}

/// Publication state carried by a replace outcome or error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfinedPublicationState {
    /// The destination name does not contain the retained inode.
    NotPublished,
    /// The destination name contains the retained inode.
    Published {
        /// Parent directory `fsync` completed after publication.
        durable: bool,
        /// Same-directory staging cleanup completed after publication.
        staging_cleaned: bool,
    },
    /// Destination could not be classified as the retained inode or as the
    /// restored directory. Observed identities are recorded when available.
    Indeterminate {
        /// Observed destination identity, if the entry could be read.
        destination: Option<ConfinedObservedIdentity>,
        /// Observed staging identity, if the entry could be read.
        staging: Option<ConfinedObservedIdentity>,
    },
}

impl ConfinedPublicationState {
    /// Returns whether the destination contains the retained inode.
    pub const fn is_published(self) -> bool {
        matches!(self, Self::Published { .. })
    }

    /// Returns whether parent-directory durability completed after publication.
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Published { durable: true, .. })
    }

    /// Returns whether staging cleanup completed after publication.
    pub const fn staging_cleaned(self) -> bool {
        matches!(
            self,
            Self::Published {
                staging_cleaned: true,
                ..
            }
        )
    }

    /// Returns whether publication raced into an unclassified destination.
    pub const fn is_indeterminate(self) -> bool {
        matches!(self, Self::Indeterminate { .. })
    }
}

/// Typed, bounded error from a root-confined filesystem operation.
///
/// The error intentionally contains no root path or caller-supplied path.
/// [`Self::raw_os_error`] exposes only a numeric operating-system code when
/// one exists. [`Self::publication_state`] distinguishes unpublished failures
/// from post-publication issues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfinedFsError {
    kind: ConfinedFsErrorKind,
    operation: &'static str,
    message: &'static str,
    limit: Option<usize>,
    value: Option<usize>,
    raw_os_error: Option<i32>,
    publication: ConfinedPublicationState,
}

impl ConfinedFsError {
    fn new(kind: ConfinedFsErrorKind, operation: &'static str, message: &'static str) -> Self {
        Self {
            kind,
            operation,
            message,
            limit: None,
            value: None,
            raw_os_error: None,
            publication: ConfinedPublicationState::NotPublished,
        }
    }

    #[cfg(unix)]
    fn os(operation: &'static str, error: &io::Error) -> Self {
        let raw_os_error = error.raw_os_error();
        let kind = classify_os_error(raw_os_error);
        Self {
            kind,
            operation,
            message: "operating-system operation failed",
            limit: None,
            value: None,
            raw_os_error,
            publication: ConfinedPublicationState::NotPublished,
        }
    }

    #[cfg(unix)]
    fn budget(operation: &'static str, message: &'static str, limit: usize, value: usize) -> Self {
        Self {
            kind: ConfinedFsErrorKind::BudgetExceeded,
            operation,
            message,
            limit: Some(limit),
            value: Some(value),
            raw_os_error: None,
            publication: ConfinedPublicationState::NotPublished,
        }
    }

    fn invalid_configuration(message: &'static str) -> Self {
        Self::new(
            ConfinedFsErrorKind::InvalidConfiguration,
            "fs::configure",
            message,
        )
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn with_publication(mut self, publication: ConfinedPublicationState) -> Self {
        self.publication = publication;
        self
    }

    /// Returns the stable error category.
    pub fn kind(&self) -> ConfinedFsErrorKind {
        self.kind
    }

    /// Returns the stable operation label.
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// Returns a bounded, path-free message.
    pub fn message(&self) -> &'static str {
        self.message
    }

    /// Returns the configured bound involved in the error, if applicable.
    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Returns the observed value involved in the error, if applicable.
    pub fn value(&self) -> Option<usize> {
        self.value
    }

    /// Returns the numeric operating-system error, if one was available.
    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }

    /// Returns whether the destination was published when this error was
    /// produced.
    pub fn publication_state(&self) -> ConfinedPublicationState {
        self.publication
    }
}

impl fmt::Display for ConfinedFsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "confined filesystem error [{}] in {}: {}",
            self.kind.as_str(),
            self.operation,
            self.message
        )?;
        if let Some(limit) = self.limit {
            write!(formatter, " (limit: {limit})")?;
        }
        if let Some(value) = self.value {
            write!(formatter, " (value: {value})")?;
        }
        if let Some(raw_os_error) = self.raw_os_error {
            write!(formatter, " (os error: {raw_os_error})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfinedFsError {}

/// Limits applied to every bounded operation on a [`ConfinedFsRoot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfinedFsLimits {
    /// Maximum bytes returned by one file read.
    pub max_read_bytes: usize,
    /// Maximum cumulative bytes written through one temporary file.
    pub max_write_bytes: usize,
    /// Maximum entries returned by one directory enumeration.
    pub max_entries: usize,
    /// Maximum name bytes accepted by one enumeration.
    pub max_entry_name_bytes: usize,
    /// Maximum exclusive-create attempts for one temporary file.
    pub max_temp_attempts: u32,
}

impl Default for ConfinedFsLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 8 * 1024 * 1024,
            max_write_bytes: 8 * 1024 * 1024,
            max_entries: 4096,
            max_entry_name_bytes: MAX_COMPONENT_BYTES,
            max_temp_attempts: 32,
        }
    }
}

impl ConfinedFsLimits {
    fn validate(self) -> Result<Self, ConfinedFsError> {
        if self.max_read_bytes > MAX_READ_BYTES {
            return Err(ConfinedFsError::invalid_configuration(
                "read budget exceeds the hard bound",
            ));
        }
        if self.max_write_bytes > MAX_WRITE_BYTES {
            return Err(ConfinedFsError::invalid_configuration(
                "write budget exceeds the hard bound",
            ));
        }
        if self.max_entries > MAX_ENUM_ENTRIES {
            return Err(ConfinedFsError::invalid_configuration(
                "enumeration budget exceeds the hard bound",
            ));
        }
        if self.max_entry_name_bytes > MAX_COMPONENT_BYTES {
            return Err(ConfinedFsError::invalid_configuration(
                "entry-name budget exceeds the hard bound",
            ));
        }
        if self.max_temp_attempts == 0 || self.max_temp_attempts > MAX_TEMP_ATTEMPTS {
            return Err(ConfinedFsError::invalid_configuration(
                "temporary retry budget is outside the hard bound",
            ));
        }
        Ok(self)
    }
}

/// Per-call directory enumeration bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnumerationBudget {
    /// Maximum entries to return.
    pub max_entries: usize,
    /// Maximum bytes in one entry name.
    pub max_name_bytes: usize,
}

impl Default for EnumerationBudget {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_name_bytes: MAX_COMPONENT_BYTES,
        }
    }
}

/// The type of an entry observed without following a symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfinedFileType {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symlink. It is reported by metadata and never followed by opens.
    Symlink,
    /// Any other operating-system entry type.
    Other,
}

/// Metadata observed relative to a confined root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfinedMetadata {
    file_type: ConfinedFileType,
    len: u64,
    link_count: u64,
}

impl ConfinedMetadata {
    /// Returns the entry type.
    pub fn file_type(&self) -> ConfinedFileType {
        self.file_type
    }

    /// Returns the byte length reported by the operating system.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns whether the entry reports a zero byte length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the observed hard-link count.
    pub fn link_count(&self) -> u64 {
        self.link_count
    }

    /// Returns whether the entry is a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type == ConfinedFileType::File
    }

    /// Returns whether the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type == ConfinedFileType::Directory
    }
}

/// One bounded directory entry with no path outside the retained root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfinedDirEntry {
    name: String,
    name_os: OsString,
    metadata: ConfinedMetadata,
}

impl ConfinedDirEntry {
    /// Returns a lossy UTF-8 display form of the entry's name.
    ///
    /// Use [`Self::name_os`] or [`Self::name_bytes`] when the exact name must
    /// be retained. Invalid UTF-8 is represented with replacement characters
    /// here and never causes enumeration to fail.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exact operating-system entry name.
    pub fn name_os(&self) -> &OsStr {
        &self.name_os
    }

    /// Returns the exact entry name bytes on Unix.
    #[cfg(unix)]
    pub fn name_bytes(&self) -> &[u8] {
        self.name_os.as_bytes()
    }

    /// Returns metadata collected without following a symlink.
    pub fn metadata(&self) -> ConfinedMetadata {
        self.metadata
    }
}

/// An open regular file reached through a [`ConfinedFsRoot`].
#[derive(Debug)]
pub struct ConfinedFile {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    max_read_bytes: usize,
}

impl ConfinedFile {
    /// Reads at most the root's configured read budget plus one byte.
    ///
    /// Reading one extra byte allows the method to report a budget violation
    /// without allocating an unbounded buffer.
    pub fn read_to_end(&mut self) -> Result<Vec<u8>, ConfinedFsError> {
        #[cfg(unix)]
        {
            let mut output = Vec::with_capacity(self.max_read_bytes.min(8192));
            let mut limited = (&mut self.file).take(self.max_read_bytes as u64 + 1);
            limited
                .read_to_end(&mut output)
                .map_err(|error| ConfinedFsError::os("fs::read", &error))?;
            if output.len() > self.max_read_bytes {
                return Err(ConfinedFsError::budget(
                    "fs::read",
                    "read budget exceeded",
                    self.max_read_bytes,
                    output.len(),
                ));
            }
            Ok(output)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported_error("fs::read"))
        }
    }

    /// Reads a UTF-8 file under the same byte budget as [`Self::read_to_end`].
    pub fn read_to_string(&mut self) -> Result<String, ConfinedFsError> {
        String::from_utf8(self.read_to_end()?).map_err(|_| {
            ConfinedFsError::new(
                ConfinedFsErrorKind::InvalidData,
                "fs::read",
                "file is not valid UTF-8",
            )
        })
    }

    /// Returns metadata for this already-open handle.
    pub fn metadata(&self) -> Result<ConfinedMetadata, ConfinedFsError> {
        #[cfg(unix)]
        {
            let metadata = unix::metadata_from_fd(self.file.as_raw_fd())
                .map_err(|error| ConfinedFsError::os("fs::metadata", &error))?;
            enforce_hardlink_policy("fs::metadata", metadata)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported_error("fs::metadata"))
        }
    }
}

/// An opaque retained directory handle opened through a [`ConfinedFsRoot`].
///
/// The capability owns the directory descriptor and exposes no public path or
/// raw-fd accessor. Cloning retains the same directory through an `Arc`.
#[derive(Clone)]
pub struct ConfinedDirectory {
    #[cfg(unix)]
    inner: Arc<ConfinedDirectoryInner>,
    #[cfg(not(unix))]
    _private: (),
}

#[cfg(unix)]
struct ConfinedDirectoryInner {
    fd: OwnedFd,
}

impl fmt::Debug for ConfinedDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfinedDirectory")
            .finish_non_exhaustive()
    }
}

impl ConfinedDirectory {
    #[cfg(unix)]
    fn from_fd(fd: OwnedFd) -> Self {
        Self {
            inner: Arc::new(ConfinedDirectoryInner { fd }),
        }
    }

    #[cfg(unix)]
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.inner.fd.as_raw_fd()
    }
}

/// A securely created temporary file and its retained parent directory.
#[derive(Debug)]
pub struct ConfinedTempFile {
    #[cfg(unix)]
    parent: OwnedFd,
    #[cfg(unix)]
    root_identity: unix::FileIdentity,
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    name: String,
    #[cfg(unix)]
    initial_identity: unix::FileIdentity,
    #[cfg(unix)]
    max_write_bytes: usize,
    #[cfg(unix)]
    written: usize,
    #[cfg(unix)]
    completed: bool,
}

impl ConfinedTempFile {
    /// Returns the generated temporary basename, never an absolute path.
    pub fn name(&self) -> &str {
        #[cfg(unix)]
        {
            &self.name
        }
        #[cfg(not(unix))]
        {
            ""
        }
    }

    /// Returns the cumulative bytes written through this handle.
    pub fn bytes_written(&self) -> usize {
        #[cfg(unix)]
        {
            self.written
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    /// Writes data while enforcing the root's cumulative write budget.
    pub fn write_all(&mut self, data: &[u8]) -> Result<(), ConfinedFsError> {
        #[cfg(unix)]
        {
            if self.completed {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::TempCompleted,
                    "fs::temp_write",
                    "temporary file has already been completed",
                ));
            }
            let new_total = self.written.checked_add(data.len()).ok_or_else(|| {
                ConfinedFsError::budget(
                    "fs::temp_write",
                    "write budget exceeded",
                    self.max_write_bytes,
                    usize::MAX,
                )
            })?;
            if new_total > self.max_write_bytes {
                return Err(ConfinedFsError::budget(
                    "fs::temp_write",
                    "write budget exceeded",
                    self.max_write_bytes,
                    new_total,
                ));
            }
            let mut offset = 0;
            while offset < data.len() {
                let end = {
                    #[cfg(test)]
                    {
                        let limit = TEST_PARTIAL_WRITE_FAIL_AFTER.with(Cell::get);
                        if limit != u64::MAX {
                            offset.saturating_add(1).min(data.len()).min(limit as usize)
                        } else {
                            data.len()
                        }
                    }
                    #[cfg(not(test))]
                    {
                        data.len()
                    }
                };
                if end <= offset {
                    return Err(ConfinedFsError::os(
                        "fs::temp_write",
                        &io::Error::other("temporary write test hook stopped progress"),
                    ));
                }
                match self
                    .file
                    .write(&data[offset..end])
                    .map_err(|error| ConfinedFsError::os("fs::temp_write", &error))
                {
                    Ok(0) => {
                        return Err(ConfinedFsError::os(
                            "fs::temp_write",
                            &io::Error::new(
                                io::ErrorKind::WriteZero,
                                "temporary write made no progress",
                            ),
                        ));
                    }
                    Ok(written) => {
                        offset += written;
                        self.written += written;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = data;
            Err(unsupported_error("fs::temp_write"))
        }
    }

    /// Flushes buffered data to the operating-system file descriptor.
    pub fn flush(&mut self) -> Result<(), ConfinedFsError> {
        #[cfg(unix)]
        {
            if self.completed {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::TempCompleted,
                    "fs::temp_flush",
                    "temporary file has already been completed",
                ));
            }
            self.file
                .flush()
                .map_err(|error| ConfinedFsError::os("fs::temp_flush", &error))
        }
        #[cfg(not(unix))]
        {
            Err(unsupported_error("fs::temp_flush"))
        }
    }

    /// Requests synchronization of the temporary file's contents.
    pub fn sync_all(&self) -> Result<(), ConfinedFsError> {
        #[cfg(unix)]
        {
            if self.completed {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::TempCompleted,
                    "fs::temp_sync",
                    "temporary file has already been completed",
                ));
            }
            self.file
                .sync_all()
                .map_err(|error| ConfinedFsError::os("fs::temp_sync", &error))
        }
        #[cfg(not(unix))]
        {
            Err(unsupported_error("fs::temp_sync"))
        }
    }

    /// Unlinks the temporary file relative to its retained parent directory.
    ///
    /// Cleanup is idempotent. Dropping an uncommitted temporary also attempts
    /// this operation, while suppressing errors because `Drop` cannot report
    /// them.
    pub fn cleanup(&mut self) -> Result<(), ConfinedFsError> {
        #[cfg(unix)]
        {
            if self.completed {
                return Ok(());
            }
            let metadata = match unix::metadata_at(self.parent.as_raw_fd(), self.name.as_bytes()) {
                Ok(metadata) => metadata,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    self.completed = true;
                    return Ok(());
                }
                Err(error) => return Err(ConfinedFsError::os("fs::temp_cleanup", &error)),
            };
            let identity =
                match unix::file_identity_at(self.parent.as_raw_fd(), self.name.as_bytes()) {
                    Ok(identity) => identity,
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                        self.completed = true;
                        return Ok(());
                    }
                    Err(error) => return Err(ConfinedFsError::os("fs::temp_cleanup", &error)),
                };
            if identity != self.initial_identity
                || !metadata.is_file()
                || metadata.link_count() != 1
            {
                self.completed = true;
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    "fs::temp_cleanup",
                    "temporary cleanup entry is not the retained inode",
                ));
            }
            match unix::unlink_at(self.parent.as_raw_fd(), self.name.as_bytes()) {
                Ok(()) => {
                    self.completed = true;
                    Ok(())
                }
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    self.completed = true;
                    Ok(())
                }
                Err(error) => Err(ConfinedFsError::os("fs::temp_cleanup", &error)),
            }
        }
        #[cfg(not(unix))]
        {
            Err(unsupported_error("fs::temp_cleanup"))
        }
    }

    /// Replaces a same-directory destination atomically.
    ///
    /// Atomic publication is Linux-only (`renameat2`). The destination must be
    /// one basename, so the operation cannot select a second parent directory.
    /// The destination is checked without following symlinks, the retained
    /// source inode is linked to a private same-directory staging name, and
    /// only that staging name is published. Synchronization of the file is
    /// performed before publication. Once the destination name contains the
    /// retained inode, this method returns [`ConfinedPublication`] recording
    /// whether parent-directory durability and staging cleanup succeeded.
    pub fn replace(&mut self, destination: &str) -> Result<ConfinedPublication, ConfinedFsError> {
        let destination = validate_component(destination, "fs::replace")?;
        #[cfg(unix)]
        {
            if self.completed {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::TempCompleted,
                    "fs::replace",
                    "temporary file has already been completed",
                ));
            }
            unix::replace_temp(self, destination)
        }
        #[cfg(not(unix))]
        {
            let _ = destination;
            Err(unsupported_error("fs::replace"))
        }
    }
}

impl Drop for ConfinedTempFile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// A capability rooted at one existing directory.
///
/// Construction opens and retains the directory itself. Every operation after
/// construction uses only that retained descriptor and validated relative
/// components; it does not depend on the process working directory and never
/// performs canonicalize-then-open. Parent and leaf symlinks are rejected for
/// opens, directory traversal, temporary creation, and replacement. Metadata
/// may report a leaf symlink as [`ConfinedFileType::Symlink`] without following
/// it.
///
/// On Linux, `openat2` is used with beneath/no-magic-link/no-symlink resolution
/// when available. The component-wise `openat` fallback retains the same
/// no-follow guarantees. Atomic temporary publication requires Linux
/// `renameat2` and is unavailable on other Unix targets. Windows and
/// unsupported targets return a typed unsupported error rather than using
/// path-based reparse-point-unsafe calls.
#[derive(Debug)]
pub struct ConfinedFsRoot {
    #[cfg(unix)]
    fd: OwnedFd,
    #[cfg(unix)]
    root_identity: unix::FileIdentity,
    #[cfg(unix)]
    binding: unix::RootBinding,
    limits: ConfinedFsLimits,
}

impl ConfinedFsRoot {
    /// Opens an existing directory as a retained root capability.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ConfinedFsError> {
        Self::with_limits(path, ConfinedFsLimits::default())
    }

    /// Opens an existing directory with explicit bounded-operation limits.
    pub fn with_limits(
        path: impl AsRef<Path>,
        limits: ConfinedFsLimits,
    ) -> Result<Self, ConfinedFsError> {
        let limits = limits.validate()?;
        let path = path.as_ref();
        #[cfg(unix)]
        {
            let opened = unix::open_root(path)?;
            Ok(Self {
                fd: opened.fd,
                root_identity: opened.root_identity,
                binding: opened.binding,
                limits,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            let _ = limits;
            Err(unsupported_error("fs::root"))
        }
    }

    /// Returns the limits retained by this capability.
    pub fn limits(&self) -> ConfinedFsLimits {
        self.limits
    }

    #[cfg(unix)]
    fn ensure_bound(&self, operation: &'static str) -> Result<(), ConfinedFsError> {
        unix::verify_root_binding(&self.binding, self.root_identity).map_err(|error| {
            if matches!(
                error.raw_os_error(),
                Some(libc::ESTALE) | Some(libc::ENOENT) | Some(libc::ENOTDIR)
            ) {
                ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    operation,
                    "the retained root path entry changed",
                )
            } else {
                ConfinedFsError::os(operation, &error)
            }
        })
    }

    /// Opens a regular file read-only relative to the retained root.
    pub fn open_read(&self, path: &str) -> Result<ConfinedFile, ConfinedFsError> {
        let path = validate_relative_path(path, "fs::open_read")?;
        #[cfg(unix)]
        {
            self.ensure_bound("fs::open_read")?;
            let fd = unix::open_relative(
                self.fd.as_raw_fd(),
                &path.components,
                libc::O_RDONLY | libc::O_NONBLOCK,
                0,
            )
            .map_err(|error| ConfinedFsError::os("fs::open_read", &error))?;
            let file = File::from(fd);
            let metadata = unix::metadata_from_fd(file.as_raw_fd())
                .map_err(|error| ConfinedFsError::os("fs::open_read", &error))?;
            enforce_hardlink_policy("fs::open_read", metadata)?;
            if !metadata.is_file() {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::WrongType,
                    "fs::open_read",
                    "read-only open requires a regular file",
                ));
            }
            unix::clear_nonblock(file.as_raw_fd())
                .map_err(|error| ConfinedFsError::os("fs::open_read", &error))?;
            self.ensure_bound("fs::open_read")?;
            Ok(ConfinedFile {
                file,
                max_read_bytes: self.limits.max_read_bytes,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(unsupported_error("fs::open_read"))
        }
    }

    /// Reads one regular file under the root's configured byte budget.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, ConfinedFsError> {
        let mut file = self.open_read(path)?;
        file.read_to_end()
    }

    /// Reads metadata without following the leaf entry.
    pub fn metadata(&self, path: &str) -> Result<ConfinedMetadata, ConfinedFsError> {
        let path = validate_relative_path(path, "fs::metadata")?;
        #[cfg(unix)]
        {
            self.ensure_bound("fs::metadata")?;
            let parent = unix::open_directory(
                self.fd.as_raw_fd(),
                &path.components[..path.components.len() - 1],
            )
            .map_err(|error| ConfinedFsError::os("fs::metadata", &error))?;
            let leaf = path.components.last().expect("validated path is nonempty");
            let metadata = unix::metadata_at(parent.as_raw_fd(), leaf.as_bytes())
                .map_err(|error| ConfinedFsError::os("fs::metadata", &error))?;
            let metadata = enforce_hardlink_policy("fs::metadata", metadata)?;
            self.ensure_bound("fs::metadata")?;
            Ok(metadata)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(unsupported_error("fs::metadata"))
        }
    }

    /// Opens a directory relative to the retained root and keeps the handle.
    ///
    /// Passing an empty path selects the retained root itself. Traversal uses
    /// the same no-follow component walk and root-binding verification as other
    /// confined operations. The returned capability owns the directory handle
    /// and does not expose a path or raw descriptor.
    pub fn open_directory(&self, path: &str) -> Result<ConfinedDirectory, ConfinedFsError> {
        let path = validate_directory_path(path, "fs::open_directory")?;
        #[cfg(unix)]
        {
            self.ensure_bound("fs::open_directory")?;
            let fd = unix::open_directory(self.fd.as_raw_fd(), &path.components)
                .map_err(|error| ConfinedFsError::os("fs::open_directory", &error))?;
            self.ensure_bound("fs::open_directory")?;
            Ok(ConfinedDirectory::from_fd(fd))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(unsupported_error("fs::open_directory"))
        }
    }

    /// Enumerates a directory relative to the root with the default budget.
    ///
    /// Passing an empty directory path selects the retained root itself. Empty
    /// paths remain invalid for file operations.
    pub fn enumerate(&self, path: &str) -> Result<Vec<ConfinedDirEntry>, ConfinedFsError> {
        self.enumerate_with_budget(path, EnumerationBudget::default())
    }

    /// Enumerates a directory with a per-call budget further bounded by the
    /// root limits. An entry that would exceed the effective bound returns a
    /// typed budget error instead of silently returning an incomplete result.
    pub fn enumerate_with_budget(
        &self,
        path: &str,
        budget: EnumerationBudget,
    ) -> Result<Vec<ConfinedDirEntry>, ConfinedFsError> {
        let path = validate_directory_path(path, "fs::enumerate")?;
        let max_entries = budget.max_entries.min(self.limits.max_entries);
        let max_name_bytes = budget.max_name_bytes.min(self.limits.max_entry_name_bytes);
        #[cfg(unix)]
        {
            self.ensure_bound("fs::enumerate")?;
            let directory = unix::open_directory(self.fd.as_raw_fd(), &path.components)
                .map_err(|error| ConfinedFsError::os("fs::enumerate", &error))?;
            let entries = unix::enumerate_directory(directory, max_entries, max_name_bytes)?;
            self.ensure_bound("fs::enumerate")?;
            Ok(entries)
        }
        #[cfg(not(unix))]
        {
            let _ = (path, max_entries, max_name_bytes);
            Err(unsupported_error("fs::enumerate"))
        }
    }

    /// Returns whether this target can atomically publish temporary files.
    ///
    /// Publication uses Linux `renameat2`. Other Unix targets can still open a
    /// confined root for reads, metadata, and enumeration.
    pub const fn supports_atomic_publication() -> bool {
        publication_supported()
    }

    /// Creates an exclusive temporary regular file in a confined directory.
    ///
    /// An empty `parent` selects the retained root itself. The returned object
    /// retains the opened parent descriptor, and its basename is relative-only.
    /// Temporary creation is refused on targets that cannot publish with
    /// Linux `renameat2`, so an unpublished exclusive file cannot be left
    /// behind.
    pub fn create_temp(
        &self,
        parent: &str,
        prefix: &str,
    ) -> Result<ConfinedTempFile, ConfinedFsError> {
        let parent = validate_directory_path(parent, "fs::temp_create")?;
        let prefix = validate_component(prefix, "fs::temp_create")?;
        self.create_temp_in_components(&parent.components, prefix)
    }

    fn create_temp_in_components(
        &self,
        components: &[&str],
        prefix: &str,
    ) -> Result<ConfinedTempFile, ConfinedFsError> {
        if prefix.len() > MAX_TEMP_PREFIX_BYTES {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::ComponentTooLong,
                "fs::temp_create",
                "temporary prefix leaves insufficient room for a generated suffix",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            self.ensure_bound("fs::temp_create")?;
            let parent_fd = unix::open_directory(self.fd.as_raw_fd(), components)
                .map_err(|error| ConfinedFsError::os("fs::temp_create", &error))?;
            for _ in 0..self.limits.max_temp_attempts {
                let name = next_temp_name(prefix);
                match unix::create_exclusive_temp(
                    parent_fd.as_raw_fd(),
                    self.root_identity,
                    &name,
                    self.limits.max_write_bytes,
                ) {
                    Ok(mut file) => {
                        if let Err(error) = self.ensure_bound("fs::temp_create") {
                            let _ = file.cleanup();
                            return Err(error);
                        }
                        return Ok(file);
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
                    Err(error) => {
                        return Err(ConfinedFsError::os("fs::temp_create", &error));
                    }
                }
            }
            Err(ConfinedFsError::new(
                ConfinedFsErrorKind::TempCollision,
                "fs::temp_create",
                "temporary name retry budget exhausted",
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (self, components, prefix);
            Err(unsupported_error("fs::temp_create"))
        }
    }

    /// Atomically replaces a same-directory destination with `temp`.
    ///
    /// This is a Linux `renameat2` publication. Binding checks that can fail
    /// before the destination is published run first. After the destination
    /// contains the retained inode, durability and staging cleanup are
    /// reported on [`ConfinedPublication`].
    pub fn atomic_replace(
        &self,
        temp: ConfinedTempFile,
        destination: &str,
    ) -> Result<ConfinedPublication, ConfinedFsError> {
        let mut temp = temp;
        #[cfg(unix)]
        {
            self.ensure_bound("fs::replace")?;
            if temp.root_identity != self.root_identity {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::CapabilityMismatch,
                    "fs::replace",
                    "temporary file belongs to a different root capability",
                ));
            }
            temp.replace(destination)
        }
        #[cfg(not(unix))]
        temp.replace(destination)
    }

    /// Writes a regular file by creating a confined temporary and atomically
    /// replacing the destination in its exact parent directory.
    ///
    /// This convenience path requires Linux publication. On other targets it
    /// fails closed before creating a temporary.
    pub fn write_file(
        &self,
        path: &str,
        data: &[u8],
    ) -> Result<ConfinedPublication, ConfinedFsError> {
        let path = validate_relative_path(path, "fs::write_file")?;
        let parent = &path.components[..path.components.len() - 1];
        let destination = path.components.last().expect("validated path is nonempty");
        let mut temp = self.create_temp_in_components(parent, ".rustscript-tmp")?;
        temp.write_all(data)?;
        temp.flush()?;
        temp.sync_all()?;
        self.atomic_replace(temp, destination)
    }
}

#[cfg(unix)]
fn classify_os_error(raw_os_error: Option<i32>) -> ConfinedFsErrorKind {
    if let Some(raw_os_error) = raw_os_error {
        if raw_os_error == libc::ENOENT {
            return ConfinedFsErrorKind::NotFound;
        }
        if raw_os_error == libc::EACCES || raw_os_error == libc::EPERM {
            return ConfinedFsErrorKind::PermissionDenied;
        }
        if raw_os_error == libc::ELOOP {
            return ConfinedFsErrorKind::SymlinkDenied;
        }
        if raw_os_error == libc::ENOTDIR {
            return ConfinedFsErrorKind::WrongType;
        }
        if raw_os_error == libc::EEXIST {
            return ConfinedFsErrorKind::AlreadyExists;
        }
        if raw_os_error == libc::EOPNOTSUPP || raw_os_error == libc::ENOSYS {
            return ConfinedFsErrorKind::UnsupportedPlatform;
        }
    }
    ConfinedFsErrorKind::Io
}

#[cfg(unix)]
fn enforce_hardlink_policy(
    operation: &'static str,
    metadata: ConfinedMetadata,
) -> Result<ConfinedMetadata, ConfinedFsError> {
    if metadata.is_file() && metadata.link_count > 1 {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::HardlinkDenied,
            operation,
            "regular files with multiple hard links are not permitted",
        ));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn stat_u64<T>(value: T) -> u64
where
    T: TryInto<u64>,
{
    value.try_into().ok().unwrap_or(0)
}

fn unsupported_error(operation: &'static str) -> ConfinedFsError {
    ConfinedFsError::new(
        ConfinedFsErrorKind::UnsupportedPlatform,
        operation,
        "secure descriptor-relative filesystem operations are unavailable on this target",
    )
}

#[cfg(test)]
fn classify_readdir_end(errno: Option<i32>) -> ConfinedFsError {
    classify_readdir_end_or_eof(errno)
        .expect_err("a non-EOF readdir status must be reported as an error")
}

#[cfg(test)]
fn classify_readdir_end_or_eof(errno: Option<i32>) -> Result<(), ConfinedFsError> {
    match errno {
        Some(0) => Ok(()),
        Some(code) => {
            #[cfg(unix)]
            {
                Err(ConfinedFsError::os(
                    "fs::enumerate",
                    &io::Error::from_raw_os_error(code),
                ))
            }
            #[cfg(not(unix))]
            {
                let _ = code;
                Err(unsupported_error("fs::enumerate"))
            }
        }
        None => Err(unsupported_error("fs::enumerate")),
    }
}

struct ValidatedPath<'a> {
    components: Vec<&'a str>,
}

fn validate_relative_path<'a>(
    path: &'a str,
    operation: &'static str,
) -> Result<ValidatedPath<'a>, ConfinedFsError> {
    validate_path(path, false, operation)
}

fn validate_directory_path<'a>(
    path: &'a str,
    operation: &'static str,
) -> Result<ValidatedPath<'a>, ConfinedFsError> {
    validate_path(path, true, operation)
}

fn validate_path<'a>(
    path: &'a str,
    allow_empty: bool,
    operation: &'static str,
) -> Result<ValidatedPath<'a>, ConfinedFsError> {
    if path.is_empty() {
        if allow_empty {
            return Ok(ValidatedPath {
                components: Vec::new(),
            });
        }
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::EmptyPath,
            operation,
            "empty paths are not valid file paths",
        ));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::PathTooLong,
            operation,
            "relative path exceeds the hard bound",
        ));
    }
    if path.as_bytes().contains(&0) {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::NulByte,
            operation,
            "path contains a NUL byte",
        ));
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::AbsolutePath,
            operation,
            "rooted or trailing-separator paths are not permitted",
        ));
    }
    if path.contains('\\') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::InvalidSeparator,
            operation,
            "backslash is not a permitted path separator",
        ));
    }
    if path.contains(':') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::PathPrefix,
            operation,
            "drive and prefix syntax is not permitted",
        ));
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::InvalidPath,
                operation,
                "empty path components are not permitted",
            ));
        }
        if component == "." || component == ".." {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::ParentTraversal,
                operation,
                "dot and parent components are not permitted",
            ));
        }
        if component.ends_with('.') {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::InvalidPath,
                operation,
                "trailing-dot components are not permitted",
            ));
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::ComponentTooLong,
                operation,
                "path component exceeds the hard bound",
            ));
        }
        components.push(component);
    }
    Ok(ValidatedPath { components })
}

fn validate_component<'a>(
    component: &'a str,
    operation: &'static str,
) -> Result<&'a str, ConfinedFsError> {
    if component.is_empty() {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::EmptyPath,
            operation,
            "empty names are not permitted",
        ));
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::ComponentTooLong,
            operation,
            "name exceeds the hard bound",
        ));
    }
    if component == "." || component == ".." {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::ParentTraversal,
            operation,
            "dot and parent names are not permitted",
        ));
    }
    if component.ends_with('.') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::InvalidPath,
            operation,
            "trailing-dot names are not permitted",
        ));
    }
    if component.contains('/') || component.contains('\\') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::InvalidSeparator,
            operation,
            "path separators are not permitted in one name",
        ));
    }
    if component.contains(':') {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::PathPrefix,
            operation,
            "drive and prefix syntax is not permitted",
        ));
    }
    if component.as_bytes().contains(&0) {
        return Err(ConfinedFsError::new(
            ConfinedFsErrorKind::NulByte,
            operation,
            "name contains a NUL byte",
        ));
    }
    Ok(component)
}

#[cfg(unix)]
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn next_temp_name(prefix: &str) -> String {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    format!("{prefix}.{}.{}-{counter:x}", std::process::id(), nanos)
}

#[cfg(unix)]
mod unix {
    use super::*;
    #[cfg(test)]
    use std::cell::{Cell, RefCell};
    use std::mem::MaybeUninit;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct FileIdentity {
        pub(super) device: u64,
        pub(super) inode: u64,
    }

    #[derive(Debug)]
    pub(super) struct RootBinding {
        anchor: OwnedFd,
        components: Vec<Vec<u8>>,
        identities: Vec<FileIdentity>,
    }

    pub(super) struct OpenedRoot {
        pub(super) fd: OwnedFd,
        pub(super) root_identity: FileIdentity,
        pub(super) binding: RootBinding,
    }

    #[cfg(all(target_os = "linux", test))]
    thread_local! {
        static FORCE_OPENAT2_FALLBACK: Cell<bool> = const { Cell::new(false) };
    }

    #[cfg(all(target_os = "linux", test))]
    pub(super) fn set_force_openat2_fallback(force: bool) {
        FORCE_OPENAT2_FALLBACK.with(|flag| flag.set(force));
    }

    #[cfg(all(target_os = "linux", test))]
    pub(super) fn force_openat2_fallback_enabled() -> bool {
        FORCE_OPENAT2_FALLBACK.with(Cell::get)
    }

    #[cfg(target_os = "linux")]
    fn force_openat2_fallback() -> bool {
        #[cfg(test)]
        {
            FORCE_OPENAT2_FALLBACK.with(Cell::get)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    pub(super) fn open_root(path: &Path) -> Result<OpenedRoot, ConfinedFsError> {
        let (absolute, components) = parse_root_path(path)?;
        let anchor =
            open_anchor(absolute).map_err(|error| ConfinedFsError::os("fs::root", &error))?;
        let mut current = duplicate_fd(anchor.as_raw_fd())
            .map_err(|error| ConfinedFsError::os("fs::root", &error))?;
        let mut identities = Vec::with_capacity(components.len());
        for component in &components {
            let next = open_directory_component(current.as_raw_fd(), component)
                .map_err(|error| ConfinedFsError::os("fs::root", &error))?;
            let identity = file_identity(next.as_raw_fd())
                .map_err(|error| ConfinedFsError::os("fs::root", &error))?;
            identities.push(identity);
            current = next;
        }
        let root_identity = file_identity(current.as_raw_fd())
            .map_err(|error| ConfinedFsError::os("fs::root", &error))?;
        Ok(OpenedRoot {
            fd: current,
            root_identity,
            binding: RootBinding {
                anchor,
                components,
                identities,
            },
        })
    }

    fn parse_root_path(path: &Path) -> Result<(bool, Vec<Vec<u8>>), ConfinedFsError> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::EmptyPath,
                "fs::root",
                "empty paths are not valid roots",
            ));
        }
        if bytes.len() > MAX_PATH_BYTES {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::PathTooLong,
                "fs::root",
                "root path exceeds the hard bound",
            ));
        }
        if bytes.contains(&0) {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::NulByte,
                "fs::root",
                "root path contains a NUL byte",
            ));
        }
        if bytes.contains(&b'\\') {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::InvalidSeparator,
                "fs::root",
                "backslash is not a permitted path separator",
            ));
        }
        if bytes.contains(&b':') {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::PathPrefix,
                "fs::root",
                "drive and prefix syntax is not permitted",
            ));
        }

        let absolute = bytes[0] == b'/';
        if absolute && bytes.get(1) == Some(&b'/') {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::InvalidPath,
                "fs::root",
                "repeated leading separators are not permitted",
            ));
        }
        let mut body = if absolute { &bytes[1..] } else { bytes };
        if body.last() == Some(&b'/') {
            body = &body[..body.len() - 1];
        }
        if !absolute && body == b"." {
            return Ok((false, Vec::new()));
        }
        if body.is_empty() {
            return Ok((absolute, Vec::new()));
        }

        let mut components = Vec::new();
        for component in body.split(|byte| *byte == b'/') {
            if component.is_empty() {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::InvalidPath,
                    "fs::root",
                    "empty root components are not permitted",
                ));
            }
            if component == b"." || component == b".." {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::ParentTraversal,
                    "fs::root",
                    "dot and parent root components are not permitted",
                ));
            }
            if component.ends_with(b".") {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::InvalidPath,
                    "fs::root",
                    "trailing-dot root components are not permitted",
                ));
            }
            if component.len() > MAX_COMPONENT_BYTES {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::ComponentTooLong,
                    "fs::root",
                    "root component exceeds the hard bound",
                ));
            }
            components.push(component.to_vec());
        }
        Ok((absolute, components))
    }

    fn open_anchor(absolute: bool) -> Result<OwnedFd, io::Error> {
        let anchor = if absolute { "/" } else { "." };
        let anchor = CString::new(anchor).expect("fixed anchor contains no NUL");
        let fd = unsafe {
            libc::open(
                anchor.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn open_directory_component(parent_fd: RawFd, component: &[u8]) -> Result<OwnedFd, io::Error> {
        let component = CString::new(component).expect("validated component contains no NUL");
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let stat_result = unsafe {
            libc::fstatat(
                parent_fd,
                component.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_result < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if (stat.st_mode as libc::mode_t) & libc::S_IFMT == libc::S_IFLNK {
            return Err(io::Error::from_raw_os_error(libc::ELOOP));
        }
        let fd = unsafe {
            libc::openat(
                parent_fd,
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTDIR) {
                let mut stat = MaybeUninit::<libc::stat>::uninit();
                let result = unsafe {
                    libc::fstatat(
                        parent_fd,
                        component.as_ptr(),
                        stat.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if result == 0 {
                    let stat = unsafe { stat.assume_init() };
                    if (stat.st_mode as libc::mode_t) & libc::S_IFMT == libc::S_IFLNK {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                }
            }
            Err(error)
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    pub(super) fn verify_root_binding(
        binding: &RootBinding,
        root_identity: FileIdentity,
    ) -> Result<(), io::Error> {
        let mut current = duplicate_fd(binding.anchor.as_raw_fd())?;
        if binding.components.is_empty() {
            if file_identity(current.as_raw_fd())? != root_identity {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            return Ok(());
        }
        for (component, expected) in binding.components.iter().zip(&binding.identities) {
            let next = open_directory_component(current.as_raw_fd(), component)?;
            if file_identity(next.as_raw_fd())? != *expected {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            current = next;
        }
        if file_identity(current.as_raw_fd())? != root_identity {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(())
    }

    pub(super) fn open_directory(
        root_fd: RawFd,
        components: &[&str],
    ) -> Result<OwnedFd, io::Error> {
        open_relative(root_fd, components, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }

    pub(super) fn open_relative(
        root_fd: RawFd,
        components: &[&str],
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<OwnedFd, io::Error> {
        #[cfg(target_os = "linux")]
        {
            if !force_openat2_fallback() {
                match openat2(root_fd, components, flags, mode) {
                    Ok(fd) => return Ok(fd),
                    Err(error) if is_openat2_unavailable(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        open_component_walk(root_fd, components, flags, mode)
    }

    #[cfg(target_os = "linux")]
    fn is_openat2_unavailable(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        )
    }

    #[cfg(target_os = "linux")]
    fn openat2(
        root_fd: RawFd,
        components: &[&str],
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<OwnedFd, io::Error> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }

        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_NO_SYMLINKS: u64 = 0x04;
        const RESOLVE_BENEATH: u64 = 0x08;

        if components.is_empty() {
            return duplicate_fd(root_fd);
        }
        let mut relative_path = Vec::new();
        for (index, component) in components.iter().enumerate() {
            if index != 0 {
                relative_path.push(b'/');
            }
            relative_path.extend_from_slice(component.as_bytes());
        }
        let path = CString::new(relative_path).expect("validated components contain no NUL");
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: mode as u64,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root_fd,
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            ) as libc::c_int
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn open_component_walk(
        root_fd: RawFd,
        components: &[&str],
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<OwnedFd, io::Error> {
        if components.is_empty() {
            return duplicate_fd(root_fd);
        }
        let mut current = duplicate_fd(root_fd)?;
        for component in &components[..components.len() - 1] {
            current = open_directory_component(current.as_raw_fd(), component.as_bytes())?;
        }
        let leaf = CString::new(
            *components
                .last()
                .expect("nonempty component list has a last item"),
        )
        .expect("validated component contains no NUL");
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                leaf.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn duplicate_fd(fd: RawFd) -> Result<OwnedFd, io::Error> {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(duplicate) });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOSYS)
        ) {
            return Err(error);
        }
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor_flags = unsafe { libc::fcntl(duplicate, libc::F_GETFD) };
        if descriptor_flags < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(error);
        }
        if unsafe {
            libc::fcntl(
                duplicate,
                libc::F_SETFD,
                descriptor_flags | libc::FD_CLOEXEC,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(error);
        }
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }

    pub(super) fn clear_nonblock(fd: RawFd) -> Result<(), io::Error> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
            return Ok(());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn metadata_from_fd(fd: RawFd) -> Result<ConfinedMetadata, io::Error> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(metadata_from_stat(unsafe { stat.assume_init() }))
    }

    pub(super) fn metadata_at(
        directory_fd: RawFd,
        name: &[u8],
    ) -> Result<ConfinedMetadata, io::Error> {
        let name = CString::new(name).expect("validated component contains no NUL");
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory_fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(metadata_from_stat(unsafe { stat.assume_init() }))
    }

    fn metadata_from_stat(stat: libc::stat) -> ConfinedMetadata {
        let mode = stat.st_mode as libc::mode_t;
        let file_type = match mode & libc::S_IFMT {
            libc::S_IFREG => ConfinedFileType::File,
            libc::S_IFDIR => ConfinedFileType::Directory,
            libc::S_IFLNK => ConfinedFileType::Symlink,
            _ => ConfinedFileType::Other,
        };
        ConfinedMetadata {
            file_type,
            len: stat_u64(stat.st_size),
            link_count: stat_u64(stat.st_nlink),
        }
    }

    pub(super) fn create_exclusive_temp(
        parent_fd: RawFd,
        root_identity: FileIdentity,
        name: &str,
        max_write_bytes: usize,
    ) -> Result<ConfinedTempFile, io::Error> {
        let name_c = CString::new(name).expect("generated temporary name contains no NUL");
        let parent = duplicate_fd(parent_fd)?;
        let fd = unsafe {
            libc::openat(
                parent_fd,
                name_c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let initial_identity = match file_identity(file.as_raw_fd()) {
            Ok(identity) => identity,
            Err(error) => {
                drop(file);
                let _ = unlink_at(parent.as_raw_fd(), name.as_bytes());
                return Err(error);
            }
        };
        Ok(ConfinedTempFile {
            parent,
            root_identity,
            file,
            name: name.to_owned(),
            initial_identity,
            max_write_bytes,
            written: 0,
            completed: false,
        })
    }

    pub(super) fn file_identity(fd: RawFd) -> Result<FileIdentity, io::Error> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok(FileIdentity {
            device: stat_u64(stat.st_dev),
            inode: stat_u64(stat.st_ino),
        })
    }

    pub(super) fn replace_temp(
        temp: &mut ConfinedTempFile,
        destination: &str,
    ) -> Result<ConfinedPublication, ConfinedFsError> {
        if temp.completed {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::TempCompleted,
                "fs::replace",
                "temporary file has already been completed",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            replace_temp_linux(temp, destination)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (temp, destination);
            Err(unsupported_error("fs::replace"))
        }
    }

    #[cfg(target_os = "linux")]
    fn replace_temp_linux(
        temp: &mut ConfinedTempFile,
        destination: &str,
    ) -> Result<ConfinedPublication, ConfinedFsError> {
        temp.file
            .sync_all()
            .map_err(|error| map_unsupported("fs::replace", error))?;
        let current_identity = file_identity(temp.file.as_raw_fd())
            .map_err(|error| ConfinedFsError::os("fs::replace", &error))?;
        if current_identity != temp.initial_identity {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::RaceDetected,
                "fs::replace",
                "temporary file identity changed",
            ));
        }
        let source = match metadata_at(temp.parent.as_raw_fd(), temp.name.as_bytes()) {
            Ok(source) => source,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    "fs::replace",
                    "temporary source disappeared before replacement",
                ));
            }
            Err(error) => return Err(ConfinedFsError::os("fs::replace", &error)),
        };
        if !source.is_file() || source.link_count() != 1 {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::RaceDetected,
                "fs::replace",
                "temporary directory entry changed",
            ));
        }
        let source_identity = file_identity_at(temp.parent.as_raw_fd(), temp.name.as_bytes())
            .map_err(|error| ConfinedFsError::os("fs::replace", &error))?;
        if source_identity != current_identity {
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::RaceDetected,
                "fs::replace",
                "temporary directory entry was swapped",
            ));
        }
        #[cfg(test)]
        run_replace_test_hook(
            temp.parent.as_raw_fd(),
            temp.name.as_bytes(),
            destination.as_bytes(),
        );

        let destination_identity =
            match metadata_at(temp.parent.as_raw_fd(), destination.as_bytes()) {
                Ok(destination_metadata) => {
                    if destination_metadata.file_type == ConfinedFileType::Symlink {
                        return Err(ConfinedFsError::new(
                            ConfinedFsErrorKind::SymlinkDenied,
                            "fs::replace",
                            "destination symlinks are not permitted",
                        ));
                    }
                    if !destination_metadata.is_file() {
                        return Err(ConfinedFsError::new(
                            ConfinedFsErrorKind::WrongType,
                            "fs::replace",
                            "atomic replacement requires a regular-file destination",
                        ));
                    }
                    enforce_hardlink_policy("fs::replace", destination_metadata)?;
                    Some(
                        file_identity_at(temp.parent.as_raw_fd(), destination.as_bytes())
                            .map_err(|error| ConfinedFsError::os("fs::replace", &error))?,
                    )
                }
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => None,
                Err(error) => return Err(ConfinedFsError::os("fs::replace", &error)),
            };

        let publish_mode = match destination_identity {
            None => ReplacePublishMode::NoReplace,
            Some(expected_destination) => ReplacePublishMode::Exchange {
                expected_destination,
            },
        };

        let staging_name = link_temp_inode(temp, current_identity)?;
        let staging_bytes = staging_name.as_bytes();
        let source_identity_after_link =
            file_identity_at(temp.parent.as_raw_fd(), temp.name.as_bytes());
        if !matches!(
            source_identity_after_link,
            Ok(identity) if identity == current_identity
        ) {
            let _ = unlink_exact(temp.parent.as_raw_fd(), staging_bytes, current_identity);
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::RaceDetected,
                "fs::replace",
                "temporary source changed before publication",
            ));
        }
        if let Err(error) = unlink_exact(
            temp.parent.as_raw_fd(),
            temp.name.as_bytes(),
            current_identity,
        ) {
            let _ = unlink_exact(temp.parent.as_raw_fd(), staging_bytes, current_identity);
            if error.raw_os_error() == Some(libc::ESTALE)
                || error.raw_os_error() == Some(libc::ENOENT)
            {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    "fs::replace",
                    "temporary source changed before publication",
                ));
            }
            return Err(ConfinedFsError::os("fs::replace", &error));
        }

        #[cfg(test)]
        run_destination_replace_test_hook(temp.parent.as_raw_fd(), destination.as_bytes());
        let publish_result = rename_at2(
            temp.parent.as_raw_fd(),
            staging_bytes,
            temp.parent.as_raw_fd(),
            destination.as_bytes(),
            publish_mode.flags(),
        );
        if let Err(error) = publish_result {
            let _ = unlink_exact(temp.parent.as_raw_fd(), staging_bytes, current_identity);
            if error.raw_os_error() == Some(libc::EEXIST)
                || error.raw_os_error() == Some(libc::ENOENT)
            {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    "fs::replace",
                    "replacement destination changed before publication",
                ));
            }
            return Err(map_unsupported("fs::replace", error));
        }

        #[cfg(test)]
        run_post_rename_test_hook(
            temp.parent.as_raw_fd(),
            staging_bytes,
            destination.as_bytes(),
        );

        let destination_entry = observe_entry(temp.parent.as_raw_fd(), destination.as_bytes())
            .map_err(|error| ConfinedFsError::os("fs::replace", &error))?;
        let staging_entry = observe_entry(temp.parent.as_raw_fd(), staging_bytes)
            .map_err(|error| ConfinedFsError::os("fs::replace", &error))?;
        let dest_is_ours = destination_entry
            .as_ref()
            .is_some_and(|entry| entry.identity == current_identity && entry.metadata.is_file());
        if !dest_is_ours {
            cleanup_owned_staging_link(
                temp.parent.as_raw_fd(),
                staging_bytes,
                current_identity,
                publish_mode,
            );
            let _ = fsync_fd(temp.parent.as_raw_fd());
            return Err(ConfinedFsError::new(
                ConfinedFsErrorKind::RaceDetected,
                "fs::replace",
                "replacement destination does not contain the retained inode",
            ));
        }

        let staging_cleaned = match publish_mode {
            ReplacePublishMode::NoReplace => match staging_entry {
                None => true,
                Some(staging)
                    if staging.identity == current_identity && staging.metadata.is_file() =>
                {
                    unlink_exact(temp.parent.as_raw_fd(), staging_bytes, current_identity).is_ok()
                }
                Some(_) => false,
            },
            ReplacePublishMode::Exchange {
                expected_destination,
            } => match staging_entry {
                Some(staging)
                    if staging.identity == expected_destination && staging.metadata.is_file() =>
                {
                    match unlink_exact(temp.parent.as_raw_fd(), staging_bytes, expected_destination)
                    {
                        Ok(()) => true,
                        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => true,
                        Err(_) => false,
                    }
                }
                Some(staging) if staging.metadata.is_dir() => {
                    match reverse_directory_exchange(
                        temp.parent.as_raw_fd(),
                        staging_bytes,
                        destination.as_bytes(),
                        current_identity,
                        staging.identity,
                    ) {
                        ReverseDirectoryOutcome::DirectoryRestored => {
                            let _ = unlink_exact(
                                temp.parent.as_raw_fd(),
                                staging_bytes,
                                current_identity,
                            );
                            temp.completed = true;
                            return Err(ConfinedFsError::new(
                                ConfinedFsErrorKind::RaceDetected,
                                "fs::replace",
                                "replacement destination changed to a directory during publication",
                            ));
                        }
                        ReverseDirectoryOutcome::DestinationPublished => false,
                        ReverseDirectoryOutcome::Indeterminate {
                            destination: observed_destination,
                            staging: observed_staging,
                        } => {
                            temp.completed = true;
                            return Err(ConfinedFsError::new(
                                ConfinedFsErrorKind::RaceDetected,
                                "fs::replace",
                                "replacement publication raced into an indeterminate directory-exchange state",
                            )
                            .with_publication(ConfinedPublicationState::Indeterminate {
                                destination: observed_destination,
                                staging: observed_staging,
                            }));
                        }
                    }
                }
                _ => false,
            },
        };

        temp.completed = true;
        let durable = fsync_fd(temp.parent.as_raw_fd()).is_ok();
        Ok(ConfinedPublication::new(durable, staging_cleaned))
    }

    #[cfg(target_os = "linux")]
    struct ObservedEntry {
        metadata: ConfinedMetadata,
        identity: FileIdentity,
    }

    #[cfg(target_os = "linux")]
    fn observe_entry(parent_fd: RawFd, name: &[u8]) -> Result<Option<ObservedEntry>, io::Error> {
        match metadata_at(parent_fd, name) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(error) => Err(error),
            Ok(metadata) => Ok(Some(ObservedEntry {
                metadata,
                identity: file_identity_at(parent_fd, name)?,
            })),
        }
    }

    #[cfg(target_os = "linux")]
    enum ReverseDirectoryOutcome {
        DestinationPublished,
        DirectoryRestored,
        Indeterminate {
            destination: Option<ConfinedObservedIdentity>,
            staging: Option<ConfinedObservedIdentity>,
        },
    }

    #[cfg(target_os = "linux")]
    fn observed_identity(entry: &ObservedEntry) -> ConfinedObservedIdentity {
        ConfinedObservedIdentity::new(
            entry.metadata.file_type,
            entry.identity.device,
            entry.identity.inode,
        )
    }

    #[cfg(target_os = "linux")]
    fn reverse_directory_exchange(
        parent_fd: RawFd,
        staging: &[u8],
        destination: &[u8],
        published_file: FileIdentity,
        displaced_directory: FileIdentity,
    ) -> ReverseDirectoryOutcome {
        let pre_destination = observe_entry(parent_fd, destination).ok().flatten();
        let pre_staging = observe_entry(parent_fd, staging).ok().flatten();
        let can_reverse = pre_destination
            .as_ref()
            .is_some_and(|entry| entry.identity == published_file && entry.metadata.is_file())
            && pre_staging.as_ref().is_some_and(|entry| {
                entry.identity == displaced_directory && entry.metadata.is_dir()
            });
        if can_reverse {
            let fail_syscall = {
                #[cfg(test)]
                {
                    FORCE_REVERSE_EXCHANGE_FAIL.with(Cell::get)
                }
                #[cfg(not(test))]
                {
                    false
                }
            };
            if !fail_syscall {
                let _ = rename_at2(parent_fd, staging, parent_fd, destination, RENAME_EXCHANGE);
            }
            #[cfg(test)]
            {
                run_post_reverse_test_hook(parent_fd, staging, destination);
            }
        }

        let destination_entry = observe_entry(parent_fd, destination).ok().flatten();
        let staging_entry = observe_entry(parent_fd, staging).ok().flatten();
        if destination_entry
            .as_ref()
            .is_some_and(|entry| entry.identity == published_file && entry.metadata.is_file())
        {
            ReverseDirectoryOutcome::DestinationPublished
        } else if destination_entry
            .as_ref()
            .is_some_and(|entry| entry.identity == displaced_directory && entry.metadata.is_dir())
        {
            ReverseDirectoryOutcome::DirectoryRestored
        } else {
            ReverseDirectoryOutcome::Indeterminate {
                destination: destination_entry.as_ref().map(observed_identity),
                staging: staging_entry.as_ref().map(observed_identity),
            }
        }
    }

    #[cfg(target_os = "linux")]
    const RENAME_NOREPLACE: libc::c_uint = 1;

    #[cfg(target_os = "linux")]
    const RENAME_EXCHANGE: libc::c_uint = 2;

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy)]
    enum ReplacePublishMode {
        NoReplace,
        Exchange { expected_destination: FileIdentity },
    }

    #[cfg(target_os = "linux")]
    impl ReplacePublishMode {
        fn flags(self) -> libc::c_uint {
            match self {
                Self::NoReplace => RENAME_NOREPLACE,
                Self::Exchange { .. } => RENAME_EXCHANGE,
            }
        }

        fn expected_old_destination(self) -> Option<FileIdentity> {
            match self {
                Self::NoReplace => None,
                Self::Exchange {
                    expected_destination,
                } => Some(expected_destination),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn cleanup_owned_staging_link(
        parent_fd: RawFd,
        staging: &[u8],
        retained: FileIdentity,
        mode: ReplacePublishMode,
    ) {
        match unlink_exact(parent_fd, staging, retained) {
            Ok(()) => return,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return,
            Err(_) => {}
        }
        if let Some(expected_old_destination) = mode.expected_old_destination() {
            let _ = unlink_exact(parent_fd, staging, expected_old_destination);
        }
    }

    #[cfg(target_os = "linux")]
    fn link_temp_inode(
        temp: &ConfinedTempFile,
        expected: FileIdentity,
    ) -> Result<String, ConfinedFsError> {
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let name = next_temp_name(".rustscript-publish");
            let source_c =
                CString::new(temp.name.as_bytes()).expect("generated name contains no NUL");
            let name_c = CString::new(name.as_bytes()).expect("generated name contains no NUL");
            let result = unsafe {
                libc::linkat(
                    temp.parent.as_raw_fd(),
                    source_c.as_ptr(),
                    temp.parent.as_raw_fd(),
                    name_c.as_ptr(),
                    0,
                )
            };
            if result == 0 {
                #[cfg(test)]
                run_staging_link_test_hook(temp.parent.as_raw_fd(), name.as_bytes());
                match file_identity_at(temp.parent.as_raw_fd(), name.as_bytes()) {
                    Ok(identity) if identity == expected => return Ok(name),
                    _ => {
                        let _ = unlink_exact(temp.parent.as_raw_fd(), name.as_bytes(), expected);
                        return Err(ConfinedFsError::new(
                            ConfinedFsErrorKind::RaceDetected,
                            "fs::replace",
                            "publication staging inode changed",
                        ));
                    }
                }
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Err(ConfinedFsError::new(
                    ConfinedFsErrorKind::RaceDetected,
                    "fs::replace",
                    "temporary source disappeared during staging",
                ));
            }
            return Err(map_unsupported("fs::replace", error));
        }
        Err(ConfinedFsError::new(
            ConfinedFsErrorKind::TempCollision,
            "fs::replace",
            "publication staging name retry budget exhausted",
        ))
    }

    #[cfg(target_os = "linux")]
    fn rename_at2(
        parent_fd: RawFd,
        source: &[u8],
        destination_parent_fd: RawFd,
        destination: &[u8],
        flags: libc::c_uint,
    ) -> Result<(), io::Error> {
        let source = CString::new(source).expect("validated name contains no NUL");
        let destination = CString::new(destination).expect("validated name contains no NUL");
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent_fd,
                source.as_ptr(),
                destination_parent_fd,
                destination.as_ptr(),
                flags,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn unlink_exact(
        parent_fd: RawFd,
        name: &[u8],
        expected: FileIdentity,
    ) -> Result<(), io::Error> {
        let metadata = metadata_at(parent_fd, name)?;
        if !metadata.is_file() || file_identity_at(parent_fd, name)? != expected {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        unlink_at(parent_fd, name)
    }

    #[cfg(target_os = "linux")]
    fn fsync_fd(fd: RawFd) -> Result<(), io::Error> {
        #[cfg(test)]
        if FORCE_FSYNC_FAIL.with(Cell::get) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        if unsafe { libc::fsync(fd) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(super) type ReplaceTestHook = fn(RawFd, &[u8], &[u8]);
    #[cfg(test)]
    pub(super) type DestinationReplaceTestHook = fn(RawFd, &[u8]);
    #[cfg(test)]
    pub(super) type StagingLinkTestHook = fn(RawFd, &[u8]);
    #[cfg(test)]
    pub(super) type PostRenameTestHook = fn(RawFd, &[u8], &[u8]);
    #[cfg(test)]
    pub(super) type PostReverseTestHook = fn(RawFd, &[u8], &[u8]);

    #[cfg(test)]
    thread_local! {
        static REPLACE_TEST_HOOK: Cell<Option<ReplaceTestHook>> = const { Cell::new(None) };
        static DESTINATION_REPLACE_TEST_HOOK: Cell<Option<DestinationReplaceTestHook>> =
            const { Cell::new(None) };
        static STAGING_LINK_TEST_HOOK: Cell<Option<StagingLinkTestHook>> = const { Cell::new(None) };
        static POST_RENAME_TEST_HOOK: Cell<Option<PostRenameTestHook>> = const { Cell::new(None) };
        static POST_REVERSE_TEST_HOOK: Cell<Option<PostReverseTestHook>> = const { Cell::new(None) };
        static STAGING_LINK_CAPTURE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
        static FORCE_FSYNC_FAIL: Cell<bool> = const { Cell::new(false) };
        static FORCE_REVERSE_EXCHANGE_FAIL: Cell<bool> = const { Cell::new(false) };
    }

    #[cfg(test)]
    static REPLACE_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    #[cfg(test)]
    pub(super) fn set_replace_test_hook(hook: Option<ReplaceTestHook>) {
        REPLACE_TEST_HOOK.with(|slot| slot.set(hook));
    }

    #[cfg(test)]
    pub(super) fn set_destination_replace_test_hook(hook: Option<DestinationReplaceTestHook>) {
        DESTINATION_REPLACE_TEST_HOOK.with(|slot| slot.set(hook));
    }

    #[cfg(test)]
    pub(super) fn set_staging_link_test_hook(hook: Option<StagingLinkTestHook>) {
        STAGING_LINK_TEST_HOOK.with(|slot| slot.set(hook));
    }

    #[cfg(test)]
    pub(super) fn set_post_rename_test_hook(hook: Option<PostRenameTestHook>) {
        POST_RENAME_TEST_HOOK.with(|slot| slot.set(hook));
    }

    #[cfg(test)]
    pub(super) fn replace_test_lock() -> std::sync::MutexGuard<'static, ()> {
        REPLACE_TEST_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn take_staging_link_capture() -> Option<Vec<u8>> {
        STAGING_LINK_CAPTURE.with(|slot| slot.borrow_mut().take())
    }

    #[cfg(test)]
    pub(super) fn capture_staging_link_name(name: &[u8]) {
        STAGING_LINK_CAPTURE.with(|slot| *slot.borrow_mut() = Some(name.to_vec()));
    }

    #[cfg(test)]
    pub(super) fn destination_replace_test_hook_installed() -> Option<DestinationReplaceTestHook> {
        DESTINATION_REPLACE_TEST_HOOK.with(Cell::get)
    }

    #[cfg(test)]
    pub(super) fn replace_test_hook_installed() -> Option<ReplaceTestHook> {
        REPLACE_TEST_HOOK.with(Cell::get)
    }

    #[cfg(test)]
    pub(super) fn staging_link_test_hook_installed() -> Option<StagingLinkTestHook> {
        STAGING_LINK_TEST_HOOK.with(Cell::get)
    }

    #[cfg(test)]
    pub(super) fn post_rename_test_hook_installed() -> Option<PostRenameTestHook> {
        POST_RENAME_TEST_HOOK.with(Cell::get)
    }

    #[cfg(test)]
    pub(super) struct ReplaceHookGuard {
        previous: Option<ReplaceTestHook>,
    }

    #[cfg(test)]
    impl ReplaceHookGuard {
        pub(super) fn new(hook: ReplaceTestHook) -> Self {
            let previous = REPLACE_TEST_HOOK.with(|slot| slot.replace(Some(hook)));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for ReplaceHookGuard {
        fn drop(&mut self) {
            set_replace_test_hook(self.previous);
        }
    }

    #[cfg(test)]
    pub(super) struct StagingLinkHookGuard {
        previous: Option<StagingLinkTestHook>,
    }

    #[cfg(test)]
    impl StagingLinkHookGuard {
        pub(super) fn new(hook: StagingLinkTestHook) -> Self {
            let previous = STAGING_LINK_TEST_HOOK.with(|slot| slot.replace(Some(hook)));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for StagingLinkHookGuard {
        fn drop(&mut self) {
            set_staging_link_test_hook(self.previous);
        }
    }

    #[cfg(test)]
    pub(super) struct PostRenameHookGuard {
        previous: Option<PostRenameTestHook>,
    }

    #[cfg(test)]
    impl PostRenameHookGuard {
        pub(super) fn new(hook: PostRenameTestHook) -> Self {
            let previous = POST_RENAME_TEST_HOOK.with(|slot| slot.replace(Some(hook)));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for PostRenameHookGuard {
        fn drop(&mut self) {
            set_post_rename_test_hook(self.previous);
        }
    }

    #[cfg(test)]
    pub(super) struct DestinationReplaceHookGuard {
        previous: Option<DestinationReplaceTestHook>,
    }

    #[cfg(test)]
    impl DestinationReplaceHookGuard {
        pub(super) fn new(hook: DestinationReplaceTestHook) -> Self {
            let previous = DESTINATION_REPLACE_TEST_HOOK.with(|slot| slot.replace(Some(hook)));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for DestinationReplaceHookGuard {
        fn drop(&mut self) {
            set_destination_replace_test_hook(self.previous);
        }
    }

    #[cfg(test)]
    pub(super) fn force_fsync_fail_enabled() -> bool {
        FORCE_FSYNC_FAIL.with(Cell::get)
    }

    #[cfg(test)]
    pub(super) struct ForceFsyncFailGuard {
        previous: bool,
    }

    #[cfg(test)]
    impl ForceFsyncFailGuard {
        pub(super) fn new() -> Self {
            let previous = FORCE_FSYNC_FAIL.with(|flag| flag.replace(true));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for ForceFsyncFailGuard {
        fn drop(&mut self) {
            FORCE_FSYNC_FAIL.with(|flag| flag.set(self.previous));
        }
    }

    #[cfg(test)]
    pub(super) struct ReverseExchangeFailGuard {
        previous: bool,
    }

    #[cfg(test)]
    impl ReverseExchangeFailGuard {
        pub(super) fn new() -> Self {
            let previous = FORCE_REVERSE_EXCHANGE_FAIL.with(|flag| flag.replace(true));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for ReverseExchangeFailGuard {
        fn drop(&mut self) {
            FORCE_REVERSE_EXCHANGE_FAIL.with(|flag| flag.set(self.previous));
        }
    }

    #[cfg(test)]
    pub(super) struct PostReverseHookGuard {
        previous: Option<PostReverseTestHook>,
    }

    #[cfg(test)]
    impl PostReverseHookGuard {
        pub(super) fn new(hook: PostReverseTestHook) -> Self {
            let previous = POST_REVERSE_TEST_HOOK.with(|slot| slot.replace(Some(hook)));
            Self { previous }
        }
    }

    #[cfg(test)]
    impl Drop for PostReverseHookGuard {
        fn drop(&mut self) {
            POST_REVERSE_TEST_HOOK.with(|slot| slot.set(self.previous));
        }
    }

    #[cfg(test)]
    fn run_replace_test_hook(parent_fd: RawFd, source: &[u8], destination: &[u8]) {
        if let Some(hook) = REPLACE_TEST_HOOK.with(Cell::take) {
            hook(parent_fd, source, destination);
        }
    }

    #[cfg(test)]
    fn run_destination_replace_test_hook(parent_fd: RawFd, destination: &[u8]) {
        if let Some(hook) = DESTINATION_REPLACE_TEST_HOOK.with(Cell::take) {
            hook(parent_fd, destination);
        }
    }

    #[cfg(test)]
    fn run_staging_link_test_hook(parent_fd: RawFd, staging: &[u8]) {
        if let Some(hook) = STAGING_LINK_TEST_HOOK.with(Cell::take) {
            hook(parent_fd, staging);
        }
    }

    #[cfg(test)]
    fn run_post_rename_test_hook(parent_fd: RawFd, staging: &[u8], destination: &[u8]) {
        if let Some(hook) = POST_RENAME_TEST_HOOK.with(Cell::take) {
            hook(parent_fd, staging, destination);
        }
    }

    #[cfg(test)]
    fn run_post_reverse_test_hook(parent_fd: RawFd, staging: &[u8], destination: &[u8]) {
        if let Some(hook) = POST_REVERSE_TEST_HOOK.with(Cell::take) {
            hook(parent_fd, staging, destination);
        }
    }

    #[cfg(target_os = "linux")]
    fn map_unsupported(operation: &'static str, error: io::Error) -> ConfinedFsError {
        if error.raw_os_error().is_some_and(|code| {
            code == libc::ENOSYS
                || code == libc::EINVAL
                || code == libc::EOPNOTSUPP
                || code == libc::ENOTSUP
        }) {
            unsupported_error(operation)
        } else {
            ConfinedFsError::os(operation, &error)
        }
    }

    pub(super) fn file_identity_at(
        directory_fd: RawFd,
        name: &[u8],
    ) -> Result<FileIdentity, io::Error> {
        let name = CString::new(name).expect("validated component contains no NUL");
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory_fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok(FileIdentity {
            device: stat_u64(stat.st_dev),
            inode: stat_u64(stat.st_ino),
        })
    }

    pub(super) fn unlink_at(parent_fd: RawFd, name: &[u8]) -> Result<(), io::Error> {
        let name = CString::new(name).expect("validated component contains no NUL");
        let result = unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn enumerate_directory(
        directory: OwnedFd,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<Vec<ConfinedDirEntry>, ConfinedFsError> {
        let raw_directory = directory.into_raw_fd();
        let stream = unsafe { libc::fdopendir(raw_directory) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(raw_directory) };
            return Err(ConfinedFsError::os("fs::enumerate", &error));
        }
        let guard = DirGuard(stream);
        let mut entries = Vec::with_capacity(max_entries.min(64));
        let directory_fd = guard_fd(guard.0);
        if directory_fd < 0 {
            return Err(ConfinedFsError::os(
                "fs::enumerate",
                &io::Error::last_os_error(),
            ));
        }
        let mut examined = 0usize;
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(guard.0) };
            if entry.is_null() {
                match errno_abi() {
                    ErrnoAbi::Known(0) => break,
                    ErrnoAbi::Known(code) => {
                        return Err(ConfinedFsError::os(
                            "fs::enumerate",
                            &io::Error::from_raw_os_error(code),
                        ));
                    }
                    ErrnoAbi::Unsupported => {
                        return Err(unsupported_error("fs::enumerate"));
                    }
                }
            }
            examined = examined.saturating_add(1);
            if examined > max_entries {
                return Err(ConfinedFsError::budget(
                    "fs::enumerate",
                    "directory entry examination budget exceeded",
                    max_entries,
                    examined,
                ));
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let name_bytes = name.to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            if name_bytes.len() > max_name_bytes {
                return Err(ConfinedFsError::budget(
                    "fs::enumerate",
                    "directory entry name budget exceeded",
                    max_name_bytes,
                    name_bytes.len(),
                ));
            }
            let metadata = match metadata_at(directory_fd, name_bytes) {
                Ok(metadata) => metadata,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(error) => return Err(ConfinedFsError::os("fs::enumerate", &error)),
            };
            let metadata = enforce_hardlink_policy("fs::enumerate", metadata)?;
            let name_os = OsString::from_vec(name_bytes.to_vec());
            entries.push(ConfinedDirEntry {
                name: String::from_utf8_lossy(name_bytes).into_owned(),
                name_os,
                metadata,
            });
        }
        Ok(entries)
    }

    fn guard_fd(stream: *mut libc::DIR) -> RawFd {
        unsafe { libc::dirfd(stream) }
    }

    fn clear_errno() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "ios",
            target_os = "macos",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        unsafe {
            *libc::__error() = 0;
        }
    }

    enum ErrnoAbi {
        Known(i32),
        #[allow(dead_code)]
        Unsupported,
    }

    fn errno_abi() -> ErrnoAbi {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            ErrnoAbi::Known(unsafe { *libc::__errno_location() })
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "ios",
            target_os = "macos",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            ErrnoAbi::Known(unsafe { *libc::__error() })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "ios",
            target_os = "macos",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            ErrnoAbi::Unsupported
        }
    }

    struct DirGuard(*mut libc::DIR);

    impl Drop for DirGuard {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn exclusive_temp_creation_does_not_overwrite_a_collision() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-unit-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let parent = unix::open_directory(root.fd.as_raw_fd(), &[]).expect("parent should open");
        let name = "fixed-collision-name";
        let mut first = unix::create_exclusive_temp(
            parent.as_raw_fd(),
            root.root_identity,
            name,
            ConfinedFsLimits::default().max_write_bytes,
        )
        .expect("first exclusive create should work");
        first
            .write_all(b"original")
            .expect("fixture should be written");
        let error = unix::create_exclusive_temp(
            parent.as_raw_fd(),
            root.root_identity,
            name,
            ConfinedFsLimits::default().max_write_bytes,
        )
        .expect_err("second exclusive create must report the collision");
        assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
        first.cleanup().expect("first temporary should be cleaned");
        drop(parent);
        drop(root);
        std::fs::remove_dir(&path).expect("temporary directory should be empty");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn partial_temporary_writes_account_for_bytes_before_a_later_error() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-partial-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "partial")
            .expect("temporary file should be created");

        let _partial_write = PartialWriteFailGuard::new(3);
        let result = temp.write_all(b"abcdef");

        let error = result.expect_err("the deterministic write hook should fail after progress");
        assert_eq!(error.kind(), ConfinedFsErrorKind::Io);
        assert_eq!(temp.bytes_written(), 3);
        temp.cleanup().expect("temporary file should be cleaned");
        drop(root);
        std::fs::remove_dir(&path).expect("temporary directory should be empty");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn forced_component_walk_keeps_descriptors_close_on_exec() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-fallback-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("file"), b"fallback").expect("fixture should be written");

        let _fallback_guard = ForceFallbackGuard::new();
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        assert_descriptor_is_cloexec(root.fd.as_raw_fd());
        let mut file = root
            .open_read("file")
            .expect("fallback open should read the file");
        assert_descriptor_is_cloexec(file.file.as_raw_fd());
        assert_descriptor_is_blocking(file.file.as_raw_fd());
        assert_eq!(
            file.read_to_end().expect("file should be readable"),
            b"fallback"
        );
        let mut temp = root
            .create_temp("", "fallback")
            .expect("fallback temp creation should work");
        assert_descriptor_is_cloexec(temp.parent.as_raw_fd());
        assert_descriptor_is_cloexec(temp.file.as_raw_fd());
        temp.cleanup().expect("temporary file should be cleaned");
        drop(file);
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    struct ForceFallbackGuard {
        previous: bool,
    }

    #[cfg(all(unix, target_os = "linux"))]
    impl ForceFallbackGuard {
        fn new() -> Self {
            let previous = unix::force_openat2_fallback_enabled();
            unix::set_force_openat2_fallback(true);
            Self { previous }
        }
    }

    #[cfg(all(unix, target_os = "linux"))]
    impl Drop for ForceFallbackGuard {
        fn drop(&mut self) {
            unix::set_force_openat2_fallback(self.previous);
        }
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn assert_descriptor_is_cloexec(fd: RawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "descriptor flags should be readable");
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "descriptor must be close-on-exec"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn assert_descriptor_is_blocking(fd: RawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "status flags should be readable");
        assert_eq!(
            flags & libc::O_NONBLOCK,
            0,
            "descriptor must be blocking after type validation"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn rustscript_publish_names(dir: &std::path::Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .expect("test directory should be readable")
            .map(|entry| {
                entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".rustscript-publish."))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn names_for_inode(dir: &std::path::Path, inode: u64) -> Vec<String> {
        use std::os::unix::fs::MetadataExt;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir).expect("test directory should be readable") {
            let entry = entry.expect("directory entry should be readable");
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.ino() == inode {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        names
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn swap_victim_onto_staging_name(parent: RawFd, staging: &[u8]) {
        unix::capture_staging_link_name(staging);
        let victim = CString::new("victim").expect("fixed victim name is valid");
        let staging = CString::new(staging).expect("staging name has no NUL");
        let result = unsafe { libc::renameat(parent, victim.as_ptr(), parent, staging.as_ptr()) };
        assert_eq!(result, 0, "victim should replace the staging name");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn staging_link_identity_mismatch_leaves_victim_and_does_not_leak_retained_inode() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;

        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-staging-swap-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        std::fs::write(path.join("victim"), b"keep-me").expect("victim should be written");
        let victim_inode = std::fs::symlink_metadata(path.join("victim"))
            .expect("victim metadata should be readable")
            .ino();
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "staging-swap")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let temp_name = temp.name().to_owned();
        let retained_inode = std::fs::symlink_metadata(path.join(&temp_name))
            .expect("temporary metadata should be readable")
            .ino();
        let _test_guard = unix::replace_test_lock();
        let _ = unix::take_staging_link_capture();
        let _staging_hook = unix::StagingLinkHookGuard::new(swap_victim_onto_staging_name);
        let result = temp.replace("destination");
        let error = result.expect_err("staging identity mismatch must be a typed conflict");
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        let staging_name =
            unix::take_staging_link_capture().expect("staging name should be captured");
        let staging_path = path.join(Path::new(std::ffi::OsStr::from_bytes(&staging_name)));
        assert_eq!(
            std::fs::read(&staging_path).expect("victim must survive at the staging name"),
            b"keep-me"
        );
        assert_eq!(
            std::fs::symlink_metadata(&staging_path)
                .expect("surviving victim metadata should be readable")
                .ino(),
            victim_inode
        );
        assert_eq!(
            std::fs::read(path.join(&temp_name)).expect("retained temporary must remain"),
            b"new"
        );
        assert_eq!(
            names_for_inode(&path, retained_inode),
            vec![temp_name.clone()],
            "authorized staging inode must not leak to another directory entry"
        );
        assert_eq!(
            rustscript_publish_names(&path),
            vec![String::from_utf8(staging_name).expect("generated staging name is UTF-8")]
        );
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"old");
        assert!(!path.join("victim").exists());
        temp.cleanup()
            .expect("retained temporary should still clean up at its original name");
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn rename_source_before_staging(parent: RawFd, source: &[u8], _destination: &[u8]) {
        let moved = CString::new(".hook-moved-source").expect("fixed hook name is valid");
        let source = CString::new(source).expect("temporary source has no NUL");
        let result = unsafe { libc::renameat(parent, source.as_ptr(), parent, moved.as_ptr()) };
        assert_eq!(result, 0, "source hook should rename the original entry");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn source_publication_hook_rejects_a_rename_before_staging() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-source-hook-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "hook")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _replace_hook = unix::ReplaceHookGuard::new(rename_source_before_staging);
        let result = temp.replace("destination");
        let error = result.expect_err("the source rename hook must be rejected");
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(path.join(".hook-moved-source")).unwrap(),
            b"new"
        );
        temp.cleanup()
            .expect("renamed source has nothing to clean at its old name");
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn swap_destination_for_symlink(parent: RawFd, destination: &[u8]) {
        let old = CString::new(".hook-old-destination").expect("fixed hook name is valid");
        let destination = CString::new(destination).expect("destination has no NUL");
        let result = unsafe { libc::renameat(parent, destination.as_ptr(), parent, old.as_ptr()) };
        assert_eq!(result, 0, "destination hook should move the original entry");
        let target = CString::new("attacker-target").expect("fixed target is valid");
        let result = unsafe { libc::symlinkat(target.as_ptr(), parent, destination.as_ptr()) };
        assert_eq!(result, 0, "destination hook should install a symlink");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn destination_publication_hook_never_follows_a_symlink_swap() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-destination-hook-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "hook-destination")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _destination_hook =
            unix::DestinationReplaceHookGuard::new(swap_destination_for_symlink);
        let publication = temp
            .replace("destination")
            .expect("symlink swap after precheck must not look like a pre-publication failure");
        assert!(publication.is_published());
        assert!(
            !publication.staging_cleaned(),
            "unexpected symlink leftover at staging must not be unlinked as the old destination"
        );
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"new");
        assert!(!path.join("attacker-target").exists());
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn swap_destination_for_directory(parent: RawFd, destination: &[u8]) {
        let destination = CString::new(destination).expect("destination has no NUL");
        let unlinked = unsafe { libc::unlinkat(parent, destination.as_ptr(), 0) };
        assert_eq!(
            unlinked, 0,
            "destination file should be removed before mkdir"
        );
        let created = unsafe { libc::mkdirat(parent, destination.as_ptr(), 0o700) };
        assert_eq!(created, 0, "destination should become a directory");
        let dir_fd = unsafe {
            libc::openat(
                parent,
                destination.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        assert!(dir_fd >= 0, "new destination directory should open");
        write_regular_file_at(dir_fd, b"marker", b"keep-dir");
        let closed = unsafe { libc::close(dir_fd) };
        assert_eq!(closed, 0, "destination directory should close");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn replace_exchange_directory_swap_restores_directory_and_does_not_publish() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-dir-swap-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "dir-swap")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _destination_hook =
            unix::DestinationReplaceHookGuard::new(swap_destination_for_directory);
        let result = temp.replace("destination");
        let error = result.expect_err("exchanging with a directory must not stay published");
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert!(
            !error.publication_state().is_published(),
            "directory swap rollback must report unpublished state, got {error}"
        );
        assert!(
            path.join("destination").is_dir(),
            "directory must remain at the destination name"
        );
        assert_eq!(
            std::fs::read(path.join("destination/marker"))
                .expect("directory contents must survive rollback"),
            b"keep-dir"
        );
        assert!(
            rustscript_publish_names(&path).is_empty(),
            "directory must not remain renamed aside onto a staging name"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn unlink_staging_after_reverse(parent: RawFd, staging: &[u8], _destination: &[u8]) {
        let staging = CString::new(staging).expect("staging name has no NUL");
        let unlinked = unsafe { libc::unlinkat(parent, staging.as_ptr(), 0) };
        assert_eq!(unlinked, 0, "reversed staging file should unlink");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn reverse_directory_exchange_success_with_postcheck_failure_reports_not_published() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-dir-swap-postcheck-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "dir-swap-postcheck")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _destination_hook =
            unix::DestinationReplaceHookGuard::new(swap_destination_for_directory);
        let _post_reverse = unix::PostReverseHookGuard::new(unlink_staging_after_reverse);
        let result = temp.replace("destination");
        let error = result.expect_err(
            "a restored directory must not be reported as published after a postcheck error",
        );
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert!(
            !error.publication_state().is_published(),
            "destination directory must not be claimed published, got {error:?}"
        );
        assert!(
            !error.publication_state().is_indeterminate(),
            "a restored directory is unpublished, not indeterminate, got {error:?}"
        );
        assert!(
            path.join("destination").is_dir(),
            "directory must remain at the destination name"
        );
        assert_eq!(
            std::fs::read(path.join("destination/marker"))
                .expect("directory contents must survive rollback"),
            b"keep-dir"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn reverse_directory_exchange_syscall_failure_keeps_published_inode() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-dir-swap-syscall-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "dir-swap-syscall")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _destination_hook =
            unix::DestinationReplaceHookGuard::new(swap_destination_for_directory);
        let _reverse_fail = unix::ReverseExchangeFailGuard::new();
        let publication = temp
            .replace("destination")
            .expect("destination still holding the retained inode must be reported published");
        assert!(publication.is_published());
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"new");
        let staging_names = rustscript_publish_names(&path);
        assert_eq!(
            staging_names.len(),
            1,
            "displaced directory must remain at the staging name"
        );
        assert!(
            path.join(&staging_names[0]).is_dir(),
            "directory must be preserved at staging after a reverse syscall failure"
        );
        assert_eq!(
            std::fs::read(path.join(&staging_names[0]).join("marker"))
                .expect("directory contents must survive at staging"),
            b"keep-dir"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn swap_destination_after_reverse(parent: RawFd, _staging: &[u8], destination: &[u8]) {
        let moved = CString::new(".hook-restored-dir").expect("fixed hook name is valid");
        let destination = CString::new(destination).expect("destination has no NUL");
        let result =
            unsafe { libc::renameat(parent, destination.as_ptr(), parent, moved.as_ptr()) };
        assert_eq!(result, 0, "restored directory should be moved aside");
        write_regular_file_at(parent, b"destination", b"victim");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn reverse_directory_exchange_concurrent_swap_reports_indeterminate() {
        use std::os::unix::fs::MetadataExt;

        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-dir-swap-indeterminate-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "dir-swap-indeterminate")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let retained_inode = std::fs::symlink_metadata(path.join(temp.name()))
            .expect("temporary metadata should be readable")
            .ino();
        let _test_guard = unix::replace_test_lock();
        let _destination_hook =
            unix::DestinationReplaceHookGuard::new(swap_destination_for_directory);
        let _post_reverse = unix::PostReverseHookGuard::new(swap_destination_after_reverse);
        let result = temp.replace("destination");
        let error = result.expect_err(
            "a destination that is neither the retained inode nor the restored directory must not be published",
        );
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert!(
            !error.publication_state().is_published(),
            "indeterminate destination must never be claimed published, got {error:?}"
        );
        let ConfinedPublicationState::Indeterminate {
            destination,
            staging,
        } = error.publication_state()
        else {
            panic!(
                "expected indeterminate publication state, got {:?}",
                error.publication_state()
            );
        };
        let destination = destination.expect("destination identity should be observed");
        let staging = staging.expect("staging identity should be observed");
        let victim_inode = std::fs::symlink_metadata(path.join("destination"))
            .expect("victim metadata should be readable")
            .ino();
        assert_eq!(destination.file_type(), ConfinedFileType::File);
        assert_eq!(destination.inode(), victim_inode);
        assert_eq!(staging.file_type(), ConfinedFileType::File);
        assert_eq!(staging.inode(), retained_inode);
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"victim");
        assert!(
            path.join(".hook-restored-dir").is_dir(),
            "directory must be preserved after the concurrent swap"
        );
        assert_eq!(
            std::fs::read(path.join(".hook-restored-dir/marker"))
                .expect("directory contents must survive the concurrent swap"),
            b"keep-dir"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn write_regular_file_at(parent: RawFd, name: &[u8], contents: &[u8]) {
        let name = CString::new(name).expect("test name has no NUL");
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        assert!(fd >= 0, "test file should be created");
        let written = unsafe { libc::write(fd, contents.as_ptr().cast(), contents.len()) };
        assert_eq!(
            written,
            contents.len() as isize,
            "test file should be written"
        );
        let closed = unsafe { libc::close(fd) };
        assert_eq!(closed, 0, "test file should close");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn exchange_mismatch_moves_published_inode_and_plants_victim(
        parent: RawFd,
        _staging: &[u8],
        destination: &[u8],
    ) {
        let moved = CString::new(".hook-retained-inode").expect("fixed hook name is valid");
        let destination = CString::new(destination).expect("destination has no NUL");
        let result =
            unsafe { libc::renameat(parent, destination.as_ptr(), parent, moved.as_ptr()) };
        assert_eq!(result, 0, "published inode should be moved aside");
        write_regular_file_at(parent, b"destination", b"victim");
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn noreplace_mismatch_exchanges_published_inode_back_to_staging(
        parent: RawFd,
        staging: &[u8],
        destination: &[u8],
    ) {
        write_regular_file_at(parent, staging, b"victim");
        let staging = CString::new(staging).expect("staging name has no NUL");
        let destination = CString::new(destination).expect("destination has no NUL");
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent,
                staging.as_ptr(),
                parent,
                destination.as_ptr(),
                2u32,
            )
        };
        assert_eq!(
            result, 0,
            "published inode should exchange back onto staging"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn replace_exchange_destination_mismatch_cleans_owned_staging_and_leaves_victim() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-exchange-mismatch-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "exchange-mismatch")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _post_rename = unix::PostRenameHookGuard::new(
            exchange_mismatch_moves_published_inode_and_plants_victim,
        );
        let result = temp.replace("destination");
        let error = result.expect_err("destination identity mismatch must be reported");
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert!(
            !temp.completed,
            "replace must not complete before owned links are cleaned and destination is verified"
        );
        assert_eq!(
            std::fs::read(path.join("destination")).expect("victim at destination must survive"),
            b"victim"
        );
        assert_eq!(
            std::fs::read(path.join(".hook-retained-inode"))
                .expect("retained inode should remain where the hook moved it"),
            b"new"
        );
        assert!(
            rustscript_publish_names(&path).is_empty(),
            "exchange leftover staging must not remain"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn replace_noreplace_destination_mismatch_cleans_retained_staging_and_leaves_victim() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-noreplace-mismatch-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "noreplace-mismatch")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _post_rename = unix::PostRenameHookGuard::new(
            noreplace_mismatch_exchanges_published_inode_back_to_staging,
        );
        let result = temp.replace("destination");
        let error = result.expect_err("destination identity mismatch must be reported");
        assert_eq!(error.kind(), ConfinedFsErrorKind::RaceDetected);
        assert!(
            !temp.completed,
            "replace must not complete before owned links are cleaned and destination is verified"
        );
        assert_eq!(
            std::fs::read(path.join("destination")).expect("victim at destination must survive"),
            b"victim"
        );
        assert!(
            rustscript_publish_names(&path).is_empty(),
            "retained staging inode must not leak after noreplace mismatch"
        );
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn replace_parent_fsync_failure_still_reports_published() {
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-fsync-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("temporary directory should be created");
        std::fs::write(path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "fsync")
            .expect("temporary file should be created");
        temp.write_all(b"new").expect("temporary write should work");
        let _test_guard = unix::replace_test_lock();
        let _fsync_guard = unix::ForceFsyncFailGuard::new();
        let publication = temp
            .replace("destination")
            .expect("publication must succeed even if parent durability fails");
        assert!(publication.is_published());
        assert!(
            !publication.is_durable(),
            "parent fsync failure must be recorded on the publication outcome"
        );
        assert!(publication.staging_cleaned());
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"new");
        drop(root);
        std::fs::remove_dir_all(&path).expect("temporary directory should be removed");
    }

    #[test]
    fn path_validation_rejects_traversal_and_platform_prefixes() {
        for path in ["", "/tmp", "../secret", "a/../secret", "a\\b", "C:secret"] {
            assert!(validate_relative_path(path, "test").is_err(), "{path:?}");
        }
        assert_eq!(
            validate_relative_path("a/b", "test")
                .expect("valid path")
                .components,
            vec!["a", "b"]
        );
    }

    #[test]
    fn readdir_end_never_treats_unknown_errno_abi_as_eof() {
        let unknown = classify_readdir_end(None);
        assert_eq!(unknown.kind(), ConfinedFsErrorKind::UnsupportedPlatform);
        assert!(matches!(classify_readdir_end_or_eof(Some(0)), Ok(())));
        assert!(
            classify_readdir_end_or_eof(None).is_err(),
            "missing errno ABI must not be classified as end-of-directory"
        );
        #[cfg(unix)]
        {
            let io_error = classify_readdir_end(Some(5));
            assert_ne!(io_error.kind(), ConfinedFsErrorKind::UnsupportedPlatform);
        }
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn partial_write_fail_guard_restores_previous_state_after_panic_before_consumption() {
        let _outer = PartialWriteFailGuard::new(3);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = PartialWriteFailGuard::new(1);
            panic!("before hook consumption");
        }));
        assert!(panicked.is_err());
        assert_eq!(
            TEST_PARTIAL_WRITE_FAIL_AFTER.with(Cell::get),
            3,
            "panic before consumption must restore the previous partial-write limit"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn replace_hook_guard_restores_previous_state_after_panic_before_consumption() {
        fn previous(_parent: RawFd, _source: &[u8], _destination: &[u8]) {}
        fn inner(_parent: RawFd, _source: &[u8], _destination: &[u8]) {}
        let _outer = unix::ReplaceHookGuard::new(previous);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unix::ReplaceHookGuard::new(inner);
            panic!("before hook consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::replace_test_hook_installed()
                .is_some_and(|hook| std::ptr::fn_addr_eq(hook, previous as unix::ReplaceTestHook)),
            "panic before consumption must restore the previous replace hook"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn staging_link_hook_guard_restores_previous_state_after_panic_before_consumption() {
        fn previous(_parent: RawFd, _staging: &[u8]) {}
        fn inner(_parent: RawFd, _staging: &[u8]) {}
        let _outer = unix::StagingLinkHookGuard::new(previous);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unix::StagingLinkHookGuard::new(inner);
            panic!("before hook consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::staging_link_test_hook_installed().is_some_and(|hook| {
                std::ptr::fn_addr_eq(hook, previous as unix::StagingLinkTestHook)
            }),
            "panic before consumption must restore the previous staging-link hook"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn post_rename_hook_guard_restores_previous_state_after_panic_before_consumption() {
        fn previous(_parent: RawFd, _staging: &[u8], _destination: &[u8]) {}
        fn inner(_parent: RawFd, _staging: &[u8], _destination: &[u8]) {}
        let _outer = unix::PostRenameHookGuard::new(previous);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unix::PostRenameHookGuard::new(inner);
            panic!("before hook consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::post_rename_test_hook_installed().is_some_and(|hook| {
                std::ptr::fn_addr_eq(hook, previous as unix::PostRenameTestHook)
            }),
            "panic before consumption must restore the previous post-rename hook"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn destination_replace_hook_guard_restores_previous_state_after_panic_before_consumption() {
        fn previous(_parent: RawFd, _destination: &[u8]) {}
        fn inner(_parent: RawFd, _destination: &[u8]) {}
        let _outer = unix::DestinationReplaceHookGuard::new(previous);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unix::DestinationReplaceHookGuard::new(inner);
            panic!("before hook consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::destination_replace_test_hook_installed().is_some_and(|hook| {
                std::ptr::fn_addr_eq(hook, previous as unix::DestinationReplaceTestHook)
            }),
            "panic before consumption must restore the previous destination-replace hook"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fsync_fail_guard_inner_drop_restores_outer_true() {
        let _outer = unix::ForceFsyncFailGuard::new();
        {
            let _inner = unix::ForceFsyncFailGuard::new();
            assert!(
                unix::force_fsync_fail_enabled(),
                "inner guard must keep the force-fsync-fail flag enabled"
            );
        }
        assert!(
            unix::force_fsync_fail_enabled(),
            "inner drop must restore the outer true flag"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fsync_fail_guard_panic_restores_prior_false() {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unix::ForceFsyncFailGuard::new();
            panic!("before fsync-fail consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            !unix::force_fsync_fail_enabled(),
            "panic must restore the previous false flag"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fsync_fail_guard_panic_restores_prior_true() {
        let _outer = unix::ForceFsyncFailGuard::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = unix::ForceFsyncFailGuard::new();
            panic!("before fsync-fail consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::force_fsync_fail_enabled(),
            "panic must restore the previous true flag"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fallback_guard_inner_drop_restores_outer_true() {
        let _outer = ForceFallbackGuard::new();
        {
            let _inner = ForceFallbackGuard::new();
            assert!(
                unix::force_openat2_fallback_enabled(),
                "inner guard must keep the force-openat2-fallback flag enabled"
            );
        }
        assert!(
            unix::force_openat2_fallback_enabled(),
            "inner drop must restore the outer true flag"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fallback_guard_panic_restores_prior_false() {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ForceFallbackGuard::new();
            panic!("before fallback consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            !unix::force_openat2_fallback_enabled(),
            "panic must restore the previous false flag"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn force_fallback_guard_panic_restores_prior_true() {
        let _outer = ForceFallbackGuard::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = ForceFallbackGuard::new();
            panic!("before fallback consumption");
        }));
        assert!(panicked.is_err());
        assert!(
            unix::force_openat2_fallback_enabled(),
            "panic must restore the previous true flag"
        );
    }

    #[cfg(all(unix, test))]
    #[test]
    fn replace_test_hooks_are_isolated_to_the_installing_thread() {
        #[cfg(target_os = "linux")]
        {
            assert!(
                publication_supported() && ConfinedFsRoot::supports_atomic_publication(),
                "Linux must advertise atomic publication"
            );
            fn mark(_parent: RawFd, _destination: &[u8]) {}
            let _hook = unix::DestinationReplaceHookGuard::new(mark);
            assert!(unix::destination_replace_test_hook_installed().is_some());
            let worker = std::thread::spawn(|| {
                assert!(
                    unix::destination_replace_test_hook_installed().is_none(),
                    "hooks installed on another thread must not be visible here"
                );
            });
            worker.join().expect("hook isolation worker should finish");
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(
                !publication_supported(),
                "non-Linux Unix must not advertise atomic publication"
            );
        }
    }
}
