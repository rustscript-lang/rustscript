use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use regex::Regex;

pub(crate) const DEFAULT_REGEX_CACHE_CAPACITY: usize = 512;

/// Per-VM bounded regex cache shared by the generic execution engines.
pub(crate) struct RegexCache {
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

impl RegexCache {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            recency: VecDeque::new(),
            compile_count: 0,
            hit_count: 0,
        }
    }

    pub(crate) fn get_or_compile(&mut self, pattern: &str) -> Result<Arc<Regex>, regex::Error> {
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

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn set_capacity(&mut self, capacity: usize) {
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

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn compile_count(&self) -> u64 {
        self.compile_count
    }

    pub(crate) fn hit_count(&self) -> u64 {
        self.hit_count
    }
}
