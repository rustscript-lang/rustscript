using System.Collections;
using System.Globalization;
using System.Runtime.CompilerServices;
using System.Text;

namespace PdVm.Runtime;

public enum PdVmValueKind
{
    Null = 0,
    Int = 1,
    Float = 2,
    Bool = 3,
    String = 4,
    Bytes = 5,
    Array = 6,
    Map = 7,
}

public sealed class PdVmMap : IEnumerable<KeyValuePair<PdVmValue, PdVmValue>>, IEquatable<PdVmMap>
{
    private readonly Dictionary<PdVmValue, PdVmValue> _entries;

    public PdVmMap()
        : this(Array.Empty<KeyValuePair<PdVmValue, PdVmValue>>())
    {
    }

    public PdVmMap(IEnumerable<KeyValuePair<PdVmValue, PdVmValue>> entries)
    {
        _entries = new Dictionary<PdVmValue, PdVmValue>(PdVmValueKeyComparer.Instance);
        foreach (var (key, value) in entries)
        {
            _entries[key] = value;
        }
    }

    public int Count => _entries.Count;

    public bool TryGetValue(PdVmValue key, out PdVmValue value) => _entries.TryGetValue(key, out value!);

    public void Set(PdVmValue key, PdVmValue value) => _entries[key] = value;

    public bool Remove(PdVmValue key) => _entries.Remove(key);

    public PdVmMap CloneMap() => new(_entries);

    public IEnumerator<KeyValuePair<PdVmValue, PdVmValue>> GetEnumerator() => _entries.GetEnumerator();

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    public bool Equals(PdVmMap? other)
    {
        if (other is null || Count != other.Count)
        {
            return false;
        }

        foreach (var (key, value) in _entries)
        {
            if (!other._entries.TryGetValue(key, out var peer) || !value.Equals(peer))
            {
                return false;
            }
        }

        return true;
    }

    public override bool Equals(object? obj) => obj is PdVmMap other && Equals(other);

    public override int GetHashCode()
    {
        var entryHashes = new List<int>(_entries.Count);
        foreach (var (key, value) in _entries)
        {
            entryHashes.Add(HashCode.Combine(PdVmValueKeyComparer.Instance.GetHashCode(key), value));
        }

        entryHashes.Sort();
        var hash = new HashCode();
        hash.Add(_entries.Count);
        foreach (var entryHash in entryHashes)
        {
            hash.Add(entryHash);
        }

        return hash.ToHashCode();
    }
}

public sealed class PdVmValueKeyComparer : IEqualityComparer<PdVmValue>
{
    public static readonly PdVmValueKeyComparer Instance = new();

    private PdVmValueKeyComparer()
    {
    }

    public bool Equals(PdVmValue? x, PdVmValue? y)
    {
        if (ReferenceEquals(x, y))
        {
            return true;
        }

        if (x is null || y is null || x.Kind != y.Kind)
        {
            return false;
        }

        return x.Kind switch
        {
            PdVmValueKind.Null => true,
            PdVmValueKind.Int => x.IntValue == y.IntValue,
            PdVmValueKind.Float => CanonicalFloatKeyBits(x.FloatValue) == CanonicalFloatKeyBits(y.FloatValue),
            PdVmValueKind.Bool => x.BoolValue == y.BoolValue,
            PdVmValueKind.String => StringComparer.Ordinal.Equals(x.StringValue, y.StringValue),
            PdVmValueKind.Bytes => x.BytesValue!.AsSpan().SequenceEqual(y.BytesValue),
            PdVmValueKind.Array => ReferenceEquals(x.ArrayValue, y.ArrayValue),
            PdVmValueKind.Map => ReferenceEquals(x.MapValue, y.MapValue),
            _ => false,
        };
    }

    public int GetHashCode(PdVmValue obj)
    {
        var hash = new HashCode();
        hash.Add(obj.Kind);

        switch (obj.Kind)
        {
            case PdVmValueKind.Int:
                hash.Add(obj.IntValue);
                break;
            case PdVmValueKind.Float:
                hash.Add(CanonicalFloatKeyBits(obj.FloatValue));
                break;
            case PdVmValueKind.Bool:
                hash.Add(obj.BoolValue);
                break;
            case PdVmValueKind.String:
                hash.Add(obj.StringValue, StringComparer.Ordinal);
                break;
            case PdVmValueKind.Bytes:
                foreach (var value in obj.BytesValue!)
                {
                    hash.Add(value);
                }
                break;
            case PdVmValueKind.Array:
                hash.Add(RuntimeHelpers.GetHashCode(obj.ArrayValue!));
                break;
            case PdVmValueKind.Map:
                hash.Add(RuntimeHelpers.GetHashCode(obj.MapValue!));
                break;
        }

        return hash.ToHashCode();
    }

