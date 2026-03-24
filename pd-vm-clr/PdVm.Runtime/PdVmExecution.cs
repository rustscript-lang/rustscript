using System.Reflection;
using System.Runtime.Loader;

namespace PdVm.Runtime;

public readonly record struct PdVmExecutionResult(PdVmStatus Status, int Steps);

public static class PdVmExecution
{
    public static PdVmExecutionResult Run(IPdVmProgram program, IPdVmHost host, int maxSteps = 1_000_000)
    {
        for (var steps = 1; steps <= maxSteps; steps++)
        {
            var status = program.RunStep(host);
            switch (status.Kind)
            {
                case PdVmStatusKind.Halted:
                    return new PdVmExecutionResult(status, steps);
                case PdVmStatusKind.Yielded:
                    continue;
                case PdVmStatusKind.Waiting:
                    throw new InvalidOperationException(
                        "program entered a waiting state; use RunAsync with an async-capable host");
                default:
                    throw new InvalidOperationException($"unexpected status {status.Kind}");
            }
        }

        throw new InvalidOperationException($"execution exceeded {maxSteps} steps");
    }

    public static async ValueTask<PdVmExecutionResult> RunAsync(
        IPdVmProgram program,
        IAsyncPdVmHost host,
        int maxSteps = 1_000_000,
        CancellationToken cancellationToken = default)
    {
        for (var steps = 1; steps <= maxSteps; steps++)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var status = program.RunStep(host);
            switch (status.Kind)
            {
                case PdVmStatusKind.Halted:
                    return new PdVmExecutionResult(status, steps);
                case PdVmStatusKind.Yielded:
                    continue;
                case PdVmStatusKind.Waiting:
                    var values = await host.WaitAsync(status.WaitingOpId, cancellationToken);
                    program.ResumePending(status.WaitingOpId, values);
                    continue;
                default:
                    throw new InvalidOperationException($"unexpected status {status.Kind}");
            }
        }

        throw new InvalidOperationException($"execution exceeded {maxSteps} steps");
    }
}

public static class PdVmAssemblyLoader
{
    public static IPdVmProgram LoadProgram(string assemblyPath)
    {
        var fullPath = Path.GetFullPath(assemblyPath);
        var assembly = AssemblyLoadContext.Default.LoadFromAssemblyPath(fullPath);
        return CreateProgram(assembly);
    }

    public static IPdVmProgram CreateProgram(Assembly assembly)
    {
        var programType = assembly
            .GetTypes()
            .FirstOrDefault(type =>
                !type.IsAbstract &&
                typeof(IPdVmProgram).IsAssignableFrom(type) &&
                type.GetConstructor(Type.EmptyTypes) is not null);

        if (programType is null)
        {
            throw new InvalidOperationException("no concrete IPdVmProgram implementation found");
        }

        return (IPdVmProgram)Activator.CreateInstance(programType)!;
    }
}
