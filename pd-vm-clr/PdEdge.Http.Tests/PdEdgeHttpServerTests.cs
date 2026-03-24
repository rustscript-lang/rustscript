using System.Net;
using System.Text;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Hosting.Server;
using Microsoft.AspNetCore.Hosting.Server.Features;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using PdEdge.Http;

namespace PdEdge.Http.Tests;

public sealed class PdEdgeHttpServerTests
{
    [Fact]
    public async Task NoProgramReturns404()
    {
        await using var server = await StartServerAsync(program: null);
        using var client = new HttpClient { BaseAddress = server.BaseAddress };

        using var response = await client.PostAsync("/perf", new StringContent(string.Empty));

        Assert.Equal(HttpStatusCode.NotFound, response.StatusCode);
        Assert.Equal("not found", await response.Content.ReadAsStringAsync());
    }

    [Fact]
    public async Task LocalHostCallsProgramCanTerminateRequest()
    {
        var source = """
            use http;

            http::response::set_status(200);
            http::response::set_header("x-method", http::request::get_method());
            http::response::set_header("x-path", http::request::get_path());
            http::response::set_header("x-body", http::request::get_body());
            http::response::set_body("ok");
            """;
        var program = await LoadInlineProgramAsync(source);
        await using var server = await StartServerAsync(program);
        using var client = new HttpClient { BaseAddress = server.BaseAddress };

        using var response = await client.PostAsync(
            "/hello",
            new StringContent("payload", Encoding.UTF8, "text/plain"));

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Equal("POST", response.Headers.GetValues("x-method").Single());
        Assert.Equal("/hello", response.Headers.GetValues("x-path").Single());
        Assert.Equal("payload", response.Headers.GetValues("x-body").Single());
        Assert.Equal("ok", await response.Content.ReadAsStringAsync());
    }

    [Fact]
    public async Task ProxyProgramRoundTripsUpstreamAndOverlaysHeaders()
    {
        await using var upstream = await StartUpstreamAsync();
        var source = $$"""
            use http;
            use proxy;

            let downstream_version = http::request::get_http_version();
            let upstream = http::exchange::prepare_default_upstream(
                "127.0.0.1",
                {{upstream.Port}},
                "1.1",
                ["x-downstream-version", downstream_version, "x-bench-program-header", "program-proxy"]
            );
            let downstream = proxy::stream::downstream();
            let upstream_stream = proxy::stream::exchange(upstream);
            proxy::forward_native(downstream, upstream_stream);
            http::response::set_headers([
                "x-downstream-version", downstream_version,
                "x-bench-response-header", "program-proxy"
            ]);
            """;
        var program = await LoadInlineProgramAsync(source);
        await using var server = await StartServerAsync(program);
        using var client = new HttpClient { BaseAddress = server.BaseAddress };

        using var response = await client.PostAsync(
            "/perf",
            new StringContent(string.Empty, Encoding.UTF8, "text/plain"));
        response.EnsureSuccessStatusCode();

        Assert.Equal("1.1", response.Headers.GetValues("x-downstream-version").Single());
        Assert.Equal("program-proxy", response.Headers.GetValues("x-bench-response-header").Single());
        Assert.Equal("1.1", response.Headers.GetValues("x-bench-upstream-version").Single());
        Assert.Equal("upstream-ok", await response.Content.ReadAsStringAsync());
    }

    private static async Task<PdEdgeLoadedProgram> LoadInlineProgramAsync(string source)
    {
        var tempRoot = Path.Combine(Path.GetTempPath(), "pd-edge-http-tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(tempRoot);
        var sourcePath = Path.Combine(tempRoot, "program.rss");
        var vmbcPath = Path.Combine(tempRoot, "program.vmbc");
        await File.WriteAllTextAsync(sourcePath, source, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false));
        await PdEdgeProgramLoader.CompileSourceFileToVmbcAsync(sourcePath, vmbcPath);
        var bytes = await File.ReadAllBytesAsync(vmbcPath);
        return PdEdgeProgramLoader.LoadFromVmbcBytes(bytes, "PdEdge.Http.Tests");
    }

    private static async Task<PdEdgeHttpServer> StartServerAsync(PdEdgeLoadedProgram? program)
    {
        var server = new PdEdgeHttpServer(
            new PdEdgeHttpOptions
            {
                ListenEndPoint = new IPEndPoint(IPAddress.Loopback, 0),
                DisableLogging = true,
            },
            program,
            loggerFactory: NullLoggerFactory.Instance);
        await server.StartAsync();
        return server;
    }

    private static async Task<UpstreamFixture> StartUpstreamAsync()
    {
        var builder = WebApplication.CreateSlimBuilder(new WebApplicationOptions { Args = Array.Empty<string>() });
        builder.WebHost.ConfigureKestrel(kestrel =>
        {
            kestrel.AddServerHeader = false;
            kestrel.Listen(IPAddress.Loopback, 0);
        });
        var app = builder.Build();
        app.Map("/{**path}", async context =>
        {
            using var reader = new StreamReader(context.Request.Body, Encoding.UTF8);
            var body = await reader.ReadToEndAsync();
            context.Response.StatusCode = StatusCodes.Status200OK;
            context.Response.Headers["x-bench-upstream-version"] = "1.1";
            context.Response.Headers["x-bench-upstream-body-mode"] =
                string.IsNullOrEmpty(body) ? "headers-only" : "body-read";
            context.Response.ContentType = "text/plain";
            await context.Response.WriteAsync(
                string.IsNullOrEmpty(body) ? "upstream-ok" : $"upstream-echo:{body}");
        });

        await app.StartAsync();
        var address = ResolveAddress(app);
        return new UpstreamFixture(app, address.Port);
    }

    private static Uri ResolveAddress(WebApplication app)
    {
        var addresses = app.Services
            .GetRequiredService<IServer>()
            .Features
            .Get<IServerAddressesFeature>()?
            .Addresses;
        return new Uri(addresses!.Single());
    }

    private sealed class UpstreamFixture(WebApplication app, int port) : IAsyncDisposable
    {
        public int Port { get; } = port;

        public async ValueTask DisposeAsync()
        {
            await app.StopAsync();
            await app.DisposeAsync();
        }
    }
}
