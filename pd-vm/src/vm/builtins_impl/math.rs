use std::f64::consts::{E, PI, TAU};

use super::super::{Value, VmError, VmResult};

#[derive(Clone, Copy, Debug, PartialEq)]
enum NumericInput {
    Int(i64),
    Float(f64),
}

impl NumericInput {
    fn as_f64(self) -> f64 {
        match self {
            NumericInput::Int(value) => value as f64,
            NumericInput::Float(value) => value,
        }
    }
}

fn missing_arg(label: &str) -> VmError {
    VmError::HostError(format!("missing argument: {label}"))
}

fn arg_numeric(args: &[Value], index: usize, label: &str) -> VmResult<NumericInput> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(NumericInput::Int(*value)),
        Some(Value::Float(value)) => Ok(NumericInput::Float(*value)),
        Some(_) => Err(VmError::TypeMismatch("number")),
        None => Err(missing_arg(label)),
    }
}

fn arg_float(args: &[Value], index: usize, label: &str) -> VmResult<f64> {
    Ok(arg_numeric(args, index, label)?.as_f64())
}

fn arg_int(args: &[Value], index: usize, label: &str) -> VmResult<i64> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        Some(_) => Err(VmError::TypeMismatch("int")),
        None => Err(missing_arg(label)),
    }
}

fn unary_same_numeric(
    args: &[Value],
    label: &str,
    int_op: impl FnOnce(i64) -> i64,
    float_op: impl FnOnce(f64) -> f64,
) -> VmResult<Vec<Value>> {
    let value = arg_numeric(args, 0, label)?;
    Ok(vec![match value {
        NumericInput::Int(value) => Value::Int(int_op(value)),
        NumericInput::Float(value) => Value::Float(float_op(value)),
    }])
}

fn unary_float(
    args: &[Value],
    label: &str,
    float_op: impl FnOnce(f64) -> f64,
) -> VmResult<Vec<Value>> {
    let value = arg_float(args, 0, label)?;
    Ok(vec![Value::Float(float_op(value))])
}

fn unary_bool(
    args: &[Value],
    label: &str,
    int_op: impl FnOnce(i64) -> bool,
    float_op: impl FnOnce(f64) -> bool,
) -> VmResult<Vec<Value>> {
    let value = arg_numeric(args, 0, label)?;
    Ok(vec![Value::Bool(match value {
        NumericInput::Int(value) => int_op(value),
        NumericInput::Float(value) => float_op(value),
    })])
}

fn binary_float(
    args: &[Value],
    lhs_label: &str,
    rhs_label: &str,
    float_op: impl FnOnce(f64, f64) -> f64,
) -> VmResult<Vec<Value>> {
    let lhs = arg_float(args, 0, lhs_label)?;
    let rhs = arg_float(args, 1, rhs_label)?;
    Ok(vec![Value::Float(float_op(lhs, rhs))])
}

pub(super) fn builtin_math_pi(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(PI)])
}

pub(super) fn builtin_math_tau(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(TAU)])
}

pub(super) fn builtin_math_e(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(E)])
}

pub(super) fn builtin_math_epsilon(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(f64::EPSILON)])
}

pub(super) fn builtin_math_inf(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(f64::INFINITY)])
}

pub(super) fn builtin_math_neg_inf(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(f64::NEG_INFINITY)])
}

pub(super) fn builtin_math_nan(_args: &[Value]) -> VmResult<Vec<Value>> {
    Ok(vec![Value::Float(f64::NAN)])
}

pub(super) fn builtin_math_abs(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::abs value", i64::wrapping_abs, f64::abs)
}

pub(super) fn builtin_math_sqrt(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::sqrt value", f64::sqrt)
}

pub(super) fn builtin_math_cbrt(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::cbrt value", f64::cbrt)
}

pub(super) fn builtin_math_exp(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::exp value", f64::exp)
}

pub(super) fn builtin_math_exp2(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::exp2 value", f64::exp2)
}

pub(super) fn builtin_math_ln(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::ln value", f64::ln)
}

pub(super) fn builtin_math_ln_1p(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::ln_1p value", f64::ln_1p)
}

pub(super) fn builtin_math_log2(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::log2 value", f64::log2)
}

pub(super) fn builtin_math_log10(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::log10 value", f64::log10)
}

pub(super) fn builtin_math_sin(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::sin value", f64::sin)
}

pub(super) fn builtin_math_cos(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::cos value", f64::cos)
}

pub(super) fn builtin_math_tan(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::tan value", f64::tan)
}

pub(super) fn builtin_math_asin(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::asin value", f64::asin)
}

pub(super) fn builtin_math_acos(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::acos value", f64::acos)
}

pub(super) fn builtin_math_atan(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::atan value", f64::atan)
}

pub(super) fn builtin_math_sinh(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::sinh value", f64::sinh)
}

pub(super) fn builtin_math_cosh(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::cosh value", f64::cosh)
}

pub(super) fn builtin_math_tanh(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::tanh value", f64::tanh)
}

pub(super) fn builtin_math_floor(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::floor value", |value| value, f64::floor)
}

pub(super) fn builtin_math_ceil(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::ceil value", |value| value, f64::ceil)
}

pub(super) fn builtin_math_round(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::round value", |value| value, f64::round)
}

pub(super) fn builtin_math_trunc(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::trunc value", |value| value, f64::trunc)
}

