macro_rules! declare_math_namespace {
    ($callback:ident) => {
        $callback! {
            module: math,
            namespace: "math",
            alias: "math",
            docs: "Numeric math builtin namespace.",
            runtime_supported_on_wasm: true,
            supports_regex_flags: false,
            members: [
                namespace_builtin!(MathPi, "pi", 0, builtin_math_pi, args_ref, "Return pi."),
                namespace_builtin!(MathTau, "tau", 0, builtin_math_tau, args_ref, "Return tau."),
                namespace_builtin!(
                    MathE,
                    "e",
                    0,
                    builtin_math_e,
                    args_ref,
                    "Return Euler's number."
                ),
                namespace_builtin!(
                    MathEpsilon,
                    "epsilon",
                    0,
                    builtin_math_epsilon,
                    args_ref,
                    "Return f64 epsilon."
                ),
                namespace_builtin!(
                    MathInf,
                    "inf",
                    0,
                    builtin_math_inf,
                    args_ref,
                    "Return positive infinity."
                ),
                namespace_builtin!(
                    MathNegInf,
                    "neg_inf",
                    0,
                    builtin_math_neg_inf,
                    args_ref,
                    "Return negative infinity."
                ),
                namespace_builtin!(MathNaN, "nan", 0, builtin_math_nan, args_ref, "Return NaN."),
                namespace_builtin!(
                    MathAbs,
                    "abs",
                    1,
                    builtin_math_abs,
                    args_ref,
                    "Absolute value."
                ),
                namespace_builtin!(
                    MathSqrt,
                    "sqrt",
                    1,
                    builtin_math_sqrt,
                    args_ref,
                    "Square root."
                ),
                namespace_builtin!(
                    MathCbrt,
                    "cbrt",
                    1,
                    builtin_math_cbrt,
                    args_ref,
                    "Cube root."
                ),
                namespace_builtin!(
                    MathExp,
                    "exp",
                    1,
                    builtin_math_exp,
                    args_ref,
                    "e raised to the value."
                ),
                namespace_builtin!(
                    MathExp2,
                    "exp2",
                    1,
                    builtin_math_exp2,
                    args_ref,
                    "2 raised to the value."
                ),
                namespace_builtin!(
                    MathLn,
                    "ln",
                    1,
                    builtin_math_ln,
                    args_ref,
                    "Natural logarithm."
                ),
                namespace_builtin!(
                    MathLn1p,
                    "ln_1p",
                    1,
                    builtin_math_ln_1p,
                    args_ref,
                    "Natural logarithm of 1 + value."
                ),
                namespace_builtin!(
                    MathLog2,
                    "log2",
                    1,
                    builtin_math_log2,
                    args_ref,
                    "Base-2 logarithm."
                ),
                namespace_builtin!(
                    MathLog10,
                    "log10",
                    1,
                    builtin_math_log10,
                    args_ref,
                    "Base-10 logarithm."
                ),
                namespace_builtin!(
                    MathSin,
                    "sin",
                    1,
                    builtin_math_sin,
                    args_ref,
                    "Sine."
                ),
                namespace_builtin!(
                    MathCos,
                    "cos",
                    1,
                    builtin_math_cos,
                    args_ref,
                    "Cosine."
                ),
                namespace_builtin!(
                    MathTan,
                    "tan",
                    1,
                    builtin_math_tan,
                    args_ref,
                    "Tangent."
                ),
                namespace_builtin!(
                    MathAsin,
                    "asin",
                    1,
                    builtin_math_asin,
                    args_ref,
                    "Arc sine."
                ),
                namespace_builtin!(
                    MathAcos,
                    "acos",
                    1,
                    builtin_math_acos,
                    args_ref,
                    "Arc cosine."
                ),
                namespace_builtin!(
                    MathAtan,
                    "atan",
                    1,
                    builtin_math_atan,
                    args_ref,
                    "Arc tangent."
                ),
                namespace_builtin!(
                    MathSinh,
                    "sinh",
                    1,
                    builtin_math_sinh,
                    args_ref,
                    "Hyperbolic sine."
                ),
                namespace_builtin!(
                    MathCosh,
                    "cosh",
                    1,
                    builtin_math_cosh,
                    args_ref,
                    "Hyperbolic cosine."
                ),
                namespace_builtin!(
                    MathTanh,
                    "tanh",
                    1,
                    builtin_math_tanh,
                    args_ref,
                    "Hyperbolic tangent."
                ),
                namespace_builtin!(
                    MathFloor,
                    "floor",
                    1,
                    builtin_math_floor,
                    args_ref,
                    "Round toward negative infinity."
                ),
                namespace_builtin!(
                    MathCeil,
                    "ceil",
                    1,
                    builtin_math_ceil,
                    args_ref,
                    "Round toward positive infinity."
                ),
                namespace_builtin!(
                    MathRound,
                    "round",
                    1,
                    builtin_math_round,
                    args_ref,
                    "Round to nearest integer."
                ),
                namespace_builtin!(
                    MathTrunc,
                    "trunc",
                    1,
                    builtin_math_trunc,
                    args_ref,
                    "Truncate fractional digits."
                ),
                namespace_builtin!(
                    MathFract,
                    "fract",
                    1,
                    builtin_math_fract,
                    args_ref,
                    "Fractional part."
                ),
                namespace_builtin!(
                    MathSignum,
                    "signum",
                    1,
                    builtin_math_signum,
                    args_ref,
                    "Sign of the number."
                ),
                namespace_builtin!(
                    MathToDegrees,
                    "to_degrees",
                    1,
                    builtin_math_to_degrees,
                    args_ref,
                    "Convert radians to degrees."
                ),
                namespace_builtin!(
                    MathToRadians,
                    "to_radians",
                    1,
                    builtin_math_to_radians,
                    args_ref,
                    "Convert degrees to radians."
                ),
                namespace_builtin!(
                    MathIsNaN,
                    "is_nan",
                    1,
                    builtin_math_is_nan,
                    args_ref,
                    "Check for NaN."
                ),
                namespace_builtin!(
                    MathIsInfinite,
                    "is_infinite",
                    1,
                    builtin_math_is_infinite,
                    args_ref,
                    "Check for infinity."
                ),
                namespace_builtin!(
                    MathIsFinite,
                    "is_finite",
                    1,
                    builtin_math_is_finite,
                    args_ref,
                    "Check for finite numbers."
                ),
                namespace_builtin!(
                    MathAtan2,
                    "atan2",
                    2,
                    builtin_math_atan2,
                    args_ref,
                    "Arc tangent of y/x using both signs."
                ),
                namespace_builtin!(
                    MathPowF,
                    "powf",
                    2,
                    builtin_math_powf,
                    args_ref,
                    "Raise value to a floating-point exponent."
                ),
                namespace_builtin!(
                    MathPowI,
                    "powi",
                    2,
                    builtin_math_powi,
                    args_ref,
                    "Raise value to an integer exponent."
                ),
                namespace_builtin!(
                    MathHypot,
                    "hypot",
                    2,
                    builtin_math_hypot,
                    args_ref,
                    "Euclidean length of a right triangle."
                ),
                namespace_builtin!(
                    MathLog,
                    "log",
                    2,
                    builtin_math_log,
                    args_ref,
                    "Logarithm in the provided base."
                ),
                namespace_builtin!(
                    MathMin,
                    "min",
                    2,
                    builtin_math_min,
                    args_ref,
                    "Minimum numeric value."
                ),
                namespace_builtin!(
                    MathMax,
                    "max",
                    2,
                    builtin_math_max,
                    args_ref,
                    "Maximum numeric value."
                ),
                namespace_builtin!(
                    MathCopySign,
                    "copysign",
                    2,
                    builtin_math_copysign,
                    args_ref,
                    "Return value with the sign of another number."
                ),
                namespace_builtin!(
                    MathClamp,
                    "clamp",
                    3,
                    builtin_math_clamp,
                    args_ref,
                    "Clamp value between min and max."
                ),
                namespace_builtin!(
                    MathMulAdd,
                    "mul_add",
                    3,
                    builtin_math_mul_add,
                    args_ref,
                    "Fused multiply-add."
                ),
            ],
        }
    };
}
