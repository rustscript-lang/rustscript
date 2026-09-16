//! Per-VM regular-expression cache owned by the regex host module.
//!
//! The cache is *host-private state*: it is one mutable instance per VM,
//! created lazily from [`RegexCache::initialize`], survives VM reuse and
//! execution-scope resets, stays isolated between VMs, and drops with its VM.
//! It never appears in guest arity, `HostFunctionSchema`, catalog
//! fingerprints, or VMBC, and `src/vm/**` owns no part of it — the generic VM
//! layer only provides the type-erased state table.
//!
//! Configuration and statistics are exposed here (module-owned
//! [`RegexCacheVmExt`] methods on [`Vm`]) instead of as inherent VM methods, so
//! the VM core stays free of regex concepts.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use regex::Regex;

use crate::host_api::HostState;
use crate::vm::{HostStateMut, Vm, VmError, VmResult};

/// Maximum number of compiled patterns retained by one VM by default.
pub const DEFAULT_REGEX_CACHE_CAPACITY: usize = 512;

/// Effect label used for regex-cache state diagnostics.
pub(crate) const REGEX_CACHE_EFFECT: &str = "state write";

/// Operation label for configuration-driven cache resolution.
const REGEX_CACHE_CONFIG_OPERATION: &str = "regex::cache_config";

/// Bounded per-VM compiled-pattern cache with LRU eviction and counters.
pub struct RegexCache {
    capacity: usize,
    entries: HashMap<String, Arc<Regex>>,
    recency: VecDeque<String>,
    compile_count: u64,
    hit_count: u64,
}

impl Default for RegexCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_REGEX_CACHE_CAPACITY)
    }
}

impl HostState for RegexCache {
    const KEY: &'static str = "regex.cache";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

impl RegexCache {
    /// Creates an empty cache with an explicit capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            recency: VecDeque::new(),
            compile_count: 0,
            hit_count: 0,
        }
    }

    /// Returns the compiled pattern, compiling and caching it on first use.
    ///
    /// A capacity of zero disables caching: every call compiles the pattern,
    /// the compile counter advances, and the hit counter stays unchanged.
    /// A pattern that fails to compile is never cached and never counted.
    pub fn get_or_compile(&mut self, pattern: &str) -> Result<Arc<Regex>, regex::Error> {
        if let Some(regex) = self.entries.get(pattern).cloned() {
            self.hit_count = self.hit_count.saturating_add(1);
            self.touch(pattern);
            return Ok(regex);
        }

        let regex = Arc::new(Regex::new(pattern)?);
        self.compile_count = self.compile_count.saturating_add(1);
        if self.capacity == 0 {
            return Ok(regex);
        }
        while self.entries.len() >= self.capacity {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.entries.insert(pattern.to_string(), regex.clone());
        self.recency.push_back(pattern.to_string());
        Ok(regex)
    }

    fn touch(&mut self, pattern: &str) {
        if let Some(index) = self.recency.iter().position(|entry| entry == pattern) {
            self.recency.remove(index);
        }
        self.recency.push_back(pattern.to_string());
    }

    /// Configured capacity (zero disables caching).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Changes the capacity, evicting least-recently-used entries immediately.
    ///
    /// Setting zero clears every entry and disables caching until a positive
    /// capacity is configured again.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > capacity {
            let Some(oldest) = self.recency.pop_front() else {
                self.entries.clear();
                break;
            };
            self.entries.remove(&oldest);
        }
        if capacity == 0 {
            self.recency.clear();
        }
    }

    /// Number of cached compiled patterns.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no compiled pattern is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of successful compilations performed by this cache.
    pub fn compile_count(&self) -> u64 {
        self.compile_count
    }

    /// Number of cache hits served by this cache.
    pub fn hit_count(&self) -> u64 {
        self.hit_count
    }
}

