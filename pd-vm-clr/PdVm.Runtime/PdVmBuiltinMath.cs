namespace PdVm.Runtime;

internal static class PdVmBuiltinMath
{
    public static PdVmValue Dispatch(PdVmBuiltin builtin, IReadOnlyList<PdVmValue> args)
    {
        return builtin switch
        {
            PdVmBuiltin.MathPi => ReturnFloat(builtin, args, Math.PI),
            PdVmBuiltin.MathTau => ReturnFloat(builtin, args, Math.Tau),
            PdVmBuiltin.MathE => ReturnFloat(builtin, args, Math.E),
            PdVmBuiltin.MathEpsilon => ReturnFloat(builtin, args, double.Epsilon),
            PdVmBuiltin.MathInf => ReturnFloat(builtin, args, double.PositiveInfinity),
            PdVmBuiltin.MathNegInf => ReturnFloat(builtin, args, double.NegativeInfinity),
            PdVmBuiltin.MathNaN => ReturnFloat(builtin, args, double.NaN),
            PdVmBuiltin.MathAbs => SameNumber(builtin, args, value => unchecked(value < 0 ? -value : value), Math.Abs),
            PdVmBuiltin.MathSqrt => FloatNumber(builtin, args, Math.Sqrt),
            PdVmBuiltin.MathCbrt => FloatNumber(builtin, args, Math.Cbrt),
            PdVmBuiltin.MathExp => FloatNumber(builtin, args, Math.Exp),
            PdVmBuiltin.MathExp2 => FloatNumber(builtin, args, value => Math.Pow(2d, value)),
            PdVmBuiltin.MathLn => FloatNumber(builtin, args, Math.Log),
            PdVmBuiltin.MathLn1p => FloatNumber(builtin, args, value => Math.Log(1d + value)),
            PdVmBuiltin.MathLog2 => FloatNumber(builtin, args, Math.Log2),
            PdVmBuiltin.MathLog10 => FloatNumber(builtin, args, Math.Log10),
            PdVmBuiltin.MathSin => FloatNumber(builtin, args, Math.Sin),
            PdVmBuiltin.MathCos => FloatNumber(builtin, args, Math.Cos),
            PdVmBuiltin.MathTan => FloatNumber(builtin, args, Math.Tan),
            PdVmBuiltin.MathAsin => FloatNumber(builtin, args, Math.Asin),
            PdVmBuiltin.MathAcos => FloatNumber(builtin, args, Math.Acos),
            PdVmBuiltin.MathAtan => FloatNumber(builtin, args, Math.Atan),
            PdVmBuiltin.MathSinh => FloatNumber(builtin, args, Math.Sinh),
            PdVmBuiltin.MathCosh => FloatNumber(builtin, args, Math.Cosh),
            PdVmBuiltin.MathTanh => FloatNumber(builtin, args, Math.Tanh),
            PdVmBuiltin.MathFloor => SameNumber(builtin, args, value => value, Math.Floor),
            PdVmBuiltin.MathCeil => SameNumber(builtin, args, value => value, Math.Ceiling),
            PdVmBuiltin.MathRound => SameNumber(builtin, args, value => value, value => Math.Round(value, MidpointRounding.AwayFromZero)),
            PdVmBuiltin.MathTrunc => SameNumber(builtin, args, value => value, Math.Truncate),
            PdVmBuiltin.MathFract => DispatchFract(args),
            PdVmBuiltin.MathSignum => DispatchSignum(args),
            PdVmBuiltin.MathToDegrees => FloatNumber(builtin, args, value => value * (180d / Math.PI)),
            PdVmBuiltin.MathToRadians => FloatNumber(builtin, args, value => value * (Math.PI / 180d)),
            PdVmBuiltin.MathIsNaN => BoolNumber(builtin, args, _ => false, double.IsNaN),
            PdVmBuiltin.MathIsInfinite => BoolNumber(builtin, args, _ => false, double.IsInfinity),
            PdVmBuiltin.MathIsFinite => BoolNumber(builtin, args, _ => true, double.IsFinite),
            PdVmBuiltin.MathAtan2 => BinaryFloatNumber(builtin, args, Math.Atan2),
            PdVmBuiltin.MathPowF => BinaryFloatNumber(builtin, args, Math.Pow),
            PdVmBuiltin.MathPowI => DispatchPowI(args),
            PdVmBuiltin.MathHypot => BinaryFloatNumber(builtin, args, (left, right) => Math.Sqrt(left * left + right * right)),
            PdVmBuiltin.MathLog => BinaryFloatNumber(builtin, args, Math.Log),
            PdVmBuiltin.MathMin => DispatchMin(args),
            PdVmBuiltin.MathMax => DispatchMax(args),
            PdVmBuiltin.MathCopySign => BinaryFloatNumber(builtin, args, Math.CopySign),
            PdVmBuiltin.MathClamp => DispatchClamp(args),
            PdVmBuiltin.MathMulAdd => DispatchMulAdd(args),
            _ => throw new NotSupportedException($"builtin {builtin} is not a math builtin"),
        };
    }

