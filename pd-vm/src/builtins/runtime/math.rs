use std::f64::consts::{E, PI, TAU};

use super::NumberValue;
use crate::vm::{VmError, VmResult};
use pd_host_function::pd_host_function;

fn same_number(
    value: NumberValue,
    int_op: impl FnOnce(i64) -> i64,
    float_op: impl FnOnce(f64) -> f64,
) -> NumberValue {
    match value {
        NumberValue::Int(value) => NumberValue::Int(int_op(value)),
        NumberValue::Float(value) => NumberValue::Float(float_op(value)),
    }
}

fn float_number(value: NumberValue, float_op: impl FnOnce(f64) -> f64) -> f64 {
    float_op(value.as_f64())
}

fn bool_number(
    value: NumberValue,
    int_op: impl FnOnce(i64) -> bool,
    float_op: impl FnOnce(f64) -> bool,
) -> bool {
    match value {
        NumberValue::Int(value) => int_op(value),
        NumberValue::Float(value) => float_op(value),
    }
}

fn binary_float_number(
    left: NumberValue,
    right: NumberValue,
    float_op: impl FnOnce(f64, f64) -> f64,
) -> f64 {
    float_op(left.as_f64(), right.as_f64())
}

#[pd_host_function(name = "math::pi")]
pub(super) fn builtin_math_pi_impl() -> f64 {
    PI
}

#[pd_host_function(name = "math::tau")]
pub(super) fn builtin_math_tau_impl() -> f64 {
    TAU
}

#[pd_host_function(name = "math::e")]
pub(super) fn builtin_math_e_impl() -> f64 {
    E
}

#[pd_host_function(name = "math::epsilon")]
pub(super) fn builtin_math_epsilon_impl() -> f64 {
    f64::EPSILON
}

#[pd_host_function(name = "math::inf")]
pub(super) fn builtin_math_inf_impl() -> f64 {
    f64::INFINITY
}

#[pd_host_function(name = "math::neg_inf")]
pub(super) fn builtin_math_neg_inf_impl() -> f64 {
    f64::NEG_INFINITY
}

#[pd_host_function(name = "math::nan")]
pub(super) fn builtin_math_nan_impl() -> f64 {
    f64::NAN
}

#[pd_host_function(name = "math::abs")]
pub(super) fn builtin_math_abs_impl(value: NumberValue) -> NumberValue {
    same_number(value, i64::wrapping_abs, f64::abs)
}

#[pd_host_function(name = "math::sqrt")]
pub(super) fn builtin_math_sqrt_impl(value: NumberValue) -> f64 {
    float_number(value, f64::sqrt)
}

#[pd_host_function(name = "math::cbrt")]
pub(super) fn builtin_math_cbrt_impl(value: NumberValue) -> f64 {
    float_number(value, f64::cbrt)
}

#[pd_host_function(name = "math::exp")]
pub(super) fn builtin_math_exp_impl(value: NumberValue) -> f64 {
    float_number(value, f64::exp)
}

#[pd_host_function(name = "math::exp2")]
pub(super) fn builtin_math_exp2_impl(value: NumberValue) -> f64 {
    float_number(value, f64::exp2)
}

#[pd_host_function(name = "math::ln")]
pub(super) fn builtin_math_ln_impl(value: NumberValue) -> f64 {
    float_number(value, f64::ln)
}

#[pd_host_function(name = "math::ln_1p")]
pub(super) fn builtin_math_ln_1p_impl(value: NumberValue) -> f64 {
    float_number(value, f64::ln_1p)
}

#[pd_host_function(name = "math::log2")]
pub(super) fn builtin_math_log2_impl(value: NumberValue) -> f64 {
    float_number(value, f64::log2)
}

#[pd_host_function(name = "math::log10")]
pub(super) fn builtin_math_log10_impl(value: NumberValue) -> f64 {
    float_number(value, f64::log10)
}

#[pd_host_function(name = "math::sin")]
pub(super) fn builtin_math_sin_impl(value: NumberValue) -> f64 {
    float_number(value, f64::sin)
}

#[pd_host_function(name = "math::cos")]
pub(super) fn builtin_math_cos_impl(value: NumberValue) -> f64 {
    float_number(value, f64::cos)
}

