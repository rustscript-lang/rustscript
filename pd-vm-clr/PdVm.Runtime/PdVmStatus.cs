namespace PdVm.Runtime;

public enum PdVmStatusKind
{
    Halted = 0,
    Yielded = 1,
    Waiting = 2,
}

public readonly struct PdVmStatus
{
    private PdVmStatus(PdVmStatusKind kind, ulong waitingOpId)
    {
        Kind = kind;
        WaitingOpId = waitingOpId;
    }

    public PdVmStatusKind Kind { get; }

    public ulong WaitingOpId { get; }

    public static PdVmStatus Halted() => new(PdVmStatusKind.Halted, 0);

    public static PdVmStatus Yielded() => new(PdVmStatusKind.Yielded, 0);

    public static PdVmStatus Waiting(ulong opId) => new(PdVmStatusKind.Waiting, opId);

    public override string ToString() =>
        Kind == PdVmStatusKind.Waiting ? $"Waiting({WaitingOpId})" : Kind.ToString();
}