    private static PdVmValue ReturnFloat(PdVmBuiltin builtin, IReadOnlyList<PdVmValue> args, double value)
    {
        EnsureArgCount(builtin, args, 0);
        return PdVmValue.FromFloat(value);
    }

    private static PdVmValue SameNumber(
        PdVmBuiltin builtin,
        IReadOnlyList<PdVmValue> args,
        Func<long, long> intOp,
        Func<double, double> floatOp)
    {
        EnsureArgCount(builtin, args, 1);
        var value = GetNumber(GetArg(args, 0));
        return value.IsInt
            ? PdVmValue.FromInt(intOp(value.IntValue))
            : PdVmValue.FromFloat(floatOp(value.FloatValue));
    }

    private static PdVmValue FloatNumber(
        PdVmBuiltin builtin,
        IReadOnlyList<PdVmValue> args,
        Func<double, double> floatOp)
    {
        EnsureArgCount(builtin, args, 1);
        return PdVmValue.FromFloat(floatOp(GetNumber(GetArg(args, 0)).AsDouble()));
    }

    private static PdVmValue BoolNumber(
        PdVmBuiltin builtin,
        IReadOnlyList<PdVmValue> args,
        Func<long, bool> intOp,
        Func<double, bool> floatOp)
    {
        EnsureArgCount(builtin, args, 1);
        var value = GetNumber(GetArg(args, 0));
        return PdVmValue.FromBool(value.IsInt ? intOp(value.IntValue) : floatOp(value.FloatValue));
    }

    private static PdVmValue BinaryFloatNumber(
        PdVmBuiltin builtin,
        IReadOnlyList<PdVmValue> args,
        Func<double, double, double> floatOp)
    {
        EnsureArgCount(builtin, args, 2);
        var left = GetNumber(GetArg(args, 0)).AsDouble();
        var right = GetNumber(GetArg(args, 1)).AsDouble();
        return PdVmValue.FromFloat(floatOp(left, right));
    }

