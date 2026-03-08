// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.
use std::task::{Context, Poll};

use crate::builtins::BuiltinFunction;

use super::{HostOpId, Value, Vm, VmError, VmResult};

mod core;
#[cfg(not(target_arch = "wasm32"))]
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
mod math;
pub(crate) mod print;
mod regex;
mod runtime;

#[cfg(target_arch = "wasm32")]
use io_wasm as io;

pub(in crate::vm) use io::IoState;

pub(super) enum BuiltinCallOutcome {
    Return(Vec<Value>),
    Pending(HostOpId),
}

pub(crate) fn register_default_host_functions(registry: &mut super::HostFunctionRegistry) {
    runtime::register_default_host_functions(registry);
}

pub(crate) fn bind_default_host_function(vm: &mut Vm, name: &str) -> bool {
    runtime::bind_default_host_function(vm, name)
}

pub(crate) fn register_builtin_namespaces(
    registry: &mut crate::builtins::BuiltinNamespaceRegistry,
) {
    io::register_builtin_namespace(registry);
    regex::register_builtin_namespace(registry);
    json::register_builtin_namespace(registry);
    jit::register_builtin_namespace(registry);
    math::register_builtin_namespace(registry);
}

pub(super) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => core::builtin_len(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => core::builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => core::builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(vec![Value::array(Vec::new())])),
        BuiltinFunction::ArrayPush => {
            core::builtin_array_push(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(vec![Value::map(Vec::new())])),
        BuiltinFunction::Get => core::builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => core::builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => core::builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::IoOpen => io::builtin_io_open(vm, args),
        BuiltinFunction::IoPopen => io::builtin_io_popen(vm, args),
        BuiltinFunction::IoReadAll => io::builtin_io_read_all(vm, args),
        BuiltinFunction::IoReadLine => io::builtin_io_read_line(vm, args),
        BuiltinFunction::IoWrite => io::builtin_io_write(vm, args),
        BuiltinFunction::IoFlush => io::builtin_io_flush(vm, args),
        BuiltinFunction::IoClose => io::builtin_io_close(vm, args),
        BuiltinFunction::IoExists => io::builtin_io_exists(vm, args),
        BuiltinFunction::Count => core::builtin_count(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReIsMatch => {
            regex::builtin_re_is_match(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ReFind => regex::builtin_re_find(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReReplace => {
            regex::builtin_re_replace(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ReSplit => regex::builtin_re_split(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReCaptures => {
            regex::builtin_re_captures(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JsonEncode => {
            json::builtin_json_encode(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JsonDecode => {
            json::builtin_json_decode(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JitSetConfig => jit::builtin_jit_set_config(vm, args),
        BuiltinFunction::JitGetConfig => jit::builtin_jit_get_config(vm),
        BuiltinFunction::JitSetEnabled => jit::builtin_jit_set_enabled(vm, args),
        BuiltinFunction::JitGetEnabled => jit::builtin_jit_get_enabled(vm),
        BuiltinFunction::JitSetHotLoopThreshold => {
            jit::builtin_jit_set_hot_loop_threshold(vm, args)
        }
        BuiltinFunction::JitGetHotLoopThreshold => jit::builtin_jit_get_hot_loop_threshold(vm),
        BuiltinFunction::JitSetMaxTraceLen => jit::builtin_jit_set_max_trace_len(vm, args),
        BuiltinFunction::JitGetMaxTraceLen => jit::builtin_jit_get_max_trace_len(vm),
        BuiltinFunction::MathPi => math::builtin_math_pi(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathTau => math::builtin_math_tau(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathE => math::builtin_math_e(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathEpsilon => {
            math::builtin_math_epsilon(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathInf => math::builtin_math_inf(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathNegInf => {
            math::builtin_math_neg_inf(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathNaN => math::builtin_math_nan(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathAbs => math::builtin_math_abs(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathSqrt => math::builtin_math_sqrt(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathCbrt => math::builtin_math_cbrt(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathExp => math::builtin_math_exp(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathExp2 => math::builtin_math_exp2(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathLn => math::builtin_math_ln(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathLn1p => {
            math::builtin_math_ln_1p(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathLog2 => math::builtin_math_log2(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathLog10 => {
            math::builtin_math_log10(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathSin => math::builtin_math_sin(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathCos => math::builtin_math_cos(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathTan => math::builtin_math_tan(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathAsin => math::builtin_math_asin(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathAcos => math::builtin_math_acos(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathAtan => math::builtin_math_atan(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathSinh => math::builtin_math_sinh(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathCosh => math::builtin_math_cosh(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathTanh => math::builtin_math_tanh(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathFloor => {
            math::builtin_math_floor(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathCeil => math::builtin_math_ceil(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathRound => {
            math::builtin_math_round(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathTrunc => {
            math::builtin_math_trunc(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathFract => {
            math::builtin_math_fract(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathSignum => {
            math::builtin_math_signum(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathToDegrees => {
            math::builtin_math_to_degrees(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathToRadians => {
            math::builtin_math_to_radians(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathIsNaN => {
            math::builtin_math_is_nan(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathIsInfinite => {
            math::builtin_math_is_infinite(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathIsFinite => {
            math::builtin_math_is_finite(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathAtan2 => {
            math::builtin_math_atan2(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathPowF => math::builtin_math_powf(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathPowI => math::builtin_math_powi(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathHypot => {
            math::builtin_math_hypot(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathLog => math::builtin_math_log(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathMin => math::builtin_math_min(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathMax => math::builtin_math_max(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MathCopySign => {
            math::builtin_math_copysign(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathClamp => {
            math::builtin_math_clamp(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MathMulAdd => {
            math::builtin_math_mul_add(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::FormatTemplate => {
            core::builtin_format_template(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ToString => core::builtin_to_string(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::TypeOf => core::builtin_type_of(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Assert => core::builtin_assert(&args).map(BuiltinCallOutcome::Return),
    }
}

pub(super) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<Vec<Value>>> {
    io::poll_builtin_io_op(vm, op_id, cx)
}

pub(super) fn close_all_handles(vm: &mut Vm) {
    io::close_all_handles(vm);
}

fn arg_string<'a>(args: &'a [Value], index: usize, label: &str) -> VmResult<&'a str> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(VmError::TypeMismatch("string")),
        None => Err(VmError::HostError(format!("missing argument: {label}"))),
    }
}
