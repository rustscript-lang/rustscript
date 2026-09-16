//! Standard `re::*` host functions.
//!
//! Every host here resolves its compiled-pattern cache from the VM's generic
//! host-private state table as a hidden `HostStateMut<'_, RegexCache>`
//! parameter: the cache is one mutable per-VM instance, invisible to guest
//! arity, `HostFunctionSchema`, and catalog fingerprints. The interpreter
//! hosts and the native/JIT/AOT shortcuts (`native_re_match`,
//! `native_re_replace`) resolve the same instance, so LRU order, capacity, and
//! hit/compile counters are shared across execution modes.
//!
//! Configuration and statistics live with the cache in
//! [`cache::RegexCacheVmExt`]; `src/vm/**` owns none of them.

use std::sync::Arc;

use regex::Regex;

use super::VmArray;
use crate::vm::{HostStateMut, Value, Vm, VmError, VmResult};
use pd_host_function::pd_host_function;

mod cache;

pub(crate) use cache::with_regex_cache;
pub use cache::{DEFAULT_REGEX_CACHE_CAPACITY, RegexCache, RegexCacheVmExt};

/// Operation label for the shared interpreter/native match path.
const RE_MATCH_OPERATION: &str = "re_match";

/// Operation label for the shared interpreter/native replace path.
const RE_REPLACE_OPERATION: &str = "re_replace";

/// Compiles (or reuses) `pattern` in this VM's private cache.
fn cached_regex(cache: &mut RegexCache, operation: &str, pattern: &str) -> VmResult<Arc<Regex>> {
    cache
        .get_or_compile(pattern)
        .map_err(|err| VmError::HostError(format!("{operation} invalid pattern: {err}")))
}

/// Returns whether a regular expression matches the input text.
#[pd_host_function(name = "re::match")]
pub(super) fn builtin_re_match(
    mut cache: HostStateMut<'_, RegexCache>,
    pattern: &str,
    text: &str,
) -> VmResult<bool> {
    let regex = cached_regex(&mut cache, "re_match", pattern)?;
    Ok(regex.is_match(text))
}

pub(crate) fn native_re_match(vm: &mut Vm, pattern: &str, text: &str) -> VmResult<bool> {
    with_regex_cache(vm, RE_MATCH_OPERATION, |cache| {
        builtin_re_match_impl(cache, pattern, text)
    })
}

/// Returns the first substring matched by a regular expression.
#[pd_host_function(name = "re::find")]
pub(super) fn builtin_re_find(
    mut cache: HostStateMut<'_, RegexCache>,
    pattern: &str,
    text: &str,
) -> VmResult<Option<String>> {
    let regex = cached_regex(&mut cache, "re_find", pattern)?;
    Ok(regex.find(text).map(|matched| matched.as_str().to_string()))
}

/// Replaces all regular-expression matches in a string.
#[pd_host_function(name = "re::replace")]
pub(super) fn builtin_re_replace(
    mut cache: HostStateMut<'_, RegexCache>,
    pattern: &str,
    text: &str,
    replacement: &str,
) -> VmResult<String> {
    let regex = cached_regex(&mut cache, "re_replace", pattern)?;
    Ok(regex.replace_all(text, replacement).into_owned())
}

pub(crate) fn native_re_replace(
    vm: &mut Vm,
    pattern: &str,
    text: &str,
    replacement: &str,
) -> VmResult<String> {
    with_regex_cache(vm, RE_REPLACE_OPERATION, |cache| {
        builtin_re_replace_impl(cache, pattern, text, replacement)
    })
}

/// Splits a string on regular-expression matches.
#[pd_host_function(name = "re::split")]
pub(super) fn builtin_re_split(
    mut cache: HostStateMut<'_, RegexCache>,
    pattern: &str,
    text: &str,
) -> VmResult<VmArray> {
    let regex = cached_regex(&mut cache, "re_split", pattern)?;
    Ok(regex
        .split(text)
        .map(|part| Value::string(part.to_string()))
        .collect::<Vec<_>>())
}

