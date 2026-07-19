use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CallSiteKey {
    pub(crate) caller_frame_key: u64,
    pub(crate) call_ip: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallSiteProfile {
    prototype_id: u32,
    observations: u64,
    mismatches: u64,
    monomorphic: bool,
}

impl CallSiteProfile {
    fn new(prototype_id: u32) -> Self {
        Self {
            prototype_id,
            observations: 1,
            mismatches: 0,
            monomorphic: true,
        }
    }

    fn observe(&mut self, prototype_id: u32) {
        self.observations = self.observations.saturating_add(1);
        if prototype_id != self.prototype_id {
            self.mismatches = self.mismatches.saturating_add(1);
            self.monomorphic = false;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitCallSiteProfile {
    pub caller_frame_key: u64,
    pub call_ip: usize,
    pub prototype_id: u32,
    pub observations: u64,
    pub mismatches: u64,
    pub monomorphic: bool,
}

pub(crate) fn observe_script_call_target(
    profiles: &mut HashMap<CallSiteKey, CallSiteProfile>,
    key: CallSiteKey,
    prototype_id: u32,
) {
    match profiles.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.get_mut().observe(prototype_id);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(CallSiteProfile::new(prototype_id));
        }
    }
}

pub(crate) fn call_site_profiles(
    profiles: &HashMap<CallSiteKey, CallSiteProfile>,
) -> Vec<JitCallSiteProfile> {
    let mut snapshot = profiles
        .iter()
        .map(|(key, profile)| JitCallSiteProfile {
            caller_frame_key: key.caller_frame_key,
            call_ip: key.call_ip,
            prototype_id: profile.prototype_id,
            observations: profile.observations,
            mismatches: profile.mismatches,
            monomorphic: profile.monomorphic,
        })
        .collect::<Vec<_>>();
    snapshot.sort_unstable_by_key(|profile| (profile.caller_frame_key, profile.call_ip));
    snapshot
}

pub(crate) fn call_site_metric_summary(
    profiles: &HashMap<CallSiteKey, CallSiteProfile>,
) -> (u64, u64, u64) {
    profiles.values().fold(
        (0u64, 0u64, 0u64),
        |(observations, monomorphic, polymorphic), profile| {
            (
                observations.saturating_add(profile.observations),
                monomorphic.saturating_add(u64::from(profile.monomorphic)),
                polymorphic.saturating_add(u64::from(!profile.monomorphic)),
            )
        },
    )
}
