#![cfg_attr(not(any(feature = "tls", feature = "http2")), allow(dead_code))]

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
};

pub(crate) const DEFAULT_TLS_SESSION_REUSE_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_UPSTREAM_HTTP_REUSE_STORE_CAPACITY: usize = 256;
pub(crate) const DEFAULT_DOWNSTREAM_HTTP2_SESSION_STORE_CAPACITY: usize = 256;

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

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.values.values()
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedLruStore;

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
}