#[pd_host_function(name = "math::tan")]
pub(super) fn builtin_math_tan_impl(value: NumberValue) -> f64 {
    float_number(value, f64::tan)
}

#[pd_host_function(name = "math::asin")]
pub(super) fn builtin_math_asin_impl(value: NumberValue) -> f64 {
    float_number(value, f64::asin)
}

#[pd_host_function(name = "math::acos")]
pub(super) fn builtin_math_acos_impl(value: NumberValue) -> f64 {
    float_number(value, f64::acos)
}

#[pd_host_function(name = "math::atan")]
pub(super) fn builtin_math_atan_impl(value: NumberValue) -> f64 {
    float_number(value, f64::atan)
}

#[pd_host_function(name = "math::sinh")]
pub(super) fn builtin_math_sinh_impl(value: NumberValue) -> f64 {
    float_number(value, f64::sinh)
}

#[pd_host_function(name = "math::cosh")]
pub(super) fn builtin_math_cosh_impl(value: NumberValue) -> f64 {
    float_number(value, f64::cosh)
}

#[pd_host_function(name = "math::tanh")]
pub(super) fn builtin_math_tanh_impl(value: NumberValue) -> f64 {
    float_number(value, f64::tanh)
}

#[pd_host_function(name = "math::floor")]
pub(super) fn builtin_math_floor_impl(value: NumberValue) -> NumberValue {
    same_number(value, |value| value, f64::floor)
}

#[pd_host_function(name = "math::ceil")]
pub(super) fn builtin_math_ceil_impl(value: NumberValue) -> NumberValue {
    same_number(value, |value| value, f64::ceil)
}

#[pd_host_function(name = "math::round")]
pub(super) fn builtin_math_round_impl(value: NumberValue) -> NumberValue {
    same_number(value, |value| value, f64::round)
}

#[pd_host_function(name = "math::trunc")]
pub(super) fn builtin_math_trunc_impl(value: NumberValue) -> NumberValue {
    same_number(value, |value| value, f64::trunc)
}

#[pd_host_function(name = "math::fract")]
pub(super) fn builtin_math_fract_impl(value: NumberValue) -> f64 {
    match value {
        NumberValue::Int(_) => 0.0,
        NumberValue::Float(value) => value.fract(),
    }
}

#[pd_host_function(name = "math::signum")]
pub(super) fn builtin_math_signum_impl(value: NumberValue) -> NumberValue {
    same_number(value, i64::signum, f64::signum)
}

#[pd_host_function(name = "math::to_degrees")]
pub(super) fn builtin_math_to_degrees_impl(value: NumberValue) -> f64 {
    float_number(value, f64::to_degrees)
}

#[pd_host_function(name = "math::to_radians")]
pub(super) fn builtin_math_to_radians_impl(value: NumberValue) -> f64 {
    float_number(value, f64::to_radians)
}

#[pd_host_function(name = "math::is_nan")]
pub(super) fn builtin_math_is_nan_impl(value: NumberValue) -> bool {
    bool_number(value, |_| false, f64::is_nan)
}

#[pd_host_function(name = "math::is_infinite")]
pub(super) fn builtin_math_is_infinite_impl(value: NumberValue) -> bool {
    bool_number(value, |_| false, f64::is_infinite)
}

#[pd_host_function(name = "math::is_finite")]
pub(super) fn builtin_math_is_finite_impl(value: NumberValue) -> bool {
    bool_number(value, |_| true, f64::is_finite)
}

#[pd_host_function(name = "math::atan2")]
pub(super) fn builtin_math_atan2_impl(y: NumberValue, x: NumberValue) -> f64 {
    binary_float_number(y, x, f64::atan2)
}

#[pd_host_function(name = "math::powf")]
pub(super) fn builtin_math_powf_impl(value: NumberValue, exponent: NumberValue) -> f64 {
    binary_float_number(value, exponent, f64::powf)
}

#[pd_host_function(name = "math::powi")]
pub(super) fn builtin_math_powi_impl(value: NumberValue, exponent: i64) -> VmResult<f64> {
    let exponent = i32::try_from(exponent)
        .map_err(|_| VmError::HostError("math::powi exponent out of range for i32".to_string()))?;
    Ok(value.as_f64().powi(exponent))
}

