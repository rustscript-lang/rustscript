using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;

namespace PdVm.Runtime;

public enum PdVmBuiltin
{
    Len,
    Slice,
    Concat,
    ArrayNew,
    ArrayPush,
    MapNew,
    Get,
    Has,
    Set,
    Keys,
    BytesFromUtf8,
    BytesToUtf8,
    BytesToUtf8Lossy,
    BytesFromHex,
    BytesToHex,
    BytesFromBase64,
    BytesToBase64,
    BytesFromArrayU8,
    BytesToArrayU8,
    IoOpen,
    IoPopen,
    IoReadAll,
    IoReadLine,
    IoWrite,
    IoFlush,
    IoClose,
    IoExists,
    ReMatch,
    ReFind,
    ReReplace,
    ReSplit,
    ReCaptures,
    JsonEncode,
    JsonDecode,
    JitSetConfig,
    JitGetConfig,
    JitSetEnabled,
    JitGetEnabled,
    JitSetHotLoopThreshold,
    JitGetHotLoopThreshold,
    JitSetMaxTraceLen,
    JitGetMaxTraceLen,
    MathPi,
    MathTau,
    MathE,
    MathEpsilon,
    MathInf,
    MathNegInf,
    MathNaN,
    MathAbs,
    MathSqrt,
    MathCbrt,
    MathExp,
    MathExp2,
    MathLn,
    MathLn1p,
    MathLog2,
    MathLog10,
    MathSin,
    MathCos,
    MathTan,
    MathAsin,
    MathAcos,
    MathAtan,
    MathSinh,
    MathCosh,
    MathTanh,
    MathFloor,
    MathCeil,
    MathRound,
    MathTrunc,
    MathFract,
    MathSignum,
    MathToDegrees,
    MathToRadians,
    MathIsNaN,
    MathIsInfinite,
    MathIsFinite,
    MathAtan2,
    MathPowF,
    MathPowI,
    MathHypot,
    MathLog,
    MathMin,
    MathMax,
    MathCopySign,
    MathClamp,
    MathMulAdd,
    Count,
    FormatTemplate,
    ToString,
    TypeOf,
    Assert,
}

public static class PdVmBuiltins
{
    public const ushort BuiltinCallBase = 0xFFA3;
    public const ushort BuiltinCallCount = 89;

    public static PdVmValue LenValue(PdVmValue value) => DispatchLen(new[] { value });

    public static PdVmValue SliceValue(PdVmValue source, PdVmValue start, PdVmValue length) =>
        DispatchSlice(new[] { source, start, length });

    public static PdVmValue ConcatValue(PdVmValue lhs, PdVmValue rhs) => DispatchConcat(new[] { lhs, rhs });

    public static PdVmValue ArrayNewValue() => PdVmValue.FromArray(Array.Empty<PdVmValue>());

    public static PdVmValue ArrayPushValue(PdVmValue array, PdVmValue value) =>
        DispatchArrayPush(new[] { array, value });

    public static PdVmValue MapNewValue() => PdVmValue.FromMap(Array.Empty<KeyValuePair<PdVmValue, PdVmValue>>());

    public static PdVmValue GetValue(PdVmValue container, PdVmValue key) => DispatchGet(new[] { container, key });

    public static PdVmValue HasValue(PdVmValue container, PdVmValue key) => DispatchHas(new[] { container, key });

    public static PdVmValue SetValue(PdVmValue container, PdVmValue key, PdVmValue value) =>
        DispatchSet(new[] { container, key, value });

    public static PdVmValue KeysValue(PdVmValue container) => DispatchKeys(new[] { container });

    public static PdVmValue CountValue(PdVmValue container) => DispatchCount(new[] { container });

    public static PdVmValue FormatTemplateValue(PdVmValue template, PdVmValue values) =>
        PdVmValue.FromString(DispatchFormatTemplate(new[] { template, values }));

    public static PdVmValue ToStringValue(PdVmValue value) =>
        PdVmValue.FromString(PdVmValue.FormatDisplay(value));

    public static PdVmValue TypeOfValue(PdVmValue value) =>
        PdVmValue.FromString(value.Type.ToString().ToLowerInvariant());

