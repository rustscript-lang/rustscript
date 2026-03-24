using System.Text;
using Microsoft.AspNetCore.Http;
using PdVm.Runtime;

namespace PdEdge.Http;

public sealed class PdEdgeRequestContext
{
    private readonly object _gate = new();
    private readonly HttpContext _httpContext;
    private RequestBodyState _bodyState = new();
    private ResponseState _responseState = new();
    private PreparedUpstreamState? _preparedUpstream;

    public PdEdgeRequestContext(HttpContext httpContext)
    {
        _httpContext = httpContext ?? throw new ArgumentNullException(nameof(httpContext));
    }

    public HttpContext HttpContext => _httpContext;

    public string RequestId => _httpContext.TraceIdentifier;

    public string HttpVersion => _httpContext.Request.Protocol switch
    {
        "HTTP/0.9" => "0.9",
        "HTTP/1.0" => "1.0",
        "HTTP/1.1" => "1.1",
        "HTTP/2" => "2",
        "HTTP/3" => "3",
        _ => "unknown",
    };

    public string Method => _httpContext.Request.Method;

    public string Path => string.IsNullOrEmpty(_httpContext.Request.Path.Value)
        ? "/"
        : _httpContext.Request.Path.Value!;

    public string PathWithQuery =>
        Path + (_httpContext.Request.QueryString.HasValue ? _httpContext.Request.QueryString.Value : string.Empty);

    public string Query => _httpContext.Request.QueryString.HasValue ? _httpContext.Request.QueryString.Value![1..] : string.Empty;

    public string Scheme => _httpContext.Request.Scheme;

    public string Host => _httpContext.Request.Host.Host ?? string.Empty;

    public long Port => _httpContext.Request.Host.Port ??
        (string.Equals(Scheme, "https", StringComparison.OrdinalIgnoreCase) ? 443 : 80);

    public string ClientIp => _httpContext.Connection.RemoteIpAddress?.ToString() ?? string.Empty;

    public string RequestHeader(string name) =>
        _httpContext.Request.Headers.TryGetValue(name, out var value) ? value.ToString() : string.Empty;

    public string QueryArgument(string name) =>
        _httpContext.Request.Query.TryGetValue(name, out var value) ? value.ToString() : string.Empty;

    public PdVmValue RequestHeadersValue() =>
        PdVmValue.FromMap(_httpContext.Request.Headers.Select(header =>
            new KeyValuePair<PdVmValue, PdVmValue>(
                PdVmValue.FromString(header.Key),
                PdVmValue.FromString(header.Value.ToString()))));

    public PdVmValue QueryArgumentsValue() =>
        PdVmValue.FromMap(_httpContext.Request.Query.Select(item =>
            new KeyValuePair<PdVmValue, PdVmValue>(
                PdVmValue.FromString(item.Key),
                PdVmValue.FromString(item.Value.ToString()))));

    public void SetResponseStatus(int statusCode)
    {
        lock (_gate)
        {
            _responseState = _responseState with { StatusCode = statusCode };
        }
    }

    public void SetResponseHeader(string name, string value)
    {
        lock (_gate)
        {
            var headers = new Dictionary<string, string>(_responseState.Headers, StringComparer.OrdinalIgnoreCase)
            {
                [name] = value,
            };
            _responseState = _responseState with { Headers = headers };
        }
    }

    public void SetResponseHeaders(IReadOnlyDictionary<string, string> headers)
    {
        lock (_gate)
        {
            var merged = new Dictionary<string, string>(_responseState.Headers, StringComparer.OrdinalIgnoreCase);
            foreach (var pair in headers)
            {
                merged[pair.Key] = pair.Value;
            }

            _responseState = _responseState with { Headers = merged };
        }
    }

    public void SetResponseBody(string body)
    {
        lock (_gate)
        {
            _responseState = _responseState with { Body = Encoding.UTF8.GetBytes(body) };
        }
    }

    public void MarkNativeForward()
    {
        lock (_gate)
        {
            _responseState = _responseState with { NativeForward = true };
        }
    }

