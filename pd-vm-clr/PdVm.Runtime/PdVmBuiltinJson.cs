using System.Text;
using System.Text.Json;

namespace PdVm.Runtime;

internal static class PdVmBuiltinJson
{
    public static string Encode(PdVmValue value)
    {
        using var stream = new MemoryStream();
        using var writer = new Utf8JsonWriter(stream);
        WriteValue(writer, value);
        writer.Flush();
        return Encoding.UTF8.GetString(stream.ToArray());
    }

    public static PdVmValue Decode(string text)
    {
        try
        {
            var reader = new Utf8JsonReader(
                Encoding.UTF8.GetBytes(text),
                new JsonReaderOptions
                {
                    AllowTrailingCommas = false,
                    CommentHandling = JsonCommentHandling.Disallow,
                });

            if (!reader.Read())
            {
                throw new JsonException("empty JSON input");
            }

            var value = ReadValue(ref reader);
            if (reader.Read())
            {
                throw new JsonException("trailing characters after JSON value");
            }

            return value;
        }
        catch (JsonException ex)
        {
            throw new InvalidOperationException($"json_decode failed: {ex.Message}", ex);
        }
    }

    private static void WriteValue(Utf8JsonWriter writer, PdVmValue value)
    {
        switch (value.Kind)
        {
            case PdVmValueKind.Null:
                writer.WriteNullValue();
                return;
            case PdVmValueKind.Int:
                writer.WriteNumberValue(value.IntValue);
                return;
            case PdVmValueKind.Float:
                if (double.IsNaN(value.FloatValue) || double.IsInfinity(value.FloatValue))
                {
                    throw new InvalidOperationException("json_encode does not support NaN or infinity");
                }

                writer.WriteNumberValue(value.FloatValue);
                return;
            case PdVmValueKind.Bool:
                writer.WriteBooleanValue(value.BoolValue);
                return;
            case PdVmValueKind.String:
                writer.WriteStringValue(value.AsString());
                return;
            case PdVmValueKind.Bytes:
                throw new InvalidOperationException("json_encode does not support bytes values");
            case PdVmValueKind.Array:
                writer.WriteStartArray();
                foreach (var item in value.AsArray())
                {
                    WriteValue(writer, item);
                }

                writer.WriteEndArray();
                return;
            case PdVmValueKind.Map:
                writer.WriteStartObject();
                foreach (var (key, item) in value.AsMap())
                {
                    if (key.Kind != PdVmValueKind.String)
                    {
                        throw new InvalidOperationException("json_encode map keys must be strings");
                    }

                    writer.WritePropertyName(key.AsString());
                    WriteValue(writer, item);
                }

                writer.WriteEndObject();
                return;
            default:
                throw new InvalidOperationException($"unsupported JSON value kind {value.Kind}");
        }
    }

    private static PdVmValue ReadValue(ref Utf8JsonReader reader)
    {
        return reader.TokenType switch
        {
            JsonTokenType.Null => PdVmValue.Null(),
            JsonTokenType.True => PdVmValue.FromBool(true),
            JsonTokenType.False => PdVmValue.FromBool(false),
            JsonTokenType.String => PdVmValue.FromString(reader.GetString() ?? string.Empty),
            JsonTokenType.Number => ReadNumber(ref reader),
            JsonTokenType.StartArray => ReadArray(ref reader),
            JsonTokenType.StartObject => ReadObject(ref reader),
            _ => throw new JsonException($"unsupported JSON token {reader.TokenType}"),
        };
    }

    private static PdVmValue ReadNumber(ref Utf8JsonReader reader)
    {
        if (reader.TryGetInt64(out var intValue))
        {
            return PdVmValue.FromInt(intValue);
        }

        var value = reader.GetDouble();
        if (!double.IsFinite(value))
        {
            throw new JsonException("json_decode number is out of supported range");
        }

        return PdVmValue.FromFloat(value);
    }

    private static PdVmValue ReadArray(ref Utf8JsonReader reader)
    {
        var values = new List<PdVmValue>();
        while (reader.Read())
        {
            if (reader.TokenType == JsonTokenType.EndArray)
            {
                return PdVmValue.FromArray(values);
            }

            values.Add(ReadValue(ref reader));
        }

        throw new JsonException("unexpected end of array");
    }

    private static PdVmValue ReadObject(ref Utf8JsonReader reader)
    {
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var entries = new List<KeyValuePair<PdVmValue, PdVmValue>>();
        while (reader.Read())
        {
            if (reader.TokenType == JsonTokenType.EndObject)
            {
                return PdVmValue.FromMap(entries);
            }

            if (reader.TokenType != JsonTokenType.PropertyName)
            {
                throw new JsonException($"expected property name, got {reader.TokenType}");
            }

            var key = reader.GetString() ?? string.Empty;
            if (!seen.Add(key))
            {
                throw new JsonException($"json_decode duplicate object key '{key}'");
            }

            if (!reader.Read())
            {
                throw new JsonException("unexpected end of object");
            }

            entries.Add(
                new KeyValuePair<PdVmValue, PdVmValue>(
                    PdVmValue.FromString(key),
                    ReadValue(ref reader)));
        }

        throw new JsonException("unexpected end of object");
    }
}