    public static void AssertValue(PdVmValue value)
    {
        if (!value.AsBool())
        {
            throw new InvalidOperationException("assertion failed");
        }
    }

    public static PdVmValue BytesFromUtf8Value(PdVmValue value) =>
        PdVmValue.FromBytes(Encoding.UTF8.GetBytes(value.AsString()));

    public static PdVmValue BytesToUtf8Value(PdVmValue value) =>
        PdVmValue.FromString(DecodeUtf8Strict(value.AsBytes()));

    public static PdVmValue BytesToUtf8LossyValue(PdVmValue value) =>
        PdVmValue.FromString(Encoding.UTF8.GetString(value.AsBytes()));

    public static PdVmValue BytesFromHexValue(PdVmValue value) =>
        PdVmValue.FromBytes(ParseHex(value.AsString()));

    public static PdVmValue BytesToHexValue(PdVmValue value) =>
        PdVmValue.FromString(Convert.ToHexString(value.AsBytes()).ToLowerInvariant());

    public static PdVmValue BytesFromBase64Value(PdVmValue value) =>
        PdVmValue.FromBytes(Convert.FromBase64String(value.AsString()));

    public static PdVmValue BytesToBase64Value(PdVmValue value) =>
        PdVmValue.FromString(Convert.ToBase64String(value.AsBytes()));

    public static PdVmValue BytesFromArrayU8Value(PdVmValue value) =>
        PdVmValue.FromBytes(ToByteArray(value.AsArray()));

    public static PdVmValue BytesToArrayU8Value(PdVmValue value) =>
        PdVmValue.FromArray(value.AsBytes().Select(item => PdVmValue.FromInt(item)));

    public static ushort GetCallIndex(PdVmBuiltin builtin)
    {
        return builtin switch
        {
            PdVmBuiltin.FormatTemplate => (ushort)(BuiltinCallBase - 4),
            PdVmBuiltin.ToString => (ushort)(BuiltinCallBase - 3),
            PdVmBuiltin.TypeOf => (ushort)(BuiltinCallBase - 2),
            PdVmBuiltin.Assert => (ushort)(BuiltinCallBase - 1),
            _ when builtin <= PdVmBuiltin.Count => (ushort)(BuiltinCallBase + (ushort)builtin),
            _ => throw new ArgumentOutOfRangeException(nameof(builtin)),
        };
    }