    public void PrepareDefaultUpstream(string host, int port, IReadOnlyDictionary<string, string> headers)
    {
        lock (_gate)
        {
            _preparedUpstream = new PreparedUpstreamState(
                host,
                port,
                new Dictionary<string, string>(headers, StringComparer.OrdinalIgnoreCase));
        }
    }

    public PreparedUpstreamState? GetPreparedUpstream()
    {
        lock (_gate)
        {
            return _preparedUpstream is null
                ? null
                : new PreparedUpstreamState(
                    _preparedUpstream.Host,
                    _preparedUpstream.Port,
                    new Dictionary<string, string>(_preparedUpstream.Headers, StringComparer.OrdinalIgnoreCase));
        }
    }

    public ResponseState GetResponseSnapshot()
    {
        lock (_gate)
        {
            return new ResponseState
            {
                StatusCode = _responseState.StatusCode,
                Headers = new Dictionary<string, string>(_responseState.Headers, StringComparer.OrdinalIgnoreCase),
                Body = _responseState.Body is null ? null : _responseState.Body.ToArray(),
                NativeForward = _responseState.NativeForward,
            };
        }
    }

    public async ValueTask<byte[]> ReadRequestBodyAsync(CancellationToken cancellationToken = default)
    {
        Stream? stream = null;
        byte[]? buffered = null;
        lock (_gate)
        {
            switch (_bodyState.Kind)
            {
                case RequestBodyKind.Buffered:
                    buffered = _bodyState.BufferedBody;
                    break;
                case RequestBodyKind.Reading:
                    throw new InvalidOperationException("request body is already being read");
                case RequestBodyKind.Taken:
                    throw new InvalidOperationException("request body has already been forwarded");
                case RequestBodyKind.Streaming:
                    stream = _bodyState.Stream ?? _httpContext.Request.Body;
                    _bodyState = new RequestBodyState { Kind = RequestBodyKind.Reading };
                    break;
            }
        }

        if (buffered is not null)
        {
            return buffered;
        }

        using var memory = new MemoryStream();
        await stream!.CopyToAsync(memory, cancellationToken);
        var bytes = memory.ToArray();
        lock (_gate)
        {
            _bodyState = new RequestBodyState
            {
                Kind = RequestBodyKind.Buffered,
                BufferedBody = bytes,
            };
        }

        return bytes;
    }

    public HttpContent TakeRequestBodyForForwarding()
    {
        lock (_gate)
        {
            return _bodyState.Kind switch
            {
                RequestBodyKind.Streaming => TakeStreamingContent(),
                RequestBodyKind.Buffered => TakeBufferedContent(),
                RequestBodyKind.Reading => throw new InvalidOperationException("request body is currently being read"),
                RequestBodyKind.Taken => throw new InvalidOperationException("request body has already been consumed"),
                _ => throw new InvalidOperationException($"unexpected request body state {_bodyState.Kind}"),
            };
        }
    }

    private HttpContent TakeStreamingContent()
    {
        var stream = _bodyState.Stream ?? _httpContext.Request.Body;
        _bodyState = new RequestBodyState { Kind = RequestBodyKind.Taken };
        return new StreamContent(stream);
    }

    private HttpContent TakeBufferedContent()
    {
        var content = new ByteArrayContent(_bodyState.BufferedBody ?? Array.Empty<byte>());
        _bodyState = new RequestBodyState { Kind = RequestBodyKind.Taken };
        return content;
    }

    public sealed record ResponseState
    {
        public int? StatusCode { get; init; }

        public IReadOnlyDictionary<string, string> Headers { get; init; } =
            new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);

        public byte[]? Body { get; init; }

        public bool NativeForward { get; init; }
    }

    public sealed record PreparedUpstreamState(
        string Host,
        int Port,
        IReadOnlyDictionary<string, string> Headers);

    private sealed class RequestBodyState
    {
        public RequestBodyKind Kind { get; set; } = RequestBodyKind.Streaming;

        public Stream? Stream { get; set; }

        public byte[]? BufferedBody { get; set; }
    }

    private enum RequestBodyKind
    {
        Streaming = 0,
        Buffered = 1,
        Reading = 2,
        Taken = 3,
    }
}
