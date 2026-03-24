namespace PdVm.Runtime;

public static class PdVmDefaultHost
{
    public static PdVmDelegateHost CreateConsoleHost(TextWriter? output = null)
    {
        output ??= Console.Out;
        var host = new PdVmDelegateHost();

        host.RegisterValue(
            "print",
            args =>
            {
                var value = RequireValue(args, 0);
                output.Write(PdVmValue.FormatDisplay(value));
                return value;
            });

        host.RegisterValue(
            "println",
            args =>
            {
                var value = RequireValue(args, 0);
                output.Write(PdVmValue.FormatDisplay(value));
                output.Write('\n');
                return value;
            });

        host.RegisterAsyncValue(
            "runtime::sleep",
            async (args, cancellationToken) =>
            {
                var duration = RequireValue(args, 0).AsInt();
                if (duration < 0)
                {
                    throw new InvalidOperationException(
                        $"runtime::sleep expects non-negative milliseconds, got {duration}");
                }

                await Task.Delay(TimeSpan.FromMilliseconds(duration), cancellationToken);
                return PdVmValue.FromBool(true);
            });

        host.Register("runtime::exit", _ => PdVmCallOutcome.Halted());

        return host;
    }

    private static PdVmValue RequireValue(IReadOnlyList<PdVmValue> args, int index)
    {
        if (index < 0 || index >= args.Count)
        {
            throw new InvalidOperationException($"missing host argument {index}");
        }

        return args[index];
    }
}