    private static long CanonicalFloatKeyBits(double value)
    {
        if (value == 0d)
        {
            return BitConverter.DoubleToInt64Bits(0d);
        }

        return BitConverter.DoubleToInt64Bits(value);
    }
}

public sealed class PdVmValue : IEquatable<PdVmValue>
{
    private static readonly PdVmValue NullValue = new(PdVmValueKind.Null);

    private PdVmValue(PdVmValueKind kind)
    {
        Kind = kind;
    }

    private PdVmValue(long value)
    {
        Kind = PdVmValueKind.Int;
        IntValue = value;
    }

    private PdVmValue(double value)
    {
        Kind = PdVmValueKind.Float;
        FloatValue = value;
    }

    private PdVmValue(bool value)
    {
        Kind = PdVmValueKind.Bool;
        BoolValue = value;
    }

    private PdVmValue(string value)
    {
        Kind = PdVmValueKind.String;
        StringValue = value;
    }

    private PdVmValue(byte[] value)
    {
        Kind = PdVmValueKind.Bytes;
        BytesValue = value;
    }

    private PdVmValue(List<PdVmValue> value)
    {
        Kind = PdVmValueKind.Array;
        ArrayValue = value;
    }

    private PdVmValue(PdVmMap value)
    {
        Kind = PdVmValueKind.Map;
        MapValue = value;
    }

    public PdVmValueKind Kind { get; }

    public long IntValue { get; }

    public double FloatValue { get; }

    public bool BoolValue { get; }

    public string? StringValue { get; }

    public byte[]? BytesValue { get; }

    public List<PdVmValue>? ArrayValue { get; }

    public PdVmMap? MapValue { get; }

    public PdVmValueType Type => Kind switch
    {
        PdVmValueKind.Null => PdVmValueType.Null,
        PdVmValueKind.Int => PdVmValueType.Int,
        PdVmValueKind.Float => PdVmValueType.Float,
        PdVmValueKind.Bool => PdVmValueType.Bool,
        PdVmValueKind.String => PdVmValueType.String,
        PdVmValueKind.Bytes => PdVmValueType.Bytes,
        PdVmValueKind.Array => PdVmValueType.Array,
        PdVmValueKind.Map => PdVmValueType.Map,
        _ => PdVmValueType.Unknown,
    };

    public static PdVmValue Null() => NullValue;

    public static PdVmValue FromInt(long value) => new(value);

    public static PdVmValue FromFloat(double value) => new(value);

    public static PdVmValue FromBool(bool value) => new(value);

    public static PdVmValue FromString(string value) => new(value ?? throw new ArgumentNullException(nameof(value)));

    public static PdVmValue FromBytes(IEnumerable<byte> value) =>
        new(value is byte[] bytes ? bytes.ToArray() : value.ToArray());

    public static PdVmValue FromArray(IEnumerable<PdVmValue> value) => new(value.ToList());

    public static PdVmValue FromMap(IEnumerable<KeyValuePair<PdVmValue, PdVmValue>> value) => new(new PdVmMap(value));

    public long AsInt()
    {
        if (Kind != PdVmValueKind.Int)
        {
            throw new InvalidOperationException($"expected int, got {Kind}");
        }

        return IntValue;
    }

    public bool AsBool()
    {
        if (Kind != PdVmValueKind.Bool)
        {
            throw new InvalidOperationException($"expected bool, got {Kind}");
        }

        return BoolValue;
    }

    public string AsString()
    {
        if (Kind != PdVmValueKind.String || StringValue is null)
        {
            throw new InvalidOperationException($"expected string, got {Kind}");
        }

        return StringValue;
    }

    public byte[] AsBytes()
    {
        if (Kind != PdVmValueKind.Bytes || BytesValue is null)
        {
            throw new InvalidOperationException($"expected bytes, got {Kind}");
        }

        return BytesValue;
    }

    public List<PdVmValue> AsArray()
    {
        if (Kind != PdVmValueKind.Array || ArrayValue is null)
        {
            throw new InvalidOperationException($"expected array, got {Kind}");
        }

        return ArrayValue;
    }

