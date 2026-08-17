use crate::builtins::BuiltinFunction;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const PROFILE_VERSION: &[u8] = b"rustscript-capability-profile-v2";

/// Immutable authorization policy for privileged builtin calls and host imports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityProfile {
    allow_all_builtins: bool,
    allow_all_host_imports: bool,
    allowed_builtin_calls: Vec<u16>,
    allowed_host_imports: Vec<String>,
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

    pub fn build(mut self) -> CapabilityProfile {
        self.allowed_builtin_calls.sort_unstable();
        self.allowed_builtin_calls.dedup();
        self.allowed_host_imports.sort();
        self.allowed_host_imports.dedup();
        let fingerprint = fingerprint(
            self.allow_all_builtins,
            self.allow_all_host_imports,
            &self.allowed_builtin_calls,
            &self.allowed_host_imports,
        );
        CapabilityProfile {
            allow_all_builtins: self.allow_all_builtins,
            allow_all_host_imports: self.allow_all_host_imports,
            allowed_builtin_calls: self.allowed_builtin_calls,
            allowed_host_imports: self.allowed_host_imports,
            fingerprint,
        }
    }
}

fn fingerprint(
    allow_all_builtins: bool,
    allow_all_host_imports: bool,
    builtin_calls: &[u16],
    host_imports: &[String],
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
    value
}

fn update_fingerprint(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(FNV_PRIME);
    }
}