pub(super) fn builtin_math_fract(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = arg_numeric(args, 0, "math::fract value")?;
    Ok(vec![Value::Float(match value {
        NumericInput::Int(_) => 0.0,
        NumericInput::Float(value) => value.fract(),
    })])
}

pub(super) fn builtin_math_signum(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_same_numeric(args, "math::signum value", i64::signum, f64::signum)
}

pub(super) fn builtin_math_to_degrees(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::to_degrees value", f64::to_degrees)
}

pub(super) fn builtin_math_to_radians(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_float(args, "math::to_radians value", f64::to_radians)
}

pub(super) fn builtin_math_is_nan(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_bool(args, "math::is_nan value", |_| false, f64::is_nan)
}

pub(super) fn builtin_math_is_infinite(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_bool(args, "math::is_infinite value", |_| false, f64::is_infinite)
}

pub(super) fn builtin_math_is_finite(args: &[Value]) -> VmResult<Vec<Value>> {
    unary_bool(args, "math::is_finite value", |_| true, f64::is_finite)
}

pub(super) fn builtin_math_atan2(args: &[Value]) -> VmResult<Vec<Value>> {
    binary_float(args, "math::atan2 y", "math::atan2 x", f64::atan2)
}

pub(super) fn builtin_math_powf(args: &[Value]) -> VmResult<Vec<Value>> {
    binary_float(args, "math::powf value", "math::powf exponent", f64::powf)
}

pub(super) fn builtin_math_powi(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = arg_float(args, 0, "math::powi value")?;
    let exponent = arg_int(args, 1, "math::powi exponent")?;
    let exponent = i32::try_from(exponent)
        .map_err(|_| VmError::HostError("math::powi exponent out of range for i32".to_string()))?;
    Ok(vec![Value::Float(value.powi(exponent))])
}

pub(super) fn builtin_math_hypot(args: &[Value]) -> VmResult<Vec<Value>> {
    binary_float(args, "math::hypot lhs", "math::hypot rhs", f64::hypot)
}

pub(super) fn builtin_math_log(args: &[Value]) -> VmResult<Vec<Value>> {
    binary_float(args, "math::log value", "math::log base", f64::log)
}

pub(super) fn builtin_math_min(args: &[Value]) -> VmResult<Vec<Value>> {
    let lhs = arg_numeric(args, 0, "math::min lhs")?;
    let rhs = arg_numeric(args, 1, "math::min rhs")?;
    Ok(vec![match (lhs, rhs) {
        (NumericInput::Int(lhs), NumericInput::Int(rhs)) => Value::Int(lhs.min(rhs)),
        (lhs, rhs) => Value::Float(lhs.as_f64().min(rhs.as_f64())),
    }])
}

pub(super) fn builtin_math_max(args: &[Value]) -> VmResult<Vec<Value>> {
    let lhs = arg_numeric(args, 0, "math::max lhs")?;
    let rhs = arg_numeric(args, 1, "math::max rhs")?;
    Ok(vec![match (lhs, rhs) {
        (NumericInput::Int(lhs), NumericInput::Int(rhs)) => Value::Int(lhs.max(rhs)),
        (lhs, rhs) => Value::Float(lhs.as_f64().max(rhs.as_f64())),
    }])
}

pub(super) fn builtin_math_copysign(args: &[Value]) -> VmResult<Vec<Value>> {
    binary_float(
        args,
        "math::copysign value",
        "math::copysign sign",
        f64::copysign,
    )
}

pub(super) fn builtin_math_clamp(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = arg_numeric(args, 0, "math::clamp value")?;
    let lower = arg_numeric(args, 1, "math::clamp min")?;
    let upper = arg_numeric(args, 2, "math::clamp max")?;
    Ok(vec![match (value, lower, upper) {
        (NumericInput::Int(value), NumericInput::Int(lower), NumericInput::Int(upper)) => {
            if lower > upper {
                return Err(VmError::HostError(
                    "math::clamp min must be <= max".to_string(),
                ));
            }
            Value::Int(value.clamp(lower, upper))
        }
        (value, lower, upper) => {
            let lower = lower.as_f64();
            let upper = upper.as_f64();
            if lower.is_nan() || upper.is_nan() || lower > upper {
                return Err(VmError::HostError(
                    "math::clamp bounds must be ordered numbers".to_string(),
                ));
            }
            Value::Float(value.as_f64().clamp(lower, upper))
        }
    }])
}

pub(super) fn builtin_math_mul_add(args: &[Value]) -> VmResult<Vec<Value>> {
    let a = arg_float(args, 0, "math::mul_add a")?;
    let b = arg_float(args, 1, "math::mul_add b")?;
    let c = arg_float(args, 2, "math::mul_add c")?;
    Ok(vec![Value::Float(a.mul_add(b, c))])
}

#[cfg(test)]
mod tests {
    use super::{builtin_math_clamp, builtin_math_is_nan, builtin_math_powi, builtin_math_sqrt};
    use crate::bytecode::Value;

    #[test]
    fn sqrt_converts_numeric_inputs_to_float() {
        let out = builtin_math_sqrt(&[Value::Int(9)]).expect("sqrt should succeed");
        assert_eq!(out, vec![Value::Float(3.0)]);
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
        assert_eq!(out, vec![Value::Bool(true)]);
    }
}
