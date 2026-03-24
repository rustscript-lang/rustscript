namespace PdVm.Runtime;

public static class PdVmOps
{
    public static PdVmValue Add(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            return PdVmValue.FromInt(unchecked(lhs.IntValue + rhs.IntValue));
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromFloat(left + right);
        }

        if (lhs.Kind == PdVmValueKind.String && rhs.Kind == PdVmValueKind.String)
        {
            return PdVmValue.FromString(lhs.AsString() + rhs.AsString());
        }

        if (lhs.Kind == PdVmValueKind.Bytes && rhs.Kind == PdVmValueKind.Bytes)
        {
            var output = new byte[lhs.AsBytes().Length + rhs.AsBytes().Length];
            Buffer.BlockCopy(lhs.AsBytes(), 0, output, 0, lhs.AsBytes().Length);
            Buffer.BlockCopy(rhs.AsBytes(), 0, output, lhs.AsBytes().Length, rhs.AsBytes().Length);
            return PdVmValue.FromBytes(output);
        }

        if (lhs.Kind == PdVmValueKind.Array && rhs.Kind == PdVmValueKind.Array)
        {
            var output = new List<PdVmValue>(lhs.AsArray());
            output.AddRange(rhs.AsArray());
            return PdVmValue.FromArray(output);
        }

        throw new InvalidOperationException($"add unsupported for {lhs.Kind} + {rhs.Kind}");
    }

    public static PdVmValue Sub(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            return PdVmValue.FromInt(unchecked(lhs.IntValue - rhs.IntValue));
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromFloat(left - right);
        }

        throw new InvalidOperationException($"sub unsupported for {lhs.Kind} - {rhs.Kind}");
    }

    public static PdVmValue Mul(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            return PdVmValue.FromInt(unchecked(lhs.IntValue * rhs.IntValue));
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromFloat(left * right);
        }

        throw new InvalidOperationException($"mul unsupported for {lhs.Kind} * {rhs.Kind}");
    }

    public static PdVmValue Div(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            if (rhs.IntValue == 0)
            {
                throw new InvalidOperationException("division by zero");
            }

            if (lhs.IntValue == long.MinValue && rhs.IntValue == -1)
            {
                throw new InvalidOperationException("integer overflow in division");
            }

            return PdVmValue.FromInt(lhs.IntValue / rhs.IntValue);
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromFloat(left / right);
        }

        throw new InvalidOperationException($"div unsupported for {lhs.Kind} / {rhs.Kind}");
    }

    public static PdVmValue Mod(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            if (rhs.IntValue == 0)
            {
                throw new InvalidOperationException("division by zero");
            }

            if (lhs.IntValue == long.MinValue && rhs.IntValue == -1)
            {
                throw new InvalidOperationException("integer overflow in remainder");
            }

            return PdVmValue.FromInt(lhs.IntValue % rhs.IntValue);
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromFloat(left % right);
        }

        throw new InvalidOperationException($"mod unsupported for {lhs.Kind} % {rhs.Kind}");
    }

    public static PdVmValue Neg(PdVmValue value)
    {
        return value.Kind switch
        {
            PdVmValueKind.Int => PdVmValue.FromInt(unchecked(-value.IntValue)),
            PdVmValueKind.Float => PdVmValue.FromFloat(-value.FloatValue),
            _ => throw new InvalidOperationException($"neg unsupported for {value.Kind}"),
        };
    }

    public static PdVmValue Not(PdVmValue value)
    {
        if (value.Kind != PdVmValueKind.Bool)
        {
            throw new InvalidOperationException($"not unsupported for {value.Kind}");
        }

        return PdVmValue.FromBool(!value.BoolValue);
    }

    public static PdVmValue Ceq(PdVmValue lhs, PdVmValue rhs) => PdVmValue.FromBool(lhs.Equals(rhs));

    public static PdVmValue Clt(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            return PdVmValue.FromBool(lhs.IntValue < rhs.IntValue);
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromBool(left < right);
        }

        throw new InvalidOperationException($"clt unsupported for {lhs.Kind} < {rhs.Kind}");
    }

    public static PdVmValue Cgt(PdVmValue lhs, PdVmValue rhs)
    {
        if (lhs.Kind == PdVmValueKind.Int && rhs.Kind == PdVmValueKind.Int)
        {
            return PdVmValue.FromBool(lhs.IntValue > rhs.IntValue);
        }

        if (TryGetNumericPair(lhs, rhs, out var left, out var right))
        {
            return PdVmValue.FromBool(left > right);
        }

        throw new InvalidOperationException($"cgt unsupported for {lhs.Kind} > {rhs.Kind}");
    }

    public static PdVmValue Shl(PdVmValue lhs, PdVmValue rhs)
    {
        return PdVmValue.FromInt(unchecked(lhs.AsInt() << GetShiftAmount(rhs)));
    }

    public static PdVmValue Shr(PdVmValue lhs, PdVmValue rhs)
    {
        return PdVmValue.FromInt(unchecked(lhs.AsInt() >> GetShiftAmount(rhs)));
    }

    public static PdVmValue Lshr(PdVmValue lhs, PdVmValue rhs)
    {
        var amount = GetShiftAmount(rhs);
        return PdVmValue.FromInt(unchecked((long)((ulong)lhs.AsInt() >> amount)));
    }

    public static PdVmValue And(PdVmValue lhs, PdVmValue rhs) => PdVmValue.FromBool(lhs.AsBool() && rhs.AsBool());

    public static PdVmValue Or(PdVmValue lhs, PdVmValue rhs) => PdVmValue.FromBool(lhs.AsBool() || rhs.AsBool());

    private static bool TryGetNumericPair(PdVmValue lhs, PdVmValue rhs, out double left, out double right)
    {
        left = default;
        right = default;
        if (!TryGetNumber(lhs, out left) || !TryGetNumber(rhs, out right))
        {
            return false;
        }

        return true;
    }

    private static bool TryGetNumber(PdVmValue value, out double number)
    {
        switch (value.Kind)
        {
            case PdVmValueKind.Int:
                number = value.IntValue;
                return true;
            case PdVmValueKind.Float:
                number = value.FloatValue;
                return true;
            default:
                number = default;
                return false;
        }
    }

    private static int GetShiftAmount(PdVmValue value)
    {
        var amount = value.AsInt();
        if (amount < 0 || amount > 63)
        {
            throw new InvalidOperationException($"invalid shift amount {amount}, expected 0..63");
        }

        return (int)amount;
    }
}