    public static byte GetArity(PdVmBuiltin builtin)
    {
        return builtin switch
        {
            PdVmBuiltin.ArrayNew or PdVmBuiltin.MapNew
                or PdVmBuiltin.JitGetConfig
                or PdVmBuiltin.JitGetEnabled
                or PdVmBuiltin.JitGetHotLoopThreshold
                or PdVmBuiltin.JitGetMaxTraceLen
                or PdVmBuiltin.MathPi
                or PdVmBuiltin.MathTau
                or PdVmBuiltin.MathE
                or PdVmBuiltin.MathEpsilon
                or PdVmBuiltin.MathInf
                or PdVmBuiltin.MathNegInf
                or PdVmBuiltin.MathNaN => 0,
            PdVmBuiltin.Len
                or PdVmBuiltin.Keys
                or PdVmBuiltin.BytesFromUtf8
                or PdVmBuiltin.BytesToUtf8
                or PdVmBuiltin.BytesToUtf8Lossy
                or PdVmBuiltin.BytesFromHex
                or PdVmBuiltin.BytesToHex
                or PdVmBuiltin.BytesFromBase64
                or PdVmBuiltin.BytesToBase64
                or PdVmBuiltin.BytesFromArrayU8
                or PdVmBuiltin.BytesToArrayU8
                or PdVmBuiltin.IoReadAll
                or PdVmBuiltin.IoReadLine
                or PdVmBuiltin.IoFlush
                or PdVmBuiltin.IoClose
                or PdVmBuiltin.IoExists
                or PdVmBuiltin.JsonEncode
                or PdVmBuiltin.JsonDecode
                or PdVmBuiltin.JitSetEnabled
                or PdVmBuiltin.JitSetHotLoopThreshold
                or PdVmBuiltin.JitSetMaxTraceLen
                or PdVmBuiltin.MathAbs
                or PdVmBuiltin.MathSqrt
                or PdVmBuiltin.MathCbrt
                or PdVmBuiltin.MathExp
                or PdVmBuiltin.MathExp2
                or PdVmBuiltin.MathLn
                or PdVmBuiltin.MathLn1p
                or PdVmBuiltin.MathLog2
                or PdVmBuiltin.MathLog10
                or PdVmBuiltin.MathSin
                or PdVmBuiltin.MathCos
                or PdVmBuiltin.MathTan
                or PdVmBuiltin.MathAsin
                or PdVmBuiltin.MathAcos
                or PdVmBuiltin.MathAtan
                or PdVmBuiltin.MathSinh
                or PdVmBuiltin.MathCosh
                or PdVmBuiltin.MathTanh
                or PdVmBuiltin.MathFloor
                or PdVmBuiltin.MathCeil
                or PdVmBuiltin.MathRound
                or PdVmBuiltin.MathTrunc
                or PdVmBuiltin.MathFract
                or PdVmBuiltin.MathSignum
                or PdVmBuiltin.MathToDegrees
                or PdVmBuiltin.MathToRadians
                or PdVmBuiltin.MathIsNaN
                or PdVmBuiltin.MathIsInfinite
                or PdVmBuiltin.MathIsFinite
                or PdVmBuiltin.Count
                or PdVmBuiltin.ToString
                or PdVmBuiltin.TypeOf
                or PdVmBuiltin.Assert => 1,
            PdVmBuiltin.Concat
                or PdVmBuiltin.ArrayPush
                or PdVmBuiltin.Get
                or PdVmBuiltin.Has
                or PdVmBuiltin.IoOpen
                or PdVmBuiltin.IoPopen
                or PdVmBuiltin.IoWrite
                or PdVmBuiltin.ReMatch
                or PdVmBuiltin.ReFind
                or PdVmBuiltin.ReSplit
                or PdVmBuiltin.ReCaptures
                or PdVmBuiltin.MathAtan2
                or PdVmBuiltin.MathPowF
                or PdVmBuiltin.MathPowI
                or PdVmBuiltin.MathHypot
                or PdVmBuiltin.MathLog
                or PdVmBuiltin.MathMin
                or PdVmBuiltin.MathMax
                or PdVmBuiltin.MathCopySign
                or PdVmBuiltin.FormatTemplate => 2,
            PdVmBuiltin.Slice
                or PdVmBuiltin.Set
                or PdVmBuiltin.ReReplace
                or PdVmBuiltin.JitSetConfig
                or PdVmBuiltin.MathClamp
                or PdVmBuiltin.MathMulAdd => 3,
            _ => throw new ArgumentOutOfRangeException(nameof(builtin)),
        };
    }

    public static bool IsBuiltinIndex(ushort callIndex)
    {
        var specialStart = BuiltinCallBase - 4;
        var mainEnd = BuiltinCallBase + BuiltinCallCount - 1;
        return callIndex >= specialStart && callIndex <= mainEnd;
    }

    public static bool TryGetBuiltin(ushort callIndex, out PdVmBuiltin builtin)
    {
        switch (callIndex)
        {
            case var index when index == BuiltinCallBase - 4:
                builtin = PdVmBuiltin.FormatTemplate;
                return true;
            case var index when index == BuiltinCallBase - 3:
                builtin = PdVmBuiltin.ToString;
                return true;
            case var index when index == BuiltinCallBase - 2:
                builtin = PdVmBuiltin.TypeOf;
                return true;
            case var index when index == BuiltinCallBase - 1:
                builtin = PdVmBuiltin.Assert;
                return true;
        }

        if (callIndex >= BuiltinCallBase && callIndex < BuiltinCallBase + BuiltinCallCount)
        {
            builtin = (PdVmBuiltin)(callIndex - BuiltinCallBase);
            return true;
        }

        builtin = default;
        return false;
    }

