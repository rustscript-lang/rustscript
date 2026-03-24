using PdVm.Runtime;

namespace PdEdge.Http;

public static class PdEdgeHostFunctions
{
    public const long DefaultDownstreamStreamHandle = 1;
    public const long DefaultUpstreamExchangeHandle = 1;
    public const long DefaultUpstreamStreamHandle = 2;

    public const string RequestGetId = "http::request::get_id";
    public const string RequestGetHttpVersion = "http::request::get_http_version";
    public const string RequestGetMethod = "http::request::get_method";
    public const string RequestGetPath = "http::request::get_path";
    public const string RequestGetPathWithQuery = "http::request::get_path_with_query";
    public const string RequestGetQuery = "http::request::get_query";
    public const string RequestGetScheme = "http::request::get_scheme";
    public const string RequestGetHost = "http::request::get_host";
    public const string RequestGetPort = "http::request::get_port";
    public const string RequestGetHeader = "http::request::get_header";
    public const string RequestGetHeaders = "http::request::get_headers";
    public const string RequestGetQueryArg = "http::request::get_query_arg";
    public const string RequestGetQueryArgs = "http::request::get_query_args";
    public const string RequestGetClientIp = "http::request::get_client_ip";
    public const string RequestGetBody = "http::request::get_body";
    public const string ResponseSetStatus = "http::response::set_status";
    public const string ResponseSetHeader = "http::response::set_header";
    public const string ResponseSetHeaders = "http::response::set_headers";
    public const string ResponseSetBody = "http::response::set_body";
    public const string ExchangeDefaultUpstream = "http::exchange::default_upstream";
    public const string ExchangePrepareDefaultUpstream = "http::exchange::prepare_default_upstream";
    public const string ProxyStreamDownstream = "proxy::stream::downstream";
    public const string ProxyStreamExchange = "proxy::stream::exchange";
    public const string ProxyForwardNative = "proxy::forward_native";

    private static readonly IReadOnlyDictionary<string, PdVmHostImport> KnownImports =
        new Dictionary<string, PdVmHostImport>(StringComparer.Ordinal)
        {
            [RequestGetId] = new(RequestGetId, 0, PdVmValueType.String),
            [RequestGetHttpVersion] = new(RequestGetHttpVersion, 0, PdVmValueType.String),
            [RequestGetMethod] = new(RequestGetMethod, 0, PdVmValueType.String),
            [RequestGetPath] = new(RequestGetPath, 0, PdVmValueType.String),
            [RequestGetPathWithQuery] = new(RequestGetPathWithQuery, 0, PdVmValueType.String),
            [RequestGetQuery] = new(RequestGetQuery, 0, PdVmValueType.String),
            [RequestGetScheme] = new(RequestGetScheme, 0, PdVmValueType.String),
            [RequestGetHost] = new(RequestGetHost, 0, PdVmValueType.String),
            [RequestGetPort] = new(RequestGetPort, 0, PdVmValueType.Int),
            [RequestGetHeader] = new(RequestGetHeader, 1, PdVmValueType.String),
            [RequestGetHeaders] = new(RequestGetHeaders, 0, PdVmValueType.Map),
            [RequestGetQueryArg] = new(RequestGetQueryArg, 1, PdVmValueType.String),
            [RequestGetQueryArgs] = new(RequestGetQueryArgs, 0, PdVmValueType.Map),
            [RequestGetClientIp] = new(RequestGetClientIp, 0, PdVmValueType.String),
            [RequestGetBody] = new(RequestGetBody, 0, PdVmValueType.String),
            [ResponseSetStatus] = new(ResponseSetStatus, 1, PdVmValueType.Null),
            [ResponseSetHeader] = new(ResponseSetHeader, 2, PdVmValueType.Null),
            [ResponseSetHeaders] = new(ResponseSetHeaders, 1, PdVmValueType.Null),
            [ResponseSetBody] = new(ResponseSetBody, 1, PdVmValueType.Null),
            [ExchangeDefaultUpstream] = new(ExchangeDefaultUpstream, 0, PdVmValueType.Int),
            [ExchangePrepareDefaultUpstream] = new(ExchangePrepareDefaultUpstream, 4, PdVmValueType.Int),
            [ProxyStreamDownstream] = new(ProxyStreamDownstream, 0, PdVmValueType.Int),
            [ProxyStreamExchange] = new(ProxyStreamExchange, 1, PdVmValueType.Int),
            [ProxyForwardNative] = new(ProxyForwardNative, 2, PdVmValueType.String),
        };

    public static bool UsesAsyncHostOps(IReadOnlyList<PdVmHostImport> imports) =>
        imports.Any(import => string.Equals(import.Name, RequestGetBody, StringComparison.Ordinal));

    public static void ValidateImports(IReadOnlyList<PdVmHostImport> imports)
    {
        foreach (var import in imports)
        {
            if (!KnownImports.TryGetValue(import.Name, out var known))
            {
                throw new InvalidOperationException(
                    $"PdEdge.Http does not support host import '{import.Name}'");
            }

            if (known.Arity != import.Arity)
            {
                throw new InvalidOperationException(
                    $"host import '{import.Name}' expects arity {known.Arity}, got {import.Arity}");
            }

            if (known.ReturnType != import.ReturnType)
            {
                throw new InvalidOperationException(
                    $"host import '{import.Name}' expects return type {known.ReturnType}, got {import.ReturnType}");
            }
        }
    }
}
