using System.Net;

namespace PdEdge.Http;

public enum PdEdgeHttpCliActionKind
{
    Run = 0,
    Help = 1,
    Version = 2,
}

public enum PdEdgeVmExecutionMode
{
    Async = 0,
    Threading = 1,
}

public readonly record struct PdEdgeHttpCliAction(PdEdgeHttpCliActionKind Kind, PdEdgeHttpOptions? Options);

public sealed record PdEdgeHttpOptions
{
    public IPEndPoint ListenEndPoint { get; init; } = new(IPAddress.Any, 8080);

    public string? ProgramSourcePath { get; init; }

    public string? ProgramVmbcPath { get; init; }

    public PdEdgeVmExecutionMode ExecutionMode { get; init; } = PdEdgeVmExecutionMode.Async;

    public ulong? VmFuel { get; init; }

    public uint VmFuelCheckInterval { get; init; } = 1;

    public bool VmJit { get; init; }

    public bool DisableLogging { get; init; }

    public int MaxSteps { get; init; } = 10_000_000;
}

public static class PdEdgeHttpCli
{
    private const string BinaryName = "pd-edge-http-minimal-clr";

    public static PdEdgeHttpCliAction Parse(IReadOnlyList<string> args)
    {
        ArgumentNullException.ThrowIfNull(args);

        var options = new PdEdgeHttpOptions();
        for (var index = 0; index < args.Count; index++)
        {
            var arg = args[index];
            switch (arg)
            {
                case "-h":
                case "--help":
                    return new PdEdgeHttpCliAction(PdEdgeHttpCliActionKind.Help, null);
                case "-V":
                case "--version":
                    return new PdEdgeHttpCliAction(PdEdgeHttpCliActionKind.Version, null);
                case "--disable-logging":
                    options = options with { DisableLogging = true };
                    break;
                case "--data-addr":
                case "--proxy-addr":
                    options = options with { ListenEndPoint = ParseEndPoint(arg, NextValue(arg, args, ref index)) };
                    break;
                case "--program-source":
                    options = options with { ProgramSourcePath = NextValue(arg, args, ref index) };
                    break;
                case "--program-vmbc":
                    options = options with { ProgramVmbcPath = NextValue(arg, args, ref index) };
                    break;
                case "--vm-execution-mode":
                    options = options with { ExecutionMode = ParseExecutionMode(NextValue(arg, args, ref index)) };
                    break;
                case "--vm-fuel":
                {
                    var value = NextValue(arg, args, ref index);
                    if (!ulong.TryParse(value, out var fuel) || fuel == 0)
                    {
                        throw new ArgumentException($"invalid --vm-fuel: {value}");
                    }

                    options = options with { VmFuel = fuel };
                    break;
                }
                case "--vm-fuel-check-interval":
                {
                    var value = NextValue(arg, args, ref index);
                    if (!uint.TryParse(value, out var interval) || interval == 0)
                    {
                        throw new ArgumentException($"invalid --vm-fuel-check-interval: {value}");
                    }

                    options = options with { VmFuelCheckInterval = interval };
                    break;
                }
                case "--vm-jit":
                    options = options with { VmJit = true };
                    break;
                case "--max-steps":
                {
                    var value = NextValue(arg, args, ref index);
                    if (!int.TryParse(value, out var maxSteps) || maxSteps <= 0)
                    {
                        throw new ArgumentException($"invalid --max-steps: {value}");
                    }

                    options = options with { MaxSteps = maxSteps };
                    break;
                }
                default:
                    throw new ArgumentException($"unknown argument: {arg}");
            }
        }

        if (!string.IsNullOrWhiteSpace(options.ProgramSourcePath) &&
            !string.IsNullOrWhiteSpace(options.ProgramVmbcPath))
        {
            throw new ArgumentException("--program-source and --program-vmbc are mutually exclusive");
        }

        if (options.VmFuelCheckInterval != 1 && options.VmFuel is null)
        {
            throw new ArgumentException("--vm-fuel-check-interval requires --vm-fuel");
        }

        return new PdEdgeHttpCliAction(PdEdgeHttpCliActionKind.Run, options);
    }

    public static string HelpText =>
        $"""
        Usage: {BinaryName} [options]

        Options:
          --data-addr <ADDR>            HTTP listen address (default: 0.0.0.0:8080)
          --proxy-addr <ADDR>           Alias for --data-addr
          --program-source <PATH>       Load and compile a source program at startup
          --program-vmbc <PATH>         Load an encoded VMBC program at startup
          --vm-execution-mode <MODE>    VM execution mode: async|threading (default: async)
          --vm-fuel <UNITS>             Compatibility flag accepted for parity with pd-edge-http-minimal
          --vm-fuel-check-interval <N>  Compatibility flag accepted for parity with pd-edge-http-minimal
          --vm-jit                      Compatibility flag accepted for parity with pd-edge-http-minimal
          --max-steps <N>               Per-request VM instruction cap (default: 10000000)
          --disable-logging             Disable log output
          -V, --version                 Show version
          -h, --help                    Show help

        Notes:
          - This runtime is HTTP/1-only and intentionally omits admin APIs, metrics, debugger,
            control plane, DAG resolution, TLS, HTTP/2, and HTTP/3.
          - Programs are loaded only at startup through CLI arguments.
          - VM fuel and JIT flags are parsed for compatibility but are currently no-ops in CLR.
        """;

    public static string VersionText
    {
        get
        {
            var version = typeof(PdEdgeHttpCli).Assembly.GetName().Version?.ToString() ?? "0.0.0";
            return $"{BinaryName} {version}";
        }
    }

    private static string NextValue(string flag, IReadOnlyList<string> args, ref int index)
    {
        if (index + 1 >= args.Count)
        {
            throw new ArgumentException($"missing value for {flag}");
        }

        var value = args[++index].Trim();
        if (string.IsNullOrEmpty(value))
        {
            throw new ArgumentException($"value for {flag} cannot be empty");
        }

        return value;
    }

    private static PdEdgeVmExecutionMode ParseExecutionMode(string value)
    {
        return value.Trim().ToLowerInvariant() switch
        {
            "async" => PdEdgeVmExecutionMode.Async,
            "threading" or "spawn-blocking" => PdEdgeVmExecutionMode.Threading,
            _ => throw new ArgumentException(
                $"invalid --vm-execution-mode: {value} (expected async|threading)"),
        };
    }

    private static IPEndPoint ParseEndPoint(string flag, string value)
    {
        if (string.IsNullOrWhiteSpace(value))
        {
            throw new ArgumentException($"invalid {flag}: {value}");
        }

        string host;
        string portText;
        if (value[0] == '[')
        {
            var close = value.IndexOf(']');
            if (close <= 1 || close + 2 > value.Length || value[close + 1] != ':')
            {
                throw new ArgumentException($"invalid {flag}: {value}");
            }

            host = value[1..close];
            portText = value[(close + 2)..];
        }
        else
        {
            var separator = value.LastIndexOf(':');
            if (separator <= 0 || separator == value.Length - 1)
            {
                throw new ArgumentException($"invalid {flag}: {value}");
            }

            host = value[..separator];
            portText = value[(separator + 1)..];
        }

        if (!IPAddress.TryParse(host, out var address) ||
            !int.TryParse(portText, out var port) ||
            port is < 0 or > 65535)
        {
            throw new ArgumentException($"invalid {flag}: {value}");
        }

        return new IPEndPoint(address, port);
    }
}
