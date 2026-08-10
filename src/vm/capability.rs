#[cfg(feature = "sqlite")]
use super::SqlitePolicy;
use crate::builtins::BuiltinFunction;
use crate::builtins::runtime::HttpConfig;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const PROFILE_VERSION: &[u8] = b"rustscript-capability-profile-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoPolicy {
    pub allowed_roots: Vec<String>,
    pub allow_write: bool,
    pub allow_process: bool,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for IoPolicy {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            allow_write: false,
            allow_process: false,
            max_read_bytes: 1024 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

/// Immutable authorization policy for privileged builtin calls and host imports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityProfile {
    allow_all_builtins: bool,
    allow_all_host_imports: bool,
    allowed_builtin_calls: Vec<u16>,
    allowed_host_imports: Vec<String>,
    http_policy: Option<HttpConfig>,
    io_policy: Option<IoPolicy>,
    #[cfg(feature = "sqlite")]
    sqlite_policy: Option<SqlitePolicy>,
    fingerprint: u64,
}

impl CapabilityProfile {
    pub fn builder() -> CapabilityProfileBuilder {
        CapabilityProfileBuilder::default()
    }

    pub fn deny_all() -> Self {
        CapabilityProfileBuilder::default().build()
    }

    pub fn allow_all() -> Self {
        CapabilityProfileBuilder {
            allow_all_builtins: true,
            allow_all_host_imports: true,
            ..CapabilityProfileBuilder::default()
        }
        .build()
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn allows_builtin(&self, builtin: BuiltinFunction) -> bool {
        self.allow_all_builtins
            || self
                .allowed_builtin_calls
                .binary_search(&builtin.call_index())
                .is_ok()
    }

    pub fn allows_host_import(&self, name: &str) -> bool {
        self.allow_all_host_imports
            || self
                .allowed_host_imports
                .binary_search_by(|candidate| candidate.as_str().cmp(name))
                .is_ok()
    }

    pub fn http_policy(&self) -> Option<&HttpConfig> {
        self.http_policy.as_ref()
    }

    pub fn io_policy(&self) -> Option<&IoPolicy> {
        self.io_policy.as_ref()
    }

    #[cfg(feature = "sqlite")]
    pub fn sqlite_policy(&self) -> Option<&SqlitePolicy> {
        self.sqlite_policy.as_ref()
    }

    pub(crate) fn allowed_builtin_calls(&self) -> &[u16] {
        &self.allowed_builtin_calls
    }

    pub(crate) fn allows_all_builtins(&self) -> bool {
        self.allow_all_builtins
    }

    pub(crate) fn allows_all_host_imports(&self) -> bool {
        self.allow_all_host_imports
    }

    pub(crate) fn with_builtin(&self, builtin: BuiltinFunction) -> Self {
        let mut builder = CapabilityProfileBuilder {
            allow_all_builtins: self.allow_all_builtins,
            allow_all_host_imports: self.allow_all_host_imports,
            allowed_builtin_calls: self.allowed_builtin_calls.clone(),
            allowed_host_imports: self.allowed_host_imports.clone(),
            http_policy: self.http_policy.clone(),
            io_policy: self.io_policy.clone(),
            #[cfg(feature = "sqlite")]
            sqlite_policy: self.sqlite_policy.clone(),
        };
        builder.allowed_builtin_calls.push(builtin.call_index());
        builder.build()
    }

    pub(crate) fn with_host_import(&self, name: &str) -> Self {
        let mut builder = CapabilityProfileBuilder {
            allow_all_builtins: self.allow_all_builtins,
            allow_all_host_imports: self.allow_all_host_imports,
            allowed_builtin_calls: self.allowed_builtin_calls.clone(),
            allowed_host_imports: self.allowed_host_imports.clone(),
            http_policy: self.http_policy.clone(),
            io_policy: self.io_policy.clone(),
            #[cfg(feature = "sqlite")]
            sqlite_policy: self.sqlite_policy.clone(),
        };
        builder.allowed_host_imports.push(name.to_string());
        builder.build()
    }
}

impl Default for CapabilityProfile {
    fn default() -> Self {
        Self::deny_all()
    }
}

#[derive(Clone, Debug, Default)]
pub struct CapabilityProfileBuilder {
    allow_all_builtins: bool,
    allow_all_host_imports: bool,
    allowed_builtin_calls: Vec<u16>,
    allowed_host_imports: Vec<String>,
    http_policy: Option<HttpConfig>,
    io_policy: Option<IoPolicy>,
    #[cfg(feature = "sqlite")]
    sqlite_policy: Option<SqlitePolicy>,
}

impl CapabilityProfileBuilder {
    pub fn allow_builtin(mut self, builtin: BuiltinFunction) -> Self {
        self.allowed_builtin_calls.push(builtin.call_index());
        self
    }

    pub fn allow_host_import(mut self, name: impl Into<String>) -> Self {
        self.allowed_host_imports.push(name.into());
        self
    }

    pub fn http_policy(mut self, policy: HttpConfig) -> Self {
        self.http_policy = Some(policy);
        self
    }

    pub fn io_policy(mut self, policy: IoPolicy) -> Self {
        self.io_policy = Some(policy);
        self
    }

    #[cfg(feature = "sqlite")]
    pub fn sqlite_policy(mut self, policy: SqlitePolicy) -> Self {
        self.sqlite_policy = Some(policy);
        self
    }

