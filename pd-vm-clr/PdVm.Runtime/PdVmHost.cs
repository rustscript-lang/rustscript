namespace PdVm.Runtime;

public sealed class PdVmHostImport
{
    public PdVmHostImport(string name, byte arity, PdVmValueType returnType)
    {
        Name = name ?? throw new ArgumentNullException(nameof(name));
        Arity = arity;
        ReturnType = returnType;
    }

    public string Name { get; }

    public byte Arity { get; }

    public PdVmValueType ReturnType { get; }
}

public enum PdVmCallOutcomeKind
{
    Return = 0,
    Halt = 1,
    Yield = 2,
    Pending = 3,
}

public sealed class PdVmCallReturn
{
    public static readonly PdVmCallReturn None = new(Array.Empty<PdVmValue>());

    public PdVmCallReturn(IReadOnlyList<PdVmValue> values)
    {
        Values = values;
    }

    public IReadOnlyList<PdVmValue> Values { get; }

    public static PdVmCallReturn One(PdVmValue value) => new(new[] { value });

    public static PdVmCallReturn FromValues(params PdVmValue[] values) => new(values);
}

public sealed class PdVmCallOutcome
{
    private PdVmCallOutcome(PdVmCallOutcomeKind kind, PdVmCallReturn? values, ulong pendingOpId)
    {
        Kind = kind;
        ReturnValues = values ?? PdVmCallReturn.None;
        PendingOpId = pendingOpId;
    }

    public PdVmCallOutcomeKind Kind { get; }

    public PdVmCallReturn ReturnValues { get; }

    public ulong PendingOpId { get; }

    public static PdVmCallOutcome Returned(PdVmCallReturn values) => new(PdVmCallOutcomeKind.Return, values, 0);

    public static PdVmCallOutcome Halted() => new(PdVmCallOutcomeKind.Halt, PdVmCallReturn.None, 0);

    public static PdVmCallOutcome Yielded() => new(PdVmCallOutcomeKind.Yield, PdVmCallReturn.None, 0);

    public static PdVmCallOutcome Pending(ulong opId) => new(PdVmCallOutcomeKind.Pending, PdVmCallReturn.None, opId);
}

public interface IPdVmHost
{
    PdVmCallOutcome Call(string name, IReadOnlyList<PdVmValue> args);
}

public interface IAsyncPdVmHost : IPdVmHost
{
    ValueTask<PdVmCallReturn> WaitAsync(ulong opId, CancellationToken cancellationToken = default);
}
