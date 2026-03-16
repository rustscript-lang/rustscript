#![cfg_attr(
    not(any(feature = "tls", feature = "http2", feature = "http3")),
    allow(dead_code)
)]

use std::{
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    sync::RwLock,
};

use crate::lock_metrics::{self, LockMetricKey};

pub(crate) const DEFAULT_TLS_SESSION_REUSE_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_UPSTREAM_HTTP_REUSE_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_DOWNSTREAM_HTTP2_SESSION_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_UPSTREAM_HTTP3_REUSE_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_DOWNSTREAM_HTTP3_SESSION_STORE_CAPACITY: usize = 256;
const DEFAULT_SHARD_COUNT: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct BoundedLruStore<K, V>
where
    K: Clone + Eq + Hash,
{
    capacity: usize,
    values: HashMap<K, V>,
    lru_order: VecDeque<K>,
}

impl<K, V> BoundedLruStore<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashMap::new(),
            lru_order: VecDeque::new(),
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<&V> {
        if !self.values.contains_key(key) {
            return None;
        }
        self.touch_key(key);
        self.values.get(key)
    }

    pub(crate) fn peek(&self, key: &K) -> Option<&V> {
        self.values.get(key)
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if !self.values.contains_key(key) {
            return None;
        }
        self.touch_key(key);
        self.values.get_mut(key)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.remove_from_lru(&key);
        if self.capacity == 0 {
            return self.values.remove(&key);
        }

        let previous = self.values.insert(key.clone(), value);
        self.lru_order.push_back(key);
        self.evict_to_capacity();
        previous
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.remove_from_lru(key);
        self.values.remove(key)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let to_remove = self
            .values
            .iter()
            .filter_map(|(key, value)| (!keep(key, value)).then_some(key.clone()))
            .collect::<Vec<_>>();
        for key in to_remove {
            let _ = self.remove(&key);
        }
    }

    fn touch_key(&mut self, key: &K) {
        if self
            .lru_order
            .back()
            .is_some_and(|most_recent| most_recent == key)
        {
            return;
        }
        self.remove_from_lru(key);
        self.lru_order.push_back(key.clone());
    }

    fn evict_to_capacity(&mut self) {
        while self.values.len() > self.capacity {
            let Some(oldest) = self.lru_order.pop_front() else {
                break;
            };
            let _ = self.values.remove(&oldest);
        }
    }

    fn remove_from_lru(&mut self, key: &K) {
        let mut retained = VecDeque::with_capacity(self.lru_order.len());
        while let Some(candidate) = self.lru_order.pop_front() {
            if &candidate != key {
                retained.push_back(candidate);
            }
        }
        self.lru_order = retained;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.values.values()
    }
}

#[derive(Debug)]
pub(crate) struct ShardedRwLruStore<K, V>
where
    K: Clone + Eq + Hash,
{
    capacity: usize,
    shards: Box<[RwLock<BoundedLruStore<K, V>>]>,
}

impl<K, V> ShardedRwLruStore<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(capacity: usize) -> Self {
        let shard_count = recommended_shard_count(capacity);
        let base_capacity = capacity / shard_count;
        let extra_capacity = capacity % shard_count;
        let shards = (0..shard_count)
            .map(|index| {
                let shard_capacity = base_capacity + usize::from(index < extra_capacity);
                RwLock::new(BoundedLruStore::new(shard_capacity))
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { capacity, shards }
    }

    pub(crate) fn peek_cloned(
        &self,
        key: &K,
        metric_key: LockMetricKey,
        poison_message: &'static str,
    ) -> Option<V>
    where
        V: Clone,
    {
        let shard = self.shard(key);
        let guard = lock_metrics::read_lock(shard, metric_key, poison_message);
        guard.peek(key).cloned()
    }

    pub(crate) fn insert(
        &self,
        key: K,
        value: V,
        metric_key: LockMetricKey,
        poison_message: &'static str,
    ) -> Option<V> {
        let shard = self.shard(&key);
        let mut guard = lock_metrics::write_lock(shard, metric_key, poison_message);
        guard.insert(key, value)
    }

    pub(crate) fn get_or_insert_with_cloned(
        &self,
        key: K,
        metric_key: LockMetricKey,
        poison_message: &'static str,
        create: impl FnOnce() -> V,
    ) -> V
    where
        V: Clone,
    {
        {
            let shard = self.shard(&key);
            let guard = lock_metrics::read_lock(shard, metric_key, poison_message);
            if let Some(existing) = guard.peek(&key) {
                return existing.clone();
            }
        }

        let shard = self.shard(&key);
        let mut guard = lock_metrics::write_lock(shard, metric_key, poison_message);
        if let Some(existing) = guard.peek(&key) {
            return existing.clone();
        }

        let value = create();
        let cloned = value.clone();
        let _ = guard.insert(key, value);
        cloned
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(test)]
    pub(crate) fn values_cloned(&self) -> Vec<V>
    where
        V: Clone,
    {
        self.shards
            .iter()
            .flat_map(|shard| {
                shard
                    .read()
                    .expect("sharded lru store lock poisoned")
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn shard(&self, key: &K) -> &RwLock<BoundedLruStore<K, V>> {
        &self.shards[shard_index_for(key, self.shards.len())]
    }
}

pub(crate) fn shard_index_for<K>(key: &K, shard_count: usize) -> usize
where
    K: Hash + ?Sized,
{
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % shard_count.max(1)
}

fn recommended_shard_count(capacity: usize) -> usize {
    capacity.clamp(1, DEFAULT_SHARD_COUNT)
}

#[cfg(test)]
mod tests {
    use crate::lock_metrics::LockMetricKey;

    use super::{BoundedLruStore, ShardedRwLruStore};

    #[test]
    fn least_recently_used_entry_is_evicted_after_capacity_is_reached() {
        let mut store = BoundedLruStore::new(2);
        store.insert("a", 1);
        store.insert("b", 2);

        assert_eq!(
            store.get(&"a"),
            Some(&1),
            "touching a should refresh recency"
        );

        store.insert("c", 3);

        assert_eq!(store.get(&"a"), Some(&1));
        assert_eq!(store.get(&"b"), None);
        assert_eq!(store.get(&"c"), Some(&3));
    }

    #[test]
    fn zero_capacity_store_does_not_retain_entries() {
        let mut store = BoundedLruStore::new(0);
        store.insert("a", 1);
        store.insert("b", 2);

        assert_eq!(store.get(&"a"), None);
        assert_eq!(store.get(&"b"), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn sharded_store_retains_requested_total_capacity() {
        let store = ShardedRwLruStore::<&str, i32>::new(10);
        assert_eq!(store.capacity(), 10);
    }

    #[test]
    fn sharded_store_get_or_insert_reuses_existing_value() {
        let store = ShardedRwLruStore::<&str, i32>::new(4);
        let first = store.get_or_insert_with_cloned(
            "a",
            LockMetricKey::UpstreamClientCache,
            "test store lock poisoned",
            || 1,
        );
        let second = store.get_or_insert_with_cloned(
            "a",
            LockMetricKey::UpstreamClientCache,
            "test store lock poisoned",
            || 2,
        );
        assert_eq!(first, 1);
        assert_eq!(second, 1);
        assert_eq!(store.values_cloned(), vec![1]);
    }
}