    pub fn build(mut self) -> CapabilityProfile {
        self.allowed_builtin_calls.sort_unstable();
        self.allowed_builtin_calls.dedup();
        self.allowed_host_imports.sort();
        self.allowed_host_imports.dedup();
        if let Some(policy) = self.http_policy.as_mut() {
            policy.allowed_schemes.sort();
            policy.allowed_schemes.dedup();
            policy.allowed_hosts.sort();
            policy.allowed_hosts.dedup();
            policy.allowed_ports.sort_unstable();
            policy.allowed_ports.dedup();
        }
        if let Some(policy) = self.io_policy.as_mut() {
            policy.allowed_roots.sort();
            policy.allowed_roots.dedup();
        }
        let fingerprint = fingerprint(
            self.allow_all_builtins,
            self.allow_all_host_imports,
            &self.allowed_builtin_calls,
            &self.allowed_host_imports,
            self.http_policy.as_ref(),
            self.io_policy.as_ref(),
            #[cfg(feature = "sqlite")]
            self.sqlite_policy.as_ref(),
        );
        CapabilityProfile {
            allow_all_builtins: self.allow_all_builtins,
            allow_all_host_imports: self.allow_all_host_imports,
            allowed_builtin_calls: self.allowed_builtin_calls,
            allowed_host_imports: self.allowed_host_imports,
            http_policy: self.http_policy,
            io_policy: self.io_policy,
            #[cfg(feature = "sqlite")]
            sqlite_policy: self.sqlite_policy,
            fingerprint,
        }
    }
}

fn fingerprint(
    allow_all_builtins: bool,
    allow_all_host_imports: bool,
    builtin_calls: &[u16],
    host_imports: &[String],
    http_policy: Option<&HttpConfig>,
    io_policy: Option<&IoPolicy>,
    #[cfg(feature = "sqlite")] sqlite_policy: Option<&SqlitePolicy>,
) -> u64 {
    let mut value = FNV_OFFSET_BASIS;
    update_fingerprint(&mut value, PROFILE_VERSION);
    update_fingerprint(
        &mut value,
        &[
            u8::from(allow_all_builtins),
            u8::from(allow_all_host_imports),
        ],
    );
    update_fingerprint(&mut value, &(builtin_calls.len() as u64).to_le_bytes());
    for call in builtin_calls {
        update_fingerprint(&mut value, &call.to_le_bytes());
    }
    update_fingerprint(&mut value, &(host_imports.len() as u64).to_le_bytes());
    for name in host_imports {
        update_fingerprint(&mut value, &(name.len() as u64).to_le_bytes());
        update_fingerprint(&mut value, name.as_bytes());
    }
    match http_policy {
        None => update_fingerprint(&mut value, &[0]),
        Some(policy) => {
            update_fingerprint(&mut value, &[1]);
            update_string_list(&mut value, &policy.allowed_schemes);
            update_string_list(&mut value, &policy.allowed_hosts);
            update_fingerprint(
                &mut value,
                &(policy.allowed_ports.len() as u64).to_le_bytes(),
            );
            for port in &policy.allowed_ports {
                update_fingerprint(&mut value, &port.to_le_bytes());
            }
            for limit in [
                policy.max_redirects as u64,
                policy.max_request_body_bytes as u64,
                policy.max_response_body_bytes as u64,
                policy.connect_timeout.as_secs(),
                u64::from(policy.connect_timeout.subsec_nanos()),
                policy.request_timeout.as_secs(),
                u64::from(policy.request_timeout.subsec_nanos()),
            ] {
                update_fingerprint(&mut value, &limit.to_le_bytes());
            }
            update_fingerprint(&mut value, &[u8::from(policy.allow_private_ips)]);
        }
    }
    match io_policy {
        None => update_fingerprint(&mut value, &[0]),
        Some(policy) => {
            update_fingerprint(&mut value, &[1]);
            update_string_list(&mut value, &policy.allowed_roots);
            update_fingerprint(
                &mut value,
                &[u8::from(policy.allow_write), u8::from(policy.allow_process)],
            );
            update_fingerprint(&mut value, &(policy.max_read_bytes as u64).to_le_bytes());
            update_fingerprint(&mut value, &(policy.max_write_bytes as u64).to_le_bytes());
        }
    }
    #[cfg(feature = "sqlite")]
    match sqlite_policy {
        None => update_fingerprint(&mut value, &[0]),
        Some(policy) => {
            update_fingerprint(&mut value, &[1]);
            match &policy.database_root {
                None => update_fingerprint(&mut value, &[0]),
                Some(root) => {
                    update_fingerprint(&mut value, &[1]);
                    update_fingerprint(&mut value, &(root.len() as u64).to_le_bytes());
                    update_fingerprint(&mut value, root.as_bytes());
                }
            }
            update_fingerprint(&mut value, &[u8::from(policy.allow_unsafe_sql)]);
            for limit in [
                policy.limits.max_connections as u64,
                policy.limits.max_statements as u64,
                policy.limits.max_rows as u64,
                policy.limits.max_columns as u64,
                policy.limits.max_result_bytes as u64,
                policy.limits.max_statement_bytes as u64,
                policy.limits.max_parameters as u64,
                policy.limits.max_parameter_bytes as u64,
                policy.limits.max_pending_operations as u64,
                policy.limits.max_transaction_ms,
                policy.limits.busy_timeout_ms,
            ] {
                update_fingerprint(&mut value, &limit.to_le_bytes());
            }
        }
    }
    value
}

fn update_string_list(fingerprint: &mut u64, values: &[String]) {
    update_fingerprint(fingerprint, &(values.len() as u64).to_le_bytes());
    for value in values {
        update_fingerprint(fingerprint, &(value.len() as u64).to_le_bytes());
        update_fingerprint(fingerprint, value.as_bytes());
    }
}

fn update_fingerprint(fingerprint: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *fingerprint ^= u64::from(*byte);
        *fingerprint = fingerprint.wrapping_mul(FNV_PRIME);
    }
}