/// Resolves this VM's private regex cache and runs `run` with its guard.
///
/// The guard is created from the *same* per-VM host-state table the
/// interpreter hosts use, so the native/JIT/AOT shortcuts and the interpreter
/// share one cache instance, one LRU order, and one pair of counters.
pub(crate) fn with_regex_cache<R>(
    vm: &mut Vm,
    operation: &str,
    run: impl FnOnce(HostStateMut<'_, RegexCache>) -> VmResult<R>,
) -> VmResult<R> {
    let mut context = vm.host_context();
    context
        .ensure_host_state::<RegexCache>(operation, REGEX_CACHE_EFFECT)
        .map_err(|error| VmError::HostError(error.to_string()))?;
    let cache = context
        .host_state_mut::<RegexCache>(operation, REGEX_CACHE_EFFECT)
        .map_err(|error| VmError::HostError(error.to_string()))?;
    run(cache)
}

/// Regex-module-owned configuration and statistics for one VM's cache.
///
/// Implemented outside `src/vm/**` on purpose: the VM core owns the generic
/// host-state table but never a regex concept.
pub trait RegexCacheVmExt {
    /// Configured capacity of this VM's regex cache.
    ///
    /// Reports [`DEFAULT_REGEX_CACHE_CAPACITY`] while the cache has not been
    /// initialized yet. A capacity of zero disables caching.
    fn regex_cache_capacity(&self) -> usize;

    /// Changes this VM's regex cache capacity, evicting LRU entries
    /// immediately; zero clears the cache and disables caching.
    fn set_regex_cache_capacity(&mut self, capacity: usize) -> VmResult<()>;

    /// Number of compiled patterns currently cached by this VM.
    fn regex_cache_entry_count(&self) -> usize;

    /// Number of compilations performed for this VM.
    fn regex_cache_compile_count(&self) -> u64;

    /// Number of cache hits served for this VM.
    fn regex_cache_hit_count(&self) -> u64;
}

impl RegexCacheVmExt for Vm {
    fn regex_cache_capacity(&self) -> usize {
        self.host_state::<RegexCache>()
            .map(|cache| cache.capacity())
            .unwrap_or(DEFAULT_REGEX_CACHE_CAPACITY)
    }

    fn set_regex_cache_capacity(&mut self, capacity: usize) -> VmResult<()> {
        let mut context = self.host_context();
        context
            .ensure_host_state::<RegexCache>(REGEX_CACHE_CONFIG_OPERATION, REGEX_CACHE_EFFECT)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let mut cache = context
            .host_state_mut::<RegexCache>(REGEX_CACHE_CONFIG_OPERATION, REGEX_CACHE_EFFECT)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        cache.set_capacity(capacity);
        Ok(())
    }

    fn regex_cache_entry_count(&self) -> usize {
        self.host_state::<RegexCache>()
            .map(|cache| cache.len())
            .unwrap_or(0)
    }

    fn regex_cache_compile_count(&self) -> u64 {
        self.host_state::<RegexCache>()
            .map(|cache| cache.compile_count())
            .unwrap_or(0)
    }

    fn regex_cache_hit_count(&self) -> u64 {
        self.host_state::<RegexCache>()
            .map(|cache| cache.hit_count())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_REGEX_CACHE_CAPACITY, RegexCache};

    #[test]
    fn cache_is_bounded_and_evicts_the_oldest_pattern() {
        let mut cache = RegexCache::with_capacity(2);
        cache.get_or_compile("a").expect("pattern should compile");
        cache.get_or_compile("b").expect("pattern should compile");
        cache.get_or_compile("c").expect("pattern should compile");

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.compile_count(), 3);
        assert_eq!(cache.hit_count(), 0);

        // "a" was the oldest entry and must have been evicted.
        cache.get_or_compile("a").expect("pattern should compile");
        assert_eq!(cache.compile_count(), 4);
        assert_eq!(cache.hit_count(), 0);
    }

    #[test]
    fn cache_hits_keep_the_most_recently_used_pattern() {
        let mut cache = RegexCache::with_capacity(2);
        cache.get_or_compile("a").expect("pattern should compile");
        cache.get_or_compile("b").expect("pattern should compile");
        cache.get_or_compile("a").expect("pattern should hit");
        cache.get_or_compile("c").expect("pattern should evict b");

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.compile_count(), 3);
        assert_eq!(cache.hit_count(), 1);
        cache.get_or_compile("a").expect("pattern should hit");
        assert_eq!(cache.hit_count(), 2);
    }

    #[test]
    fn zero_capacity_disables_caching() {
        let mut cache = RegexCache::with_capacity(0);
        cache
            .get_or_compile("same")
            .expect("pattern should compile");
        cache
            .get_or_compile("same")
            .expect("pattern should compile again");

        assert_eq!(cache.capacity(), 0);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.compile_count(), 2);
        assert_eq!(cache.hit_count(), 0);
    }

    #[test]
    fn shrinking_the_capacity_evicts_immediately() {
        let mut cache = RegexCache::default();
        assert_eq!(cache.capacity(), DEFAULT_REGEX_CACHE_CAPACITY);
        cache.get_or_compile("a").expect("pattern should compile");
        cache.get_or_compile("b").expect("pattern should compile");
        cache.get_or_compile("c").expect("pattern should compile");

        cache.set_capacity(1);
        assert_eq!(cache.capacity(), 1);
        assert_eq!(cache.len(), 1);

        cache.get_or_compile("c").expect("most recent stays cached");
        assert_eq!(cache.compile_count(), 3);
        assert_eq!(cache.hit_count(), 1);
    }

    #[test]
    fn invalid_patterns_are_never_cached_or_counted() {
        let mut cache = RegexCache::with_capacity(4);
        assert!(cache.get_or_compile("(").is_err());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.compile_count(), 0);
        assert_eq!(cache.hit_count(), 0);
    }
}
