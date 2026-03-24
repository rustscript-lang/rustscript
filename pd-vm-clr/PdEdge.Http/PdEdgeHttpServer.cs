using System.Net;
using System.Net.Http.Headers;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Hosting.Server;
using Microsoft.AspNetCore.Hosting.Server.Features;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;
using PdVm.Runtime;

namespace PdEdge.Http;

public sealed class PdEdgeHttpServer : IAsyncDisposable
{
    private static readonly string[] HopByHopHeaders =
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];

    private readonly PdEdgeHttpOptions _options;
    private readonly PdEdgeLoadedProgram? _program;
    private readonly HttpClient _client;
    private readonly ILoggerFactory? _loggerFactory;
    private WebApplication? _app;
    private ILogger<PdEdgeHttpServer>? _logger;

    public PdEdgeHttpServer(
        PdEdgeHttpOptions options,
        PdEdgeLoadedProgram? program,
        HttpClient? client = null,
        ILoggerFactory? loggerFactory = null)
    {
        _options = options ?? throw new ArgumentNullException(nameof(options));
        _program = program;
        _client = client ?? CreateHttpClient();
        _loggerFactory = loggerFactory;
    }

    public Uri? BaseAddress { get; private set; }

    public async Task StartAsync(CancellationToken cancellationToken = default)
    {
        if (_app is not null)
        {
            throw new InvalidOperationException("server is already started");
        }

        var builder = WebApplication.CreateSlimBuilder(new WebApplicationOptions
        {
            Args = Array.Empty<string>(),
        });

        builder.WebHost.ConfigureKestrel(kestrel =>
        {
            kestrel.AddServerHeader = false;
            kestrel.Listen(_options.ListenEndPoint);
        });

        builder.Logging.ClearProviders();
        if (!_options.DisableLogging)
        {
            if (_loggerFactory is not null)
            {
                builder.Services.AddSingleton(_loggerFactory);
            }

            builder.Logging.AddSimpleConsole(console =>
            {
                console.TimestampFormat = "HH:mm:ss ";
                console.SingleLine = true;
            });
        }

        builder.Services.AddSingleton(_client);

        var app = builder.Build();
        _logger = app.Services.GetService<ILogger<PdEdgeHttpServer>>();
        app.Map("/{**path}", HandleRequestAsync);
        await app.StartAsync(cancellationToken);
        _app = app;
        BaseAddress = ResolveBaseAddress(app);
    }

    public async ValueTask DisposeAsync()
    {
        if (_app is not null)
        {
            await _app.StopAsync();
            await _app.DisposeAsync();
            _app = null;
        }

        _client.Dispose();
        _loggerFactory?.Dispose();
    }

    private async Task HandleRequestAsync(HttpContext httpContext)
    {
        if (_program is null)
        {
            await WriteTextResponseAsync(httpContext, StatusCodes.Status404NotFound, "not found");
            return;
        }

        var requestContext = new PdEdgeRequestContext(httpContext);
        try
        {
            await ExecuteProgramAsync(_program, requestContext, httpContext.RequestAborted);
            await ResolveResponseAsync(requestContext, httpContext.RequestAborted);
        }
        catch (Exception exception)
        {
            _logger?.LogWarning(exception, "vm execution error");
            await WriteTextResponseAsync(
                httpContext,
                StatusCodes.Status500InternalServerError,
                $"vm execution error: {exception.Message}");
        }
    }

    private async Task ExecuteProgramAsync(
        PdEdgeLoadedProgram program,
        PdEdgeRequestContext requestContext,
        CancellationToken cancellationToken)
    {
        switch (_options.ExecutionMode)
        {
            case PdEdgeVmExecutionMode.Async:
                await ExecuteProgramCoreAsync(program, requestContext, cancellationToken);
                break;
            case PdEdgeVmExecutionMode.Threading:
                await Task.Run(
                    () => ExecuteProgramCoreAsync(program, requestContext, cancellationToken).AsTask(),
                    cancellationToken);
                break;
            default:
                throw new InvalidOperationException($"unexpected execution mode {_options.ExecutionMode}");
        }
    }

    private async ValueTask ExecuteProgramCoreAsync(
        PdEdgeLoadedProgram program,
        PdEdgeRequestContext requestContext,
        CancellationToken cancellationToken)
    {
        var instance = program.CreateProgram();
        var host = CreateHost(requestContext);
        if (program.UsesAsyncHostOps)
        {
            await PdVmExecution.RunAsync(instance, host, _options.MaxSteps, cancellationToken);
            return;
        }

        PdVmExecution.Run(instance, host, _options.MaxSteps);
    }

    private PdVmDelegateHost CreateHost(PdEdgeRequestContext requestContext)
    {
        var host = new PdVmDelegateHost();
        host.RegisterValue(PdEdgeHostFunctions.RequestGetId, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetId, args, 0);
            return PdVmValue.FromString(requestContext.RequestId);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetHttpVersion, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetHttpVersion, args, 0);
            return PdVmValue.FromString(requestContext.HttpVersion);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetMethod, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetMethod, args, 0);
            return PdVmValue.FromString(requestContext.Method);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetPath, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetPath, args, 0);
            return PdVmValue.FromString(requestContext.Path);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetPathWithQuery, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetPathWithQuery, args, 0);
            return PdVmValue.FromString(requestContext.PathWithQuery);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetQuery, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetQuery, args, 0);
            return PdVmValue.FromString(requestContext.Query);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetScheme, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetScheme, args, 0);
            return PdVmValue.FromString(requestContext.Scheme);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetHost, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetHost, args, 0);
            return PdVmValue.FromString(requestContext.Host);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetPort, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetPort, args, 0);
            return PdVmValue.FromInt(requestContext.Port);
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetHeader, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetHeader, args, 1);
            return PdVmValue.FromString(requestContext.RequestHeader(ExpectString(args[0], "header name")));
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetHeaders, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetHeaders, args, 0);
            return requestContext.RequestHeadersValue();
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetQueryArg, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetQueryArg, args, 1);
            return PdVmValue.FromString(requestContext.QueryArgument(ExpectString(args[0], "query arg name")));
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetQueryArgs, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetQueryArgs, args, 0);
            return requestContext.QueryArgumentsValue();
        });
        host.RegisterValue(PdEdgeHostFunctions.RequestGetClientIp, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetClientIp, args, 0);
            return PdVmValue.FromString(requestContext.ClientIp);
        });
        host.RegisterAsyncValue(PdEdgeHostFunctions.RequestGetBody, async (args, cancellationToken) =>
        {
            ExpectArgCount(PdEdgeHostFunctions.RequestGetBody, args, 0);
            var body = await requestContext.ReadRequestBodyAsync(cancellationToken);
            return PdVmValue.FromString(System.Text.Encoding.UTF8.GetString(body));
        });
        host.RegisterReturn(PdEdgeHostFunctions.ResponseSetStatus, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ResponseSetStatus, args, 1);
            var statusCode = checked((int)ExpectInt(args[0], "status code"));
            if (statusCode is < 100 or > 599)
            {
                throw new InvalidOperationException($"status code must be in range 100..=599, got {statusCode}");
            }

            requestContext.SetResponseStatus(statusCode);
            return PdVmCallReturn.None;
        });
        host.RegisterReturn(PdEdgeHostFunctions.ResponseSetHeader, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ResponseSetHeader, args, 2);
            requestContext.SetResponseHeader(
                ExpectString(args[0], "header name"),
                ExpectString(args[1], "header value"));
            return PdVmCallReturn.None;
        });
        host.RegisterReturn(PdEdgeHostFunctions.ResponseSetHeaders, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ResponseSetHeaders, args, 1);
            requestContext.SetResponseHeaders(ParseHeaderBatch(args[0]));
            return PdVmCallReturn.None;
        });
        host.RegisterReturn(PdEdgeHostFunctions.ResponseSetBody, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ResponseSetBody, args, 1);
            requestContext.SetResponseBody(ExpectString(args[0], "response body"));
            return PdVmCallReturn.None;
        });
        host.RegisterValue(PdEdgeHostFunctions.ExchangeDefaultUpstream, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ExchangeDefaultUpstream, args, 0);
            return PdVmValue.FromInt(PdEdgeHostFunctions.DefaultUpstreamExchangeHandle);
        });
        host.RegisterValue(PdEdgeHostFunctions.ExchangePrepareDefaultUpstream, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ExchangePrepareDefaultUpstream, args, 4);
            var hostName = ExpectString(args[0], "upstream host");
            var port = checked((int)ExpectInt(args[1], "upstream port"));
            if (port is <= 0 or > 65535)
            {
                throw new InvalidOperationException("invalid upstream port");
            }

            var version = ExpectString(args[2], "upstream version preference");
            ValidateVersionPreference(version);
            requestContext.PrepareDefaultUpstream(hostName, port, ParseHeaderBatch(args[3]));
            return PdVmValue.FromInt(PdEdgeHostFunctions.DefaultUpstreamExchangeHandle);
        });
        host.RegisterValue(PdEdgeHostFunctions.ProxyStreamDownstream, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ProxyStreamDownstream, args, 0);
            return PdVmValue.FromInt(PdEdgeHostFunctions.DefaultDownstreamStreamHandle);
        });
        host.RegisterValue(PdEdgeHostFunctions.ProxyStreamExchange, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ProxyStreamExchange, args, 1);
            var exchange = ExpectInt(args[0], "exchange handle");
            if (exchange != PdEdgeHostFunctions.DefaultUpstreamExchangeHandle)
            {
                throw new InvalidOperationException(
                    $"PdEdge.Http supports only the default upstream exchange, got {exchange}");
            }

            return PdVmValue.FromInt(PdEdgeHostFunctions.DefaultUpstreamStreamHandle);
        });
        host.RegisterValue(PdEdgeHostFunctions.ProxyForwardNative, args =>
        {
            ExpectArgCount(PdEdgeHostFunctions.ProxyForwardNative, args, 2);
            var left = ExpectInt(args[0], "left proxy stream");
            var right = ExpectInt(args[1], "right proxy stream");
            var validPair = (left == PdEdgeHostFunctions.DefaultDownstreamStreamHandle &&
                             right == PdEdgeHostFunctions.DefaultUpstreamStreamHandle) ||
                            (left == PdEdgeHostFunctions.DefaultUpstreamStreamHandle &&
                             right == PdEdgeHostFunctions.DefaultDownstreamStreamHandle);
            if (!validPair)
            {
                throw new InvalidOperationException(
                    "PdEdge.Http supports only downstream<->default-upstream native forwarding");
            }

            requestContext.MarkNativeForward();
            return PdVmValue.FromString("native-http1");
        });

        return host;
    }

    private async Task ResolveResponseAsync(PdEdgeRequestContext requestContext, CancellationToken cancellationToken)
    {
        var response = requestContext.GetResponseSnapshot();
        if (response.NativeForward && response.Body is null)
        {
            try
            {
                await WriteForwardResponseAsync(requestContext, response, cancellationToken);
                return;
            }
            catch (Exception exception)
            {
                _logger?.LogWarning(exception, "upstream forward failed");
                await WriteTextResponseAsync(
                    requestContext.HttpContext,
                    StatusCodes.Status502BadGateway,
                    "bad gateway");
                return;
            }
        }

        if (response.StatusCode.HasValue || response.Headers.Count > 0 || response.Body is not null)
        {
            await WriteLocalResponseAsync(requestContext.HttpContext, response, cancellationToken);
            return;
        }

        await WriteTextResponseAsync(requestContext.HttpContext, StatusCodes.Status404NotFound, "not found");
    }

    private async Task WriteForwardResponseAsync(
        PdEdgeRequestContext requestContext,
        PdEdgeRequestContext.ResponseState overrides,
        CancellationToken cancellationToken)
    {
        using var upstreamResponse = await ForwardUpstreamAsync(requestContext, cancellationToken);
        var downstream = requestContext.HttpContext.Response;
        downstream.StatusCode = overrides.StatusCode ?? (int)upstreamResponse.StatusCode;

        var connectionTokens = new HashSet<string>(GetConnectionTokens(upstreamResponse.Headers), StringComparer.OrdinalIgnoreCase);
        AddResponseHeaders(downstream, upstreamResponse.Headers, connectionTokens);
        if (upstreamResponse.Content is not null)
        {
            AddResponseHeaders(downstream, upstreamResponse.Content.Headers, connectionTokens);
        }

        foreach (var pair in overrides.Headers)
        {
            downstream.Headers[pair.Key] = pair.Value;
        }

        if (upstreamResponse.Content is null)
        {
            return;
        }

        await using var stream = await upstreamResponse.Content.ReadAsStreamAsync(cancellationToken);
        await stream.CopyToAsync(downstream.Body, cancellationToken);
    }

    private async Task<HttpResponseMessage> ForwardUpstreamAsync(
        PdEdgeRequestContext requestContext,
        CancellationToken cancellationToken)
    {
        var upstream = requestContext.GetPreparedUpstream() ??
            throw new InvalidOperationException("default upstream exchange was not prepared");

        var request = new HttpRequestMessage(
            new HttpMethod(requestContext.HttpContext.Request.Method),
            BuildUpstreamUri(upstream, requestContext));
        request.Version = HttpVersion.Version11;
        request.VersionPolicy = HttpVersionPolicy.RequestVersionExact;
        request.Headers.Host = $"{upstream.Host}:{upstream.Port}";
        request.Content = requestContext.TakeRequestBodyForForwarding();

        var skipHeaders = new HashSet<string>(GetConnectionTokens(requestContext.HttpContext.Request.Headers), StringComparer.OrdinalIgnoreCase);
        foreach (var header in requestContext.HttpContext.Request.Headers)
        {
            if (string.Equals(header.Key, "Host", StringComparison.OrdinalIgnoreCase) ||
                skipHeaders.Contains(header.Key) ||
                IsHopByHopHeader(header.Key))
            {
                continue;
            }

            TryAddRequestHeader(
                request,
                header.Key,
                header.Value.Where(value => value is not null).Select(value => value!).ToArray());
        }

        foreach (var pair in upstream.Headers)
        {
            TryAddRequestHeader(request, pair.Key, [pair.Value]);
        }

        return await _client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellationToken);
    }

    private static void AddResponseHeaders(
        HttpResponse downstream,
        HttpHeaders headers,
        HashSet<string> connectionTokens)
    {
        foreach (var header in headers)
        {
            if (connectionTokens.Contains(header.Key) || IsHopByHopHeader(header.Key))
            {
                continue;
            }

            downstream.Headers[header.Key] = header.Value.ToArray();
        }
    }

    private static void TryAddRequestHeader(HttpRequestMessage request, string name, string[] values)
    {
        if (!request.Headers.TryAddWithoutValidation(name, values))
        {
            request.Content ??= new ByteArrayContent(Array.Empty<byte>());
            request.Content.Headers.TryAddWithoutValidation(name, values);
        }
    }

    private static Uri BuildUpstreamUri(
        PdEdgeRequestContext.PreparedUpstreamState upstream,
        PdEdgeRequestContext requestContext)
    {
        var builder = new UriBuilder(Uri.UriSchemeHttp, upstream.Host, upstream.Port)
        {
            Path = requestContext.Path,
            Query = requestContext.Query,
        };
        return builder.Uri;
    }

    private static async Task WriteLocalResponseAsync(
        HttpContext httpContext,
        PdEdgeRequestContext.ResponseState response,
        CancellationToken cancellationToken)
    {
        httpContext.Response.StatusCode = response.StatusCode ?? StatusCodes.Status200OK;
        foreach (var pair in response.Headers)
        {
            httpContext.Response.Headers[pair.Key] = pair.Value;
        }

        if (response.Body is null)
        {
            return;
        }

        httpContext.Response.ContentLength = response.Body.Length;
        await httpContext.Response.Body.WriteAsync(response.Body, cancellationToken);
    }

    private static async Task WriteTextResponseAsync(HttpContext httpContext, int statusCode, string body)
    {
        var bytes = System.Text.Encoding.UTF8.GetBytes(body);
        httpContext.Response.StatusCode = statusCode;
        httpContext.Response.ContentType = "text/plain; charset=utf-8";
        httpContext.Response.ContentLength = bytes.Length;
        await httpContext.Response.Body.WriteAsync(bytes);
    }

    private static IReadOnlyDictionary<string, string> ParseHeaderBatch(PdVmValue value)
    {
        var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        switch (value.Kind)
        {
            case PdVmValueKind.Array:
            {
                var values = value.AsArray();
                if (values.Count % 2 != 0)
                {
                    throw new InvalidOperationException(
                        "header batch array must contain alternating name/value pairs");
                }

                for (var index = 0; index < values.Count; index += 2)
                {
                    headers[ExpectString(values[index], "header name")] =
                        ExpectString(values[index + 1], "header value");
                }

                return headers;
            }
            case PdVmValueKind.Map:
                foreach (var pair in value.AsMap())
                {
                    headers[ExpectString(pair.Key, "header name")] =
                        ExpectString(pair.Value, "header value");
                }

                return headers;
            default:
                throw new InvalidOperationException("header batch must be an array or map");
        }
    }

    private static void ValidateVersionPreference(string version)
    {
        var normalized = version.Trim().ToLowerInvariant();
        if (normalized is "1.1" or "http/1.1" or "http1" or "http11" or "auto")
        {
            return;
        }

        throw new InvalidOperationException(
            $"PdEdge.Http supports only auto/1.1 upstream preference, got '{version}'");
    }

    private static void ExpectArgCount(string name, IReadOnlyList<PdVmValue> args, int expected)
    {
        if (args.Count != expected)
        {
            throw new InvalidOperationException($"{name} expected {expected} arguments, got {args.Count}");
        }
    }

    private static string ExpectString(PdVmValue value, string label)
    {
        if (value.Kind != PdVmValueKind.String)
        {
            throw new InvalidOperationException($"{label} must be a string");
        }

        return value.AsString();
    }

    private static long ExpectInt(PdVmValue value, string label)
    {
        if (value.Kind != PdVmValueKind.Int)
        {
            throw new InvalidOperationException($"{label} must be an int");
        }

        return value.AsInt();
    }

    private static bool IsHopByHopHeader(string name) =>
        HopByHopHeaders.Contains(name, StringComparer.OrdinalIgnoreCase);

    private static IEnumerable<string> GetConnectionTokens(IHeaderDictionary headers)
    {
        if (!headers.TryGetValue("Connection", out var connection))
        {
            return Array.Empty<string>();
        }

        return connection
            .SelectMany(value => (value ?? string.Empty).Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
            .Where(value => !string.IsNullOrWhiteSpace(value));
    }

    private static IEnumerable<string> GetConnectionTokens(HttpHeaders headers)
    {
        if (!headers.TryGetValues("Connection", out var values))
        {
            return Array.Empty<string>();
        }

        return values
            .SelectMany(value => value.Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
            .Where(value => !string.IsNullOrWhiteSpace(value));
    }

    private static HttpClient CreateHttpClient()
    {
        var handler = new SocketsHttpHandler
        {
            AllowAutoRedirect = false,
            AutomaticDecompression = DecompressionMethods.None,
            UseProxy = false,
            PooledConnectionIdleTimeout = TimeSpan.FromMinutes(2),
            PooledConnectionLifetime = TimeSpan.FromMinutes(10),
            MaxConnectionsPerServer = 1024,
        };

        return new HttpClient(handler)
        {
            Timeout = Timeout.InfiniteTimeSpan,
            DefaultRequestVersion = HttpVersion.Version11,
            DefaultVersionPolicy = HttpVersionPolicy.RequestVersionExact,
        };
    }

    private static Uri ResolveBaseAddress(WebApplication app)
    {
        var addresses = app.Services
            .GetRequiredService<IServer>()
            .Features
            .Get<IServerAddressesFeature>()?
            .Addresses;

        var address = addresses?.FirstOrDefault()
            ?? throw new InvalidOperationException("failed to resolve listening address");
        return new Uri(address);
    }
}

public static class PdEdgeHttpEntryPoint
{
    public static async Task<int> RunAsync(string[] args)
    {
        PdEdgeHttpCliAction action;
        try
        {
            action = PdEdgeHttpCli.Parse(args);
        }
        catch (ArgumentException exception)
        {
            Console.Error.WriteLine(exception.Message);
            return 2;
        }

        switch (action.Kind)
        {
            case PdEdgeHttpCliActionKind.Help:
                Console.Error.WriteLine(PdEdgeHttpCli.HelpText);
                return 0;
            case PdEdgeHttpCliActionKind.Version:
                Console.Out.WriteLine(PdEdgeHttpCli.VersionText);
                return 0;
            case PdEdgeHttpCliActionKind.Run:
                break;
            default:
                throw new InvalidOperationException($"unexpected CLI action {action.Kind}");
        }

        var options = action.Options ?? throw new InvalidOperationException("run action requires options");
        var program = await PdEdgeProgramLoader.LoadAsync(options);
        await using var server = new PdEdgeHttpServer(options, program);
        await server.StartAsync();
        await Task.Delay(Timeout.InfiniteTimeSpan);
        return 0;
    }
}