/// Returns the capture groups produced by the first regular-expression match.
#[pd_host_function(name = "re::captures")]
pub(super) fn builtin_re_captures(
    mut cache: HostStateMut<'_, RegexCache>,
    pattern: &str,
    text: &str,
) -> VmResult<VmArray> {
    let regex = cached_regex(&mut cache, "re_captures", pattern)?;
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
    use super::cache::REGEX_CACHE_EFFECT;
    use super::*;
    use crate::{OpCode, Program, Vm};

    fn empty_vm() -> Vm {
        Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
    }

    #[test]
    fn regex_cache_reuses_a_compiled_pattern_across_builtin_calls() {
        let mut vm = empty_vm();

        assert!(
            native_re_match(&mut vm, "(?i)^foo$", "FoO").expect("match should work"),
            "the pattern must match"
        );
        assert_eq!(
            with_regex_cache(&mut vm, "re_find", |cache| builtin_re_find_impl(
                cache,
                "(?i)^foo$",
                "FoO"
            ))
            .expect("find should work"),
            Some("FoO".to_string())
        );

        assert_eq!(vm.regex_cache_entry_count(), 1);
        assert_eq!(vm.regex_cache_compile_count(), 1);
        assert_eq!(vm.regex_cache_hit_count(), 1);
    }

    #[test]
    fn vm_regex_cache_capacity_can_be_changed_and_shrinks_immediately() {
        let mut vm = empty_vm();
        assert_eq!(vm.regex_cache_capacity(), DEFAULT_REGEX_CACHE_CAPACITY);
        assert_eq!(
            vm.regex_cache_entry_count(),
            0,
            "capacity inspection must not force a cache into existence"
        );

        native_re_match(&mut vm, "a", "a").expect("pattern should compile");
        native_re_match(&mut vm, "b", "b").expect("pattern should compile");
        native_re_match(&mut vm, "c", "c").expect("pattern should compile");
        vm.set_regex_cache_capacity(1)
            .expect("capacity change must resolve the cache");

        assert_eq!(vm.regex_cache_capacity(), 1);
        assert_eq!(vm.regex_cache_entry_count(), 1);
        native_re_match(&mut vm, "c", "c").expect("most recent pattern should remain");
        assert_eq!(vm.regex_cache_compile_count(), 3);
        assert_eq!(vm.regex_cache_hit_count(), 1);
    }

    #[test]
    fn zero_vm_regex_cache_capacity_disables_caching() {
        let mut vm = empty_vm();
        {
            let mut context = vm.host_context();
            context
                .ensure_host_state::<RegexCache>("re::match", REGEX_CACHE_EFFECT)
                .expect("regex cache must resolve");
            let mut cache = context
                .host_state_mut::<RegexCache>("re::match", REGEX_CACHE_EFFECT)
                .expect("regex cache must be borrowable");
            cache.set_capacity(0);
        }

        native_re_match(&mut vm, "same", "same").expect("pattern should compile");
        native_re_match(&mut vm, "same", "same").expect("pattern should compile again");

        assert_eq!(vm.regex_cache_capacity(), 0);
        assert_eq!(vm.regex_cache_entry_count(), 0);
        assert_eq!(vm.regex_cache_compile_count(), 2);
        assert_eq!(vm.regex_cache_hit_count(), 0);
    }

    #[test]
    fn invalid_patterns_report_the_operation_and_stay_uncached() {
        let mut vm = empty_vm();
        let error = native_re_match(&mut vm, "(", "text")
            .expect_err("an invalid pattern must fail as a host error");
        assert!(
            matches!(error, VmError::HostError(ref detail) if detail.contains("re_match invalid pattern")),
            "unexpected error: {error:?}"
        );
        assert_eq!(vm.regex_cache_entry_count(), 0);
        assert_eq!(vm.regex_cache_compile_count(), 0);
    }

    #[test]
    fn regex_cache_is_isolated_between_vms_and_survives_reuse() {
        let mut first = empty_vm();
        let second = empty_vm();

        native_re_match(&mut first, "a", "a").expect("pattern should compile");
        assert_eq!(first.regex_cache_compile_count(), 1);
        assert_eq!(
            second.regex_cache_compile_count(),
            0,
            "each VM owns one private cache"
        );

        first.reset_for_reuse().expect("reset must succeed");
        assert_eq!(
            first.regex_cache_compile_count(),
            1,
            "reuse must not clear the regex cache"
        );
        assert_eq!(first.regex_cache_entry_count(), 1);
    }
}
