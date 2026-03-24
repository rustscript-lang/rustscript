using PdVm.Runtime;

namespace PdVm.Compiler;

public enum PdVmBytecodeOpCode : byte
{
    Nop = 0x00,
    Ret = 0x01,
    Ldc = 0x02,
    Add = 0x03,
    Sub = 0x04,
    Mul = 0x05,
    Div = 0x06,
    Neg = 0x07,
    Ceq = 0x08,
    Clt = 0x09,
    Cgt = 0x0A,
    Br = 0x0B,
    Brfalse = 0x0C,
    Pop = 0x0D,
    Dup = 0x0E,
    Ldloc = 0x0F,
    Stloc = 0x10,
    Call = 0x11,
    Shl = 0x12,
    Shr = 0x13,
    Mod = 0x14,
    And = 0x15,
    Or = 0x16,
    Not = 0x17,
    Lshr = 0x18,
}

public sealed record PdVmInstruction(
    int Offset,
    PdVmBytecodeOpCode OpCode,
    int NextOffset,
    int? ConstantIndex = null,
    int? JumpTarget = null,
    byte? LocalIndex = null,
    ushort? CallIndex = null,
    byte? ArgCount = null);

public sealed class PdVmProgramModel
{
    public PdVmProgramModel(
        IReadOnlyList<PdVmValue> constants,
        byte[] code,
        int localCount,
        IReadOnlyList<PdVmHostImport> imports,
        IReadOnlyList<PdVmInstruction> instructions)
    {
        Constants = constants ?? throw new ArgumentNullException(nameof(constants));
        Code = code ?? throw new ArgumentNullException(nameof(code));
        LocalCount = localCount;
        Imports = imports ?? throw new ArgumentNullException(nameof(imports));
        Instructions = instructions ?? throw new ArgumentNullException(nameof(instructions));
    }

    public IReadOnlyList<PdVmValue> Constants { get; }

    public byte[] Code { get; }

    public int LocalCount { get; }

    public IReadOnlyList<PdVmHostImport> Imports { get; }

    public IReadOnlyList<PdVmInstruction> Instructions { get; }
}
