use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use regex::Regex;

use super::VmArray;
use crate::vm::{Value, Vm, VmError, VmResult};
use pd_host_function::pd_host_function;

const DEFAULT_REGEX_CACHE_CAPACITY: usize = 512;

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

fn cached_regex(vm: &mut Vm, operation: &str, pattern: &str) -> VmResult<Arc<Regex>> {
    vm.cached_regex(pattern)
        .map_err(|err| VmError::HostError(format!("{operation} invalid pattern: {err}")))
}

/// Returns whether a regular expression matches the input text.
#[pd_host_function(name = "re::match")]
pub(super) fn builtin_re_match(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<bool> {
    let regex = cached_regex(vm, "re_match", pattern)?;
    Ok(regex.is_match(text))
}

pub(crate) fn native_re_match(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<bool> {
    builtin_re_match_impl(vm, pattern, text)
}

/// Returns the first substring matched by a regular expression.
#[pd_host_function(name = "re::find")]
pub(super) fn builtin_re_find(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<Option<String>> {
    let regex = cached_regex(vm, "re_find", pattern)?;
    Ok(regex.find(text).map(|matched| matched.as_str().to_string()))
}

/// Replaces all regular-expression matches in a string.
#[pd_host_function(name = "re::replace")]
pub(super) fn builtin_re_replace(
    vm: &mut Vm,
    pattern: &str,
    text: &str,
    replacement: &str,
) -> VmResult<String> {
    let regex = cached_regex(vm, "re_replace", pattern)?;
    Ok(regex.replace_all(text, replacement).into_owned())
}

pub(crate) fn native_re_replace(
    vm: &mut Vm,
    pattern: &str,
    text: &str,
    replacement: &str,
) -> VmResult<String> {
    builtin_re_replace_impl(vm, pattern, text, replacement)
}

/// Splits a string on regular-expression matches.
#[pd_host_function(name = "re::split")]
pub(super) fn builtin_re_split(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<VmArray> {
    let regex = cached_regex(vm, "re_split", pattern)?;
    Ok(regex
        .split(text)
        .map(|part| Value::string(part.to_string()))
        .collect::<Vec<_>>())
}

/// Returns the capture groups produced by the first regular-expression match.
#[pd_host_function(name = "re::captures")]
pub(super) fn builtin_re_captures(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<VmArray> {
    let regex = cached_regex(vm, "re_captures", pattern)?;
    let Some(captures) = regex.captures(text) else {
        return Ok(Vec::new());
    };

    let mut groups = Vec::with_capacity(captures.len());
    for index in 0..captures.len() {
        let group_value = match captures.get(index) {
            Some(group) => Value::string(group.as_str().to_string()),
            None => Value::Null,
        };
        groups.push(group_value);
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program, Vm};

    #[test]
    fn regex_cache_reuses_a_compiled_pattern_across_builtin_calls() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));

        assert!(builtin_re_match_impl(&mut vm, "(?i)^foo$", "FoO").expect("match should work"));
        assert_eq!(
            builtin_re_find_impl(&mut vm, "(?i)^foo$", "FoO").expect("find should work"),
            Some("FoO".to_string())
        );

        assert_eq!(vm.regex_cache_entry_count(), 1);
        assert_eq!(vm.regex_cache_compile_count(), 1);
        assert_eq!(vm.regex_cache_hit_count(), 1);
    }

    #[test]
    fn regex_cache_is_bounded() {
        let mut cache = RegexCache::with_capacity(2);
        cache.get_or_compile("a").expect("pattern should compile");
        cache.get_or_compile("b").expect("pattern should compile");
        cache.get_or_compile("c").expect("pattern should compile");

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.compile_count(), 3);
    }

    #[test]
    fn vm_regex_cache_capacity_can_be_changed_and_shrinks_immediately() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        assert_eq!(vm.regex_cache_capacity(), DEFAULT_REGEX_CACHE_CAPACITY);

        builtin_re_match_impl(&mut vm, "a", "a").expect("pattern should compile");
        builtin_re_match_impl(&mut vm, "b", "b").expect("pattern should compile");
        builtin_re_match_impl(&mut vm, "c", "c").expect("pattern should compile");
        vm.set_regex_cache_capacity(1);

        assert_eq!(vm.regex_cache_capacity(), 1);
        assert_eq!(vm.regex_cache_entry_count(), 1);
        builtin_re_match_impl(&mut vm, "c", "c").expect("most recent pattern should remain");
        assert_eq!(vm.regex_cache_compile_count(), 3);
        assert_eq!(vm.regex_cache_hit_count(), 1);
    }

    #[test]
    fn zero_vm_regex_cache_capacity_disables_caching() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        vm.set_regex_cache_capacity(0);

        builtin_re_match_impl(&mut vm, "same", "same").expect("pattern should compile");
        builtin_re_match_impl(&mut vm, "same", "same").expect("pattern should compile again");

        assert_eq!(vm.regex_cache_capacity(), 0);
        assert_eq!(vm.regex_cache_entry_count(), 0);
        assert_eq!(vm.regex_cache_compile_count(), 2);
        assert_eq!(vm.regex_cache_hit_count(), 0);
    }
}