    private static PdVmValue DispatchFract(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathFract, args, 1);
        var value = GetNumber(GetArg(args, 0));
        return PdVmValue.FromFloat(value.IsInt ? 0d : value.FloatValue - Math.Truncate(value.FloatValue));
    }

    private static PdVmValue DispatchSignum(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathSignum, args, 1);
        var value = GetNumber(GetArg(args, 0));
        if (value.IsInt)
        {
            return PdVmValue.FromInt(value.IntValue switch
            {
                > 0 => 1,
                < 0 => -1,
                _ => 0,
            });
        }

        if (double.IsNaN(value.FloatValue))
        {
            return PdVmValue.FromFloat(double.NaN);
        }

        return PdVmValue.FromFloat(HasNegativeSignBit(value.FloatValue) ? -1d : 1d);
    }

    private static PdVmValue DispatchPowI(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathPowI, args, 2);
        var value = GetNumber(GetArg(args, 0)).AsDouble();
        var exponent = GetArg(args, 1).AsInt();
        if (exponent is < int.MinValue or > int.MaxValue)
        {
            throw new InvalidOperationException("math::powi exponent out of range for i32");
        }

        return PdVmValue.FromFloat(Math.Pow(value, exponent));
    }

    private static PdVmValue DispatchMin(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathMin, args, 2);
        var left = GetNumber(GetArg(args, 0));
        var right = GetNumber(GetArg(args, 1));
        if (left.IsInt && right.IsInt)
        {
            return PdVmValue.FromInt(Math.Min(left.IntValue, right.IntValue));
        }

        return PdVmValue.FromFloat(RustMin(left.AsDouble(), right.AsDouble()));
    }

    private static PdVmValue DispatchMax(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathMax, args, 2);
        var left = GetNumber(GetArg(args, 0));
        var right = GetNumber(GetArg(args, 1));
        if (left.IsInt && right.IsInt)
        {
            return PdVmValue.FromInt(Math.Max(left.IntValue, right.IntValue));
        }

        return PdVmValue.FromFloat(RustMax(left.AsDouble(), right.AsDouble()));
    }

    private static PdVmValue DispatchClamp(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathClamp, args, 3);
        var value = GetNumber(GetArg(args, 0));
        var min = GetNumber(GetArg(args, 1));
        var max = GetNumber(GetArg(args, 2));
        if (value.IsInt && min.IsInt && max.IsInt)
        {
            if (min.IntValue > max.IntValue)
            {
                throw new InvalidOperationException("math::clamp min must be <= max");
            }

            return PdVmValue.FromInt(Math.Clamp(value.IntValue, min.IntValue, max.IntValue));
        }

        var minValue = min.AsDouble();
        var maxValue = max.AsDouble();
        if (double.IsNaN(minValue) || double.IsNaN(maxValue) || minValue > maxValue)
        {
            throw new InvalidOperationException("math::clamp bounds must be ordered numbers");
        }

        var input = value.AsDouble();
        var clamped = double.IsNaN(input)
            ? double.NaN
            : input < minValue ? minValue : input > maxValue ? maxValue : input;
        return PdVmValue.FromFloat(clamped);
    }

    private static PdVmValue DispatchMulAdd(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.MathMulAdd, args, 3);
        var left = GetNumber(GetArg(args, 0)).AsDouble();
        var right = GetNumber(GetArg(args, 1)).AsDouble();
        var addend = GetNumber(GetArg(args, 2)).AsDouble();
        return PdVmValue.FromFloat(left * right + addend);
    }

    private static NumberValue GetNumber(PdVmValue value)
    {
        return value.Kind switch
        {
            PdVmValueKind.Int => NumberValue.FromInt(value.IntValue),
            PdVmValueKind.Float => NumberValue.FromFloat(value.FloatValue),
            _ => throw new InvalidOperationException($"expected number, got {value.Kind}"),
        };
    }

    private static PdVmValue GetArg(IReadOnlyList<PdVmValue> args, int index)
    {
        if (index < 0 || index >= args.Count)
        {
            throw new InvalidOperationException($"missing builtin argument at index {index}");
        }

        return args[index];
    }

    private static void EnsureArgCount(PdVmBuiltin builtin, IReadOnlyList<PdVmValue> args, int expected)
    {
        if (args.Count != expected)
        {
            throw new InvalidOperationException($"builtin {builtin} expects {expected} arguments, got {args.Count}");
        }
    }

    private static bool HasNegativeSignBit(double value) =>
        (BitConverter.DoubleToInt64Bits(value) & (1L << 63)) != 0;

    private static double RustMin(double left, double right)
    {
        if (double.IsNaN(left))
        {
            return right;
        }

        if (double.IsNaN(right))
        {
            return left;
        }

        return Math.Min(left, right);
    }

    private static double RustMax(double left, double right)
    {
        if (double.IsNaN(left))
        {
            return right;
        }

        if (double.IsNaN(right))
        {
            return left;
        }

        return Math.Max(left, right);
    }

    private readonly record struct NumberValue(bool IsInt, long IntValue, double FloatValue)
    {
        public static NumberValue FromInt(long value) => new(true, value, value);

        public static NumberValue FromFloat(double value) => new(false, 0, value);

        public double AsDouble() => IsInt ? IntValue : FloatValue;
    }
}
