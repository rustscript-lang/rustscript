namespace PdVm.Runtime;

public sealed class PdVmDelegateHost : IAsyncPdVmHost
{
    private readonly Dictionary<string, Func<IReadOnlyList<PdVmValue>, PdVmCallOutcome>> _syncHandlers =
        new(StringComparer.Ordinal);
    private readonly Dictionary<string, Func<IReadOnlyList<PdVmValue>, CancellationToken, ValueTask<PdVmCallReturn>>> _asyncHandlers =
        new(StringComparer.Ordinal);
    private readonly Dictionary<ulong, Task<PdVmCallReturn>> _pendingOperations = new();
    private long _nextOpId;

    public void Register(string name, Func<IReadOnlyList<PdVmValue>, PdVmCallOutcome> handler)
    {
        _syncHandlers[name] = handler;
    }

    public void RegisterReturn(string name, Func<IReadOnlyList<PdVmValue>, PdVmCallReturn> handler)
    {
        Register(name, args => PdVmCallOutcome.Returned(handler(args)));
    }

    public void RegisterValue(string name, Func<IReadOnlyList<PdVmValue>, PdVmValue> handler)
    {
        RegisterReturn(name, args => PdVmCallReturn.One(handler(args)));
    }

    public void RegisterAsync(
        string name,
        Func<IReadOnlyList<PdVmValue>, CancellationToken, ValueTask<PdVmCallReturn>> handler)
    {
        _asyncHandlers[name] = handler;
    }

    public void RegisterAsyncValue(
        string name,
        Func<IReadOnlyList<PdVmValue>, CancellationToken, ValueTask<PdVmValue>> handler)
    {
        RegisterAsync(
            name,
            async (args, cancellationToken) => PdVmCallReturn.One(await handler(args, cancellationToken)));
    }

    public PdVmCallOutcome Call(string name, IReadOnlyList<PdVmValue> args)
    {
        if (_syncHandlers.TryGetValue(name, out var syncHandler))
        {
            return syncHandler(args);
        }

        if (_asyncHandlers.TryGetValue(name, out var asyncHandler))
        {
            var opId = (ulong)Interlocked.Increment(ref _nextOpId);
            _pendingOperations[opId] = asyncHandler(args, CancellationToken.None).AsTask();
            return PdVmCallOutcome.Pending(opId);
        }

        throw new InvalidOperationException($"unbound host import '{name}'");
    }

    public async ValueTask<PdVmCallReturn> WaitAsync(ulong opId, CancellationToken cancellationToken = default)
    {
        if (!_pendingOperations.Remove(opId, out var operation))
        {
            throw new InvalidOperationException($"unknown pending host operation {opId}");
        }

        return await operation.WaitAsync(cancellationToken);
    }
}