    public PdVmMap AsMap()
    {
        if (Kind != PdVmValueKind.Map || MapValue is null)
        {
            throw new InvalidOperationException($"expected map, got {Kind}");
        }

        return MapValue;
    }

    public bool Equals(PdVmValue? other)
    {
        if (other is null || Kind != other.Kind)
        {
            return false;
        }

        return Kind switch
        {
            PdVmValueKind.Null => true,
            PdVmValueKind.Int => IntValue == other.IntValue,
            PdVmValueKind.Float => FloatValue == other.FloatValue,
            PdVmValueKind.Bool => BoolValue == other.BoolValue,
            PdVmValueKind.String => StringComparer.Ordinal.Equals(StringValue, other.StringValue),
            PdVmValueKind.Bytes => BytesValue!.AsSpan().SequenceEqual(other.BytesValue),
            PdVmValueKind.Array => ArrayValue!.SequenceEqual(other.ArrayValue!),
            PdVmValueKind.Map => MapValue!.Equals(other.MapValue),
            _ => false,
        };
    }

    public override bool Equals(object? obj) => obj is PdVmValue other && Equals(other);

    public override int GetHashCode()
    {
        var hash = new HashCode();
        hash.Add(Kind);

        switch (Kind)
        {
            case PdVmValueKind.Int:
                hash.Add(IntValue);
                break;
            case PdVmValueKind.Float:
                hash.Add(FloatValue == 0d ? 0d : FloatValue);
                break;
            case PdVmValueKind.Bool:
                hash.Add(BoolValue);
                break;
            case PdVmValueKind.String:
                hash.Add(StringValue, StringComparer.Ordinal);
                break;
            case PdVmValueKind.Bytes:
                foreach (var value in BytesValue!)
                {
                    hash.Add(value);
                }
                break;
            case PdVmValueKind.Array:
                foreach (var item in ArrayValue!)
                {
                    hash.Add(item);
                }
                break;
            case PdVmValueKind.Map:
                hash.Add(MapValue);
                break;
        }

        return hash.ToHashCode();
    }

    public override string ToString() => FormatDisplay(this);

    internal static string FormatDisplay(PdVmValue value)
    {
        return value.Kind switch
        {
            PdVmValueKind.Null => "null",
            PdVmValueKind.Int => value.IntValue.ToString(CultureInfo.InvariantCulture),
            PdVmValueKind.Float => value.FloatValue.ToString("R", CultureInfo.InvariantCulture),
            PdVmValueKind.Bool => value.BoolValue ? "true" : "false",
            PdVmValueKind.String => value.AsString(),
            PdVmValueKind.Bytes => FormatBytes(value.AsBytes()),
            PdVmValueKind.Array => $"[{string.Join(", ", value.AsArray().Select(FormatDisplay))}]",
            PdVmValueKind.Map => $"{{{string.Join(", ", value.AsMap().Select(pair => $"{FormatDisplay(pair.Key)}: {FormatDisplay(pair.Value)}"))}}}",
            _ => value.Kind.ToString(),
        };
    }

    internal static int CountStringRunes(string value)
    {
        var count = 0;
        foreach (var _ in value.EnumerateRunes())
        {
            count++;
        }

        return count;
    }

    internal static string SliceStringByRunes(string value, int start, int length)
    {
        if (length <= 0)
        {
            return string.Empty;
        }

        var builder = new StringBuilder();
        var index = 0;
        foreach (var rune in value.EnumerateRunes())
        {
            if (index >= start && index < start + length)
            {
                builder.Append(rune.ToString());
            }

            index++;
            if (index >= start + length)
            {
                break;
            }
        }

        return builder.ToString();
    }

    internal static string GetStringRuneAt(string value, int index)
    {
        if (index < 0)
        {
            throw new InvalidOperationException("string index must be non-negative");
        }

        var current = 0;
        foreach (var rune in value.EnumerateRunes())
        {
            if (current == index)
            {
                return rune.ToString();
            }

            current++;
        }

        throw new InvalidOperationException($"string index {index} out of bounds");
    }

    private static string FormatBytes(byte[] bytes)
    {
        const int PreviewLength = 16;
        var previewLength = Math.Min(bytes.Length, PreviewLength);
        var preview = Convert.ToHexString(bytes.AsSpan(0, previewLength)).ToLowerInvariant();
        return bytes.Length > previewLength
            ? $"bytes[len={bytes.Length} hex={preview}..]"
            : $"bytes[len={bytes.Length} hex={preview}]";
    }
}