#[pd_host_function(name = "math::hypot")]
pub(super) fn builtin_math_hypot_impl(left: NumberValue, right: NumberValue) -> f64 {
    binary_float_number(left, right, f64::hypot)
}

#[pd_host_function(name = "math::log")]
pub(super) fn builtin_math_log_impl(value: NumberValue, base: NumberValue) -> f64 {
    binary_float_number(value, base, f64::log)
}

#[pd_host_function(name = "math::min")]
pub(super) fn builtin_math_min_impl(left: NumberValue, right: NumberValue) -> NumberValue {
    match (left, right) {
        (NumberValue::Int(left), NumberValue::Int(right)) => NumberValue::Int(left.min(right)),
        (left, right) => NumberValue::Float(left.as_f64().min(right.as_f64())),
    }
}

#[pd_host_function(name = "math::max")]
pub(super) fn builtin_math_max_impl(left: NumberValue, right: NumberValue) -> NumberValue {
    match (left, right) {
        (NumberValue::Int(left), NumberValue::Int(right)) => NumberValue::Int(left.max(right)),
        (left, right) => NumberValue::Float(left.as_f64().max(right.as_f64())),
    }
}

#[pd_host_function(name = "math::copysign")]
pub(super) fn builtin_math_copysign_impl(value: NumberValue, sign: NumberValue) -> f64 {
    binary_float_number(value, sign, f64::copysign)
}

#[pd_host_function(name = "math::clamp")]
pub(super) fn builtin_math_clamp_impl(
    value: NumberValue,
    min: NumberValue,
    max: NumberValue,
) -> VmResult<NumberValue> {
    match (value, min, max) {
        (NumberValue::Int(value), NumberValue::Int(min), NumberValue::Int(max)) => {
            if min > max {
                return Err(VmError::HostError(
                    "math::clamp min must be <= max".to_string(),
                ));
            }
            Ok(NumberValue::Int(value.clamp(min, max)))
        }
        (value, min, max) => {
            let min = min.as_f64();
            let max = max.as_f64();
            if min.is_nan() || max.is_nan() || min > max {
                return Err(VmError::HostError(
                    "math::clamp bounds must be ordered numbers".to_string(),
                ));
            }
            Ok(NumberValue::Float(value.as_f64().clamp(min, max)))
        }
    }
}

#[pd_host_function(name = "math::mul_add")]
pub(super) fn builtin_math_mul_add_impl(
    left: NumberValue,
    right: NumberValue,
    addend: NumberValue,
) -> f64 {
    left.as_f64().mul_add(right.as_f64(), addend.as_f64())
}

#[cfg(test)]
mod tests {
    use super::{builtin_math_clamp, builtin_math_is_nan, builtin_math_powi, builtin_math_sqrt};
    use crate::builtins::runtime::typed::NumberValue;
    use crate::bytecode::Value;

    #[test]
    fn sqrt_converts_numeric_inputs_to_float() {
        let out = builtin_math_sqrt(&[Value::Int(9)]).expect("sqrt should succeed");
        assert_eq!(out, 3.0);
    }

    #[test]
    fn powi_requires_integer_exponents() {
        let err = builtin_math_powi(&[Value::Int(2), Value::Float(3.0)])
            .expect_err("powi should reject float exponents");
        assert!(matches!(err, crate::vm::VmError::TypeMismatch("int")));
    }

    #[test]
    fn clamp_rejects_inverted_integer_bounds() {
        let err = builtin_math_clamp(&[Value::Int(2), Value::Int(3), Value::Int(1)])
            .expect_err("clamp should reject inverted bounds");
        assert!(matches!(err, crate::vm::VmError::HostError(_)));
    }

    #[test]
    fn is_nan_reports_nan_inputs() {
        let out = builtin_math_is_nan(&[Value::Float(f64::NAN)]).expect("is_nan should succeed");
        assert!(out);
    }

    #[test]
    fn clamp_returns_number_value() {
        let out =
            builtin_math_clamp(&[Value::Int(2), Value::Int(0), Value::Int(5)]).expect("clamp");
        assert_eq!(out, NumberValue::Int(2));
    }
}