    public static PdVmCallOutcome Dispatch(ushort callIndex, IReadOnlyList<PdVmValue> args)
    {
        if (!TryGetBuiltin(callIndex, out var builtin))
        {
            throw new NotSupportedException(
                IsBuiltinIndex(callIndex)
                    ? $"builtin call index 0x{callIndex:X4} is not supported by PdVm.Runtime yet"
                    : $"call index 0x{callIndex:X4} is not a builtin");
        }

        return builtin switch
        {
            PdVmBuiltin.Len => ReturnOne(LenValue(GetArg(args, 0))),
            PdVmBuiltin.Slice => ReturnOne(SliceValue(GetArg(args, 0), GetArg(args, 1), GetArg(args, 2))),
            PdVmBuiltin.Concat => ReturnOne(ConcatValue(GetArg(args, 0), GetArg(args, 1))),
            PdVmBuiltin.ArrayNew => ReturnOne(ArrayNewValue()),
            PdVmBuiltin.ArrayPush => ReturnOne(ArrayPushValue(GetArg(args, 0), GetArg(args, 1))),
            PdVmBuiltin.MapNew => ReturnOne(MapNewValue()),
            PdVmBuiltin.Get => ReturnOne(GetValue(GetArg(args, 0), GetArg(args, 1))),
            PdVmBuiltin.Has => ReturnOne(HasValue(GetArg(args, 0), GetArg(args, 1))),
            PdVmBuiltin.Set => ReturnOne(SetValue(GetArg(args, 0), GetArg(args, 1), GetArg(args, 2))),
            PdVmBuiltin.Keys => ReturnOne(KeysValue(GetArg(args, 0))),
            PdVmBuiltin.BytesFromUtf8 => ReturnOne(BytesFromUtf8Value(GetArg(args, 0))),
            PdVmBuiltin.BytesToUtf8 => ReturnOne(BytesToUtf8Value(GetArg(args, 0))),
            PdVmBuiltin.BytesToUtf8Lossy => ReturnOne(BytesToUtf8LossyValue(GetArg(args, 0))),
            PdVmBuiltin.BytesFromHex => ReturnOne(BytesFromHexValue(GetArg(args, 0))),
            PdVmBuiltin.BytesToHex => ReturnOne(BytesToHexValue(GetArg(args, 0))),
            PdVmBuiltin.BytesFromBase64 => ReturnOne(BytesFromBase64Value(GetArg(args, 0))),
            PdVmBuiltin.BytesToBase64 => ReturnOne(BytesToBase64Value(GetArg(args, 0))),
            PdVmBuiltin.BytesFromArrayU8 => ReturnOne(BytesFromArrayU8Value(GetArg(args, 0))),
            PdVmBuiltin.BytesToArrayU8 => ReturnOne(BytesToArrayU8Value(GetArg(args, 0))),
            PdVmBuiltin.IoOpen => ReturnOne(DispatchIoOpen(args)),
            PdVmBuiltin.IoPopen => ReturnOne(DispatchIoPopen(args)),
            PdVmBuiltin.IoReadAll => ReturnOne(DispatchIoReadAll(args)),
            PdVmBuiltin.IoReadLine => ReturnOne(DispatchIoReadLine(args)),
            PdVmBuiltin.IoWrite => ReturnOne(DispatchIoWrite(args)),
            PdVmBuiltin.IoFlush => ReturnOne(DispatchIoFlush(args)),
            PdVmBuiltin.IoClose => ReturnOne(DispatchIoClose(args)),
            PdVmBuiltin.IoExists => ReturnOne(DispatchIoExists(args)),
            PdVmBuiltin.ReMatch => ReturnOne(DispatchRegexMatch(args)),
            PdVmBuiltin.ReFind => ReturnOne(DispatchRegexFind(args)),
            PdVmBuiltin.ReReplace => ReturnOne(DispatchRegexReplace(args)),
            PdVmBuiltin.ReSplit => ReturnOne(DispatchRegexSplit(args)),
            PdVmBuiltin.ReCaptures => ReturnOne(DispatchRegexCaptures(args)),
            PdVmBuiltin.JsonEncode => ReturnOne(DispatchJsonEncode(args)),
            PdVmBuiltin.JsonDecode => ReturnOne(DispatchJsonDecode(args)),
            PdVmBuiltin.Count => ReturnOne(CountValue(GetArg(args, 0))),
            PdVmBuiltin.FormatTemplate => ReturnOne(FormatTemplateValue(GetArg(args, 0), GetArg(args, 1))),
            PdVmBuiltin.ToString => ReturnOne(ToStringValue(GetArg(args, 0))),
            PdVmBuiltin.TypeOf => ReturnOne(TypeOfValue(GetArg(args, 0))),
            PdVmBuiltin.Assert => DispatchAssert(args),
            PdVmBuiltin.JitSetConfig
                or PdVmBuiltin.JitGetConfig
                or PdVmBuiltin.JitSetEnabled
                or PdVmBuiltin.JitGetEnabled
                or PdVmBuiltin.JitSetHotLoopThreshold
                or PdVmBuiltin.JitGetHotLoopThreshold
                or PdVmBuiltin.JitSetMaxTraceLen
                or PdVmBuiltin.JitGetMaxTraceLen => throw new NotSupportedException(
                    $"builtin {builtin} is not implemented"),
            _ => ReturnOne(PdVmBuiltinMath.Dispatch(builtin, args)),
        };
    }

