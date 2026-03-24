namespace PdVm.Runtime;

public interface IPdVmProgram
{
    IReadOnlyList<PdVmValue> Stack { get; }

    IReadOnlyList<PdVmValue> Locals { get; }

    int InstructionPointer { get; }

    PdVmStatus RunStep(IPdVmHost host);

    void ResumePending(ulong opId, PdVmCallReturn returnValues);
}