    private static PdVmCallOutcome ReturnOne(PdVmValue value) =>
        PdVmCallOutcome.Returned(PdVmCallReturn.One(value));

    private static PdVmValue DispatchLen(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Len, args, 1);
        var value = GetArg(args, 0);
        return value.Kind switch
        {
            PdVmValueKind.String => PdVmValue.FromInt(PdVmValue.CountStringRunes(value.AsString())),
            PdVmValueKind.Bytes => PdVmValue.FromInt(value.AsBytes().Length),
            PdVmValueKind.Array => PdVmValue.FromInt(value.AsArray().Count),
            PdVmValueKind.Map => PdVmValue.FromInt(value.AsMap().Count),
            _ => throw new InvalidOperationException("len expects string/bytes/array/map"),
        };
    }

    private static PdVmValue DispatchSlice(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Slice, args, 3);
        var source = GetArg(args, 0);
        var start = GetArg(args, 1).AsInt();
        var length = GetArg(args, 2).AsInt();
        if (start < 0 || length <= 0)
        {
            return source.Kind switch
            {
                PdVmValueKind.String => PdVmValue.FromString(string.Empty),
                PdVmValueKind.Bytes => PdVmValue.FromBytes(Array.Empty<byte>()),
                PdVmValueKind.Array => PdVmValue.FromArray(Array.Empty<PdVmValue>()),
                _ => throw new InvalidOperationException("slice expects string/bytes/array"),
            };
        }

        var startIndex = checked((int)start);
        var sliceLength = checked((int)length);
        return source.Kind switch
        {
            PdVmValueKind.String => PdVmValue.FromString(PdVmValue.SliceStringByRunes(source.AsString(), startIndex, sliceLength)),
            PdVmValueKind.Bytes => PdVmValue.FromBytes(source.AsBytes().Skip(startIndex).Take(sliceLength)),
            PdVmValueKind.Array => PdVmValue.FromArray(source.AsArray().Skip(startIndex).Take(sliceLength)),
            _ => throw new InvalidOperationException("slice expects string/bytes/array"),
        };
    }

    private static PdVmValue DispatchConcat(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Concat, args, 2);
        var lhs = GetArg(args, 0);
        var rhs = GetArg(args, 1);

        if (lhs.Kind == PdVmValueKind.Array && rhs.Kind == PdVmValueKind.Array)
        {
            return PdVmValue.FromArray(lhs.AsArray().Concat(rhs.AsArray()));
        }

        return PdVmOps.Add(lhs, rhs);
    }

    private static PdVmValue DispatchArrayPush(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ArrayPush, args, 2);
        var values = new List<PdVmValue>(GetArg(args, 0).AsArray()) { GetArg(args, 1) };
        return PdVmValue.FromArray(values);
    }

    private static PdVmValue DispatchGet(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Get, args, 2);
        var container = GetArg(args, 0);
        var key = GetArg(args, 1);

        return container.Kind switch
        {
            PdVmValueKind.Array => GetArrayValue(container.AsArray(), key.AsInt()),
            PdVmValueKind.Bytes => PdVmValue.FromInt(GetByteValue(container.AsBytes(), key.AsInt())),
            PdVmValueKind.Map => GetMapValue(container.AsMap(), key),
            PdVmValueKind.String => PdVmValue.FromString(PdVmValue.GetStringRuneAt(container.AsString(), checked((int)key.AsInt()))),
            _ => throw new InvalidOperationException("get expects bytes/array/map/string"),
        };
    }

    private static PdVmValue DispatchHas(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Has, args, 2);
        var container = GetArg(args, 0);
        var key = GetArg(args, 1);

        return container.Kind switch
        {
            PdVmValueKind.Array => PdVmValue.FromBool(HasArrayIndex(container.AsArray(), key.AsInt())),
            PdVmValueKind.Bytes => PdVmValue.FromBool(HasArrayIndex(container.AsBytes(), key.AsInt())),
            PdVmValueKind.Map => PdVmValue.FromBool(container.AsMap().TryGetValue(key, out _)),
            _ => throw new InvalidOperationException("has expects bytes/array/map"),
        };
    }

    private static PdVmValue DispatchSet(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Set, args, 3);
        var container = GetArg(args, 0);
        var key = GetArg(args, 1);
        var value = GetArg(args, 2);

        return container.Kind switch
        {
            PdVmValueKind.Array => SetArrayValue(container.AsArray(), key.AsInt(), value),
            PdVmValueKind.Map => SetMapValue(container.AsMap(), key, value),
            _ => throw new InvalidOperationException("set expects array/map"),
        };
    }

    private static PdVmValue DispatchKeys(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Keys, args, 1);
        var container = GetArg(args, 0);
        return container.Kind switch
        {
            PdVmValueKind.Array => PdVmValue.FromArray(Enumerable.Range(0, container.AsArray().Count).Select(index => PdVmValue.FromInt(index))),
            PdVmValueKind.Map => PdVmValue.FromArray(container.AsMap().Select(pair => pair.Key)),
            _ => throw new InvalidOperationException("keys expects array/map"),
        };
    }

    private static PdVmValue DispatchCount(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Count, args, 1);
        var container = GetArg(args, 0);
        return container.Kind switch
        {
            PdVmValueKind.Array => PdVmValue.FromInt(container.AsArray().Count),
            PdVmValueKind.Map => PdVmValue.FromInt(container.AsMap().Count),
            _ => throw new InvalidOperationException("count expects array/map"),
        };
    }

    private static PdVmValue DispatchIoOpen(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoOpen, args, 2);
        return PdVmBuiltinIo.Open(GetArg(args, 0).AsString(), GetArg(args, 1).AsString());
    }

    private static PdVmValue DispatchIoPopen(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoPopen, args, 2);
        return PdVmBuiltinIo.Popen(GetArg(args, 0).AsString(), GetArg(args, 1).AsString());
    }

    private static PdVmValue DispatchIoReadAll(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoReadAll, args, 1);
        return PdVmBuiltinIo.ReadAll(GetArg(args, 0).AsInt());
    }

    private static PdVmValue DispatchIoReadLine(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoReadLine, args, 1);
        return PdVmBuiltinIo.ReadLine(GetArg(args, 0).AsInt());
    }

    private static PdVmValue DispatchIoWrite(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoWrite, args, 2);
        return PdVmBuiltinIo.Write(GetArg(args, 0).AsInt(), GetArg(args, 1).AsString());
    }

    private static PdVmValue DispatchIoFlush(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoFlush, args, 1);
        return PdVmBuiltinIo.Flush(GetArg(args, 0).AsInt());
    }

    private static PdVmValue DispatchIoClose(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoClose, args, 1);
        return PdVmBuiltinIo.Close(GetArg(args, 0).AsInt());
    }

    private static PdVmValue DispatchIoExists(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.IoExists, args, 1);
        return PdVmBuiltinIo.Exists(GetArg(args, 0).AsString());
    }

    private static PdVmValue DispatchJsonEncode(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.JsonEncode, args, 1);
        return PdVmValue.FromString(PdVmBuiltinJson.Encode(GetArg(args, 0)));
    }

    private static PdVmValue DispatchJsonDecode(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.JsonDecode, args, 1);
        return PdVmBuiltinJson.Decode(GetArg(args, 0).AsString());
    }

    private static PdVmValue DispatchRegexMatch(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ReMatch, args, 2);
        var regex = CompileRegex(PdVmBuiltin.ReMatch, GetArg(args, 0).AsString());
        return PdVmValue.FromBool(regex.IsMatch(GetArg(args, 1).AsString()));
    }

    private static PdVmValue DispatchRegexFind(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ReFind, args, 2);
        var regex = CompileRegex(PdVmBuiltin.ReFind, GetArg(args, 0).AsString());
        var match = regex.Match(GetArg(args, 1).AsString());
        return match.Success ? PdVmValue.FromString(match.Value) : PdVmValue.Null();
    }

    private static PdVmValue DispatchRegexReplace(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ReReplace, args, 3);
        var regex = CompileRegex(PdVmBuiltin.ReReplace, GetArg(args, 0).AsString());
        return PdVmValue.FromString(
            regex.Replace(
                GetArg(args, 1).AsString(),
                GetArg(args, 2).AsString()));
    }

    private static PdVmValue DispatchRegexSplit(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ReSplit, args, 2);
        var regex = CompileRegex(PdVmBuiltin.ReSplit, GetArg(args, 0).AsString());
        return PdVmValue.FromArray(
            regex.Split(GetArg(args, 1).AsString()).Select(PdVmValue.FromString));
    }

    private static PdVmValue DispatchRegexCaptures(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.ReCaptures, args, 2);
        var regex = CompileRegex(PdVmBuiltin.ReCaptures, GetArg(args, 0).AsString());
        var match = regex.Match(GetArg(args, 1).AsString());
        if (!match.Success)
        {
            return PdVmValue.FromArray(Array.Empty<PdVmValue>());
        }

        var values = new PdVmValue[match.Groups.Count];
        for (var index = 0; index < match.Groups.Count; index++)
        {
            var group = match.Groups[index];
            values[index] = group.Success ? PdVmValue.FromString(group.Value) : PdVmValue.Null();
        }

        return PdVmValue.FromArray(values);
    }

    private static PdVmCallOutcome DispatchAssert(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.Assert, args, 1);
        AssertValue(GetArg(args, 0));

        return PdVmCallOutcome.Returned(PdVmCallReturn.None);
    }

    private static Regex CompileRegex(PdVmBuiltin builtin, string pattern)
    {
        try
        {
            return new Regex(pattern, RegexOptions.CultureInvariant);
        }
        catch (ArgumentException ex)
        {
            var name = builtin switch
            {
                PdVmBuiltin.ReMatch => "re_match",
                PdVmBuiltin.ReFind => "re_find",
                PdVmBuiltin.ReReplace => "re_replace",
                PdVmBuiltin.ReSplit => "re_split",
                PdVmBuiltin.ReCaptures => "re_captures",
                _ => "regex",
            };
            throw new InvalidOperationException($"{name} invalid pattern: {ex.Message}", ex);
        }
    }

    private static PdVmValue GetArrayValue<T>(IReadOnlyList<T> values, long index)
        where T : notnull
    {
        if (index < 0)
        {
            throw new InvalidOperationException("array index must be non-negative");
        }

        var resolvedIndex = checked((int)index);
        if (resolvedIndex < 0 || resolvedIndex >= values.Count)
        {
            throw new InvalidOperationException($"array index {resolvedIndex} out of bounds");
        }

        return values[resolvedIndex] switch
        {
            PdVmValue value => value,
            byte value => PdVmValue.FromInt(value),
            _ => throw new InvalidOperationException("unsupported array payload"),
        };
    }

    private static long GetByteValue(byte[] values, long index)
    {
        if (index < 0)
        {
            throw new InvalidOperationException("bytes index must be non-negative");
        }

        var resolvedIndex = checked((int)index);
        if (resolvedIndex < 0 || resolvedIndex >= values.Length)
        {
            throw new InvalidOperationException($"bytes index {resolvedIndex} out of bounds");
        }

        return values[resolvedIndex];
    }

    private static PdVmValue GetMapValue(PdVmMap map, PdVmValue key)
    {
        if (!map.TryGetValue(key, out var value))
        {
            throw new InvalidOperationException("map key not found");
        }

        return value;
    }

    private static bool HasArrayIndex<T>(IReadOnlyCollection<T> values, long index)
    {
        if (index < 0)
        {
            return false;
        }

        return index < values.Count;
    }

    private static PdVmValue SetArrayValue(List<PdVmValue> values, long index, PdVmValue value)
    {
        if (index < 0)
        {
            throw new InvalidOperationException("array index must be non-negative");
        }

        var resolvedIndex = checked((int)index);
        var output = new List<PdVmValue>(values);
        if (resolvedIndex < output.Count)
        {
            output[resolvedIndex] = value;
        }
        else if (resolvedIndex == output.Count)
        {
            output.Add(value);
        }
        else
        {
            throw new InvalidOperationException($"array index {resolvedIndex} out of bounds");
        }

        return PdVmValue.FromArray(output);
    }

    private static PdVmValue SetMapValue(PdVmMap map, PdVmValue key, PdVmValue value)
    {
        var output = map.CloneMap();
        if (value.Kind == PdVmValueKind.Null)
        {
            output.Remove(key);
        }
        else
        {
            output.Set(key, value);
        }

        return PdVmValue.FromMap(output);
    }

    private static byte[] ToByteArray(IReadOnlyList<PdVmValue> values)
    {
        var output = new byte[values.Count];
        for (var index = 0; index < values.Count; index++)
        {
            var value = values[index];
            if (value.Kind != PdVmValueKind.Int || value.IntValue < byte.MinValue || value.IntValue > byte.MaxValue)
            {
                throw new InvalidOperationException($"bytes::from_array_u8 entry {index} must be an int in 0..=255");
            }

            output[index] = (byte)value.IntValue;
        }

        return output;
    }

    private static string DecodeUtf8Strict(byte[] payload)
    {
        try
        {
            return new UTF8Encoding(false, true).GetString(payload);
        }
        catch (DecoderFallbackException ex)
        {
            throw new InvalidOperationException($"bytes::to_utf8 requires valid utf-8: {ex.Message}", ex);
        }
    }

    private static byte[] ParseHex(string text)
    {
        if (text.Length % 2 != 0)
        {
            throw new InvalidOperationException("bytes::from_hex requires an even number of hex digits");
        }

        var output = new byte[text.Length / 2];
        for (var index = 0; index < text.Length; index += 2)
        {
            output[index / 2] = byte.Parse(text.AsSpan(index, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture);
        }

        return output;
    }

    private static string DispatchFormatTemplate(IReadOnlyList<PdVmValue> args)
    {
        EnsureArgCount(PdVmBuiltin.FormatTemplate, args, 2);
        var template = GetArg(args, 0).AsString();
        var values = GetArg(args, 1).AsArray();
        var builder = new StringBuilder(template.Length + values.Count * 8);
        var implicitIndex = 0;

        for (var index = 0; index < template.Length; index++)
        {
            var current = template[index];
            if (current == '{')
            {
                if (index + 1 < template.Length && template[index + 1] == '{')
                {
                    builder.Append('{');
                    index++;
                    continue;
                }

                var close = template.IndexOf('}', index + 1);
                if (close < 0)
                {
                    throw new InvalidOperationException($"format string is missing a closing '}}': {template}");
                }

                var slot = template[(index + 1)..close];
                int resolvedIndex;
                if (slot.Length == 0)
                {
                    resolvedIndex = implicitIndex++;
                }
                else if (!int.TryParse(slot, NumberStyles.None, CultureInfo.InvariantCulture, out resolvedIndex))
                {
                    throw new InvalidOperationException($"format placeholder '{{{slot}}}' is invalid");
                }

                if (resolvedIndex < 0 || resolvedIndex >= values.Count)
                {
                    throw new InvalidOperationException($"format argument index {resolvedIndex} is out of bounds");
                }

                builder.Append(PdVmValue.FormatDisplay(values[resolvedIndex]));
                index = close;
                continue;
            }

            if (current == '}' && index + 1 < template.Length && template[index + 1] == '}')
            {
                builder.Append('}');
                index++;
                continue;
            }

            builder.Append(current);
        }

        return builder.ToString();
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
}
