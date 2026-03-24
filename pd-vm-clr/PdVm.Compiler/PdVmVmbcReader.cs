using System.Text;
using PdVm.Runtime;

namespace PdVm.Compiler;

public static class PdVmVmbcReader
{
    private static readonly byte[] Magic = "VMBC"u8.ToArray();

    private const ushort Version = 8;
    private const ushort Flags = 0;

    public static PdVmProgramModel ReadFile(string path)
    {
        if (path is null)
        {
            throw new ArgumentNullException(nameof(path));
        }

        return ReadBytes(File.ReadAllBytes(path));
    }

    public static PdVmProgramModel ReadBytes(byte[] bytes)
    {
        if (bytes is null)
        {
            throw new ArgumentNullException(nameof(bytes));
        }

        var cursor = new Cursor(bytes);
        var magic = cursor.ReadExact(4);
        if (!magic.SequenceEqual(Magic))
        {
            throw new PdVmCompilerException($"invalid VMBC magic: {Convert.ToHexString(magic)}");
        }

        var version = cursor.ReadUInt16();
        if (version != Version)
        {
            throw new PdVmCompilerException($"unsupported VMBC version {version}, expected {Version}");
        }

        var flags = cursor.ReadUInt16();
        if (flags != Flags)
        {
            throw new PdVmCompilerException($"unsupported VMBC flags {flags}, expected {Flags}");
        }

        var constantCount = checked((int)cursor.ReadUInt32());
        var constants = new List<PdVmValue>(constantCount);
        for (var index = 0; index < constantCount; index++)
        {
            constants.Add(ReadConstant(ref cursor));
        }

        var codeLength = checked((int)cursor.ReadUInt32());
        var code = cursor.ReadExact(codeLength).ToArray();

        var importCount = checked((int)cursor.ReadUInt32());
        var imports = new List<PdVmHostImport>(importCount);
        for (var index = 0; index < importCount; index++)
        {
            var name = cursor.ReadString();
            var arity = cursor.ReadByte();
            var returnType = ReadValueType(cursor.ReadByte());
            imports.Add(new PdVmHostImport(name, arity, returnType));
        }

        SkipTypeMap(ref cursor);
        SkipDebugInfo(ref cursor);

        if (!cursor.IsEof)
        {
            throw new PdVmCompilerException("trailing bytes after VMBC payload");
        }

        var instructions = DecodeInstructions(code, constants.Count, imports);
        var localCount = InferLocalCount(instructions);
        return new PdVmProgramModel(constants, code, localCount, imports, instructions);
    }

    private static PdVmValue ReadConstant(ref Cursor cursor)
    {
        return cursor.ReadByte() switch
        {
            0 => PdVmValue.FromInt(cursor.ReadInt64()),
            1 => cursor.ReadByte() switch
            {
                0 => PdVmValue.FromBool(false),
                1 => PdVmValue.FromBool(true),
                var value => throw new PdVmCompilerException($"invalid VMBC bool literal {value}"),
            },
            2 => PdVmValue.FromString(cursor.ReadString()),
            3 => PdVmValue.FromFloat(cursor.ReadDouble()),
            4 => PdVmValue.Null(),
            5 => PdVmValue.FromBytes(cursor.ReadExact(checked((int)cursor.ReadUInt32())).ToArray()),
            var tag => throw new PdVmCompilerException($"invalid VMBC constant tag {tag}"),
        };
    }

    private static IReadOnlyList<PdVmInstruction> DecodeInstructions(
        byte[] code,
        int constantCount,
        IReadOnlyList<PdVmHostImport> imports)
    {
        var instructions = new List<PdVmInstruction>();
        var instructionStarts = new HashSet<int>();
        var jumpTargets = new List<(int Offset, int Target)>();
        var ip = 0;

        while (ip < code.Length)
        {
            var offset = ip;
            instructionStarts.Add(offset);
            var op = ParseOpcode(code[ip]);
            ip++;

            PdVmInstruction instruction;
            switch (op)
            {
                case PdVmBytecodeOpCode.Nop:
                case PdVmBytecodeOpCode.Ret:
                case PdVmBytecodeOpCode.Add:
                case PdVmBytecodeOpCode.Sub:
                case PdVmBytecodeOpCode.Mul:
                case PdVmBytecodeOpCode.Div:
                case PdVmBytecodeOpCode.Neg:
                case PdVmBytecodeOpCode.Ceq:
                case PdVmBytecodeOpCode.Clt:
                case PdVmBytecodeOpCode.Cgt:
                case PdVmBytecodeOpCode.Pop:
                case PdVmBytecodeOpCode.Dup:
                case PdVmBytecodeOpCode.Shl:
                case PdVmBytecodeOpCode.Shr:
                case PdVmBytecodeOpCode.Mod:
                case PdVmBytecodeOpCode.And:
                case PdVmBytecodeOpCode.Or:
                case PdVmBytecodeOpCode.Not:
                case PdVmBytecodeOpCode.Lshr:
                    instruction = new PdVmInstruction(offset, op, ip);
                    break;
                case PdVmBytecodeOpCode.Ldc:
                {
                    var constantIndex = checked((int)ReadUInt32Operand(code, ref ip, offset, op, 4));
                    if (constantIndex < 0 || constantIndex >= constantCount)
                    {
                        throw new PdVmCompilerException(
                            $"ldc at offset {offset} references invalid constant index {constantIndex}");
                    }

                    instruction = new PdVmInstruction(offset, op, ip, ConstantIndex: constantIndex);
                    break;
                }
                case PdVmBytecodeOpCode.Br:
                case PdVmBytecodeOpCode.Brfalse:
                {
                    var target = checked((int)ReadUInt32Operand(code, ref ip, offset, op, 4));
                    jumpTargets.Add((offset, target));
                    instruction = new PdVmInstruction(offset, op, ip, JumpTarget: target);
                    break;
                }
                case PdVmBytecodeOpCode.Ldloc:
                case PdVmBytecodeOpCode.Stloc:
                {
                    var localIndex = ReadByteOperand(code, ref ip, offset, op, 1);
                    instruction = new PdVmInstruction(offset, op, ip, LocalIndex: localIndex);
                    break;
                }
                case PdVmBytecodeOpCode.Call:
                {
                    var callIndex = ReadUInt16Operand(code, ref ip, offset, op, 3);
                    var argCount = ReadByteOperand(code, ref ip, offset, op, 3);
                    ValidateCall(offset, callIndex, argCount, imports);
                    instruction = new PdVmInstruction(offset, op, ip, CallIndex: callIndex, ArgCount: argCount);
                    break;
                }
                default:
                    throw new PdVmCompilerException($"invalid opcode 0x{(byte)op:X2} at offset {offset}");
            }

            instructions.Add(instruction);
        }

        foreach (var (offset, target) in jumpTargets)
        {
            if (!instructionStarts.Contains(target))
            {
                throw new PdVmCompilerException(
                    $"jump at offset {offset} targets invalid instruction boundary {target}");
            }
        }

        return instructions;
    }

    private static void ValidateCall(
        int offset,
        ushort callIndex,
        byte argCount,
        IReadOnlyList<PdVmHostImport> imports)
    {
        if (PdVmBuiltins.TryGetBuiltin(callIndex, out var builtin))
        {
            var expected = PdVmBuiltins.GetArity(builtin);
            if (expected != argCount)
            {
                throw new PdVmCompilerException(
                    $"builtin call 0x{callIndex:X4} at offset {offset} expects arity {expected}, got {argCount}");
            }

            return;
        }

        if (PdVmBuiltins.IsBuiltinIndex(callIndex))
        {
            throw new PdVmCompilerException(
                $"builtin call 0x{callIndex:X4} at offset {offset} is not supported by PdVm.Runtime yet");
        }

        if (callIndex >= imports.Count)
        {
            throw new PdVmCompilerException(
                $"import call at offset {offset} references invalid import index {callIndex}");
        }

        var import = imports[callIndex];
        if (import.Arity != argCount)
        {
            throw new PdVmCompilerException(
                $"import '{import.Name}' at offset {offset} expects arity {import.Arity}, got {argCount}");
        }
    }

    private static int InferLocalCount(IEnumerable<PdVmInstruction> instructions)
    {
        var maxLocal = -1;
        foreach (var instruction in instructions)
        {
            if ((instruction.OpCode == PdVmBytecodeOpCode.Ldloc || instruction.OpCode == PdVmBytecodeOpCode.Stloc) &&
                instruction.LocalIndex is byte localIndex)
            {
                maxLocal = Math.Max(maxLocal, localIndex);
            }
        }

        return maxLocal + 1;
    }

    private static PdVmBytecodeOpCode ParseOpcode(byte raw)
    {
        if (Enum.IsDefined(typeof(PdVmBytecodeOpCode), raw))
        {
            return (PdVmBytecodeOpCode)raw;
        }

        throw new PdVmCompilerException($"invalid opcode 0x{raw:X2}");
    }

    private static void SkipTypeMap(ref Cursor cursor)
    {
        switch (cursor.ReadByte())
        {
            case 0:
                return;
            case 1:
            {
                var localCount = checked((int)cursor.ReadUInt32());
                for (var index = 0; index < localCount; index++)
                {
                    _ = ReadValueType(cursor.ReadByte());
                }

                var operandCount = checked((int)cursor.ReadUInt32());
                for (var index = 0; index < operandCount; index++)
                {
                    _ = cursor.ReadUInt32();
                    _ = ReadValueType(cursor.ReadByte());
                    _ = ReadValueType(cursor.ReadByte());
                }

                return;
            }
            default:
                throw new PdVmCompilerException("invalid type map flag in VMBC payload");
        }
    }

    private static void SkipDebugInfo(ref Cursor cursor)
    {
        switch (cursor.ReadByte())
        {
            case 0:
                return;
            case 1:
            {
                switch (cursor.ReadByte())
                {
                    case 0:
                        break;
                    case 1:
                        _ = cursor.ReadString();
                        break;
                    default:
                        throw new PdVmCompilerException("invalid debug source flag in VMBC payload");
                }

                var lineCount = checked((int)cursor.ReadUInt32());
                cursor.Skip(checked(lineCount * 8));

                var functionCount = checked((int)cursor.ReadUInt32());
                for (var functionIndex = 0; functionIndex < functionCount; functionIndex++)
                {
                    _ = cursor.ReadString();
                    var argCount = checked((int)cursor.ReadUInt32());
                    for (var argIndex = 0; argIndex < argCount; argIndex++)
                    {
                        _ = cursor.ReadString();
                        _ = cursor.ReadByte();
                    }
                }

                var localCount = checked((int)cursor.ReadUInt32());
                for (var localIndex = 0; localIndex < localCount; localIndex++)
                {
                    _ = cursor.ReadString();
                    _ = cursor.ReadByte();
                    SkipOptionalUInt32(ref cursor);
                    SkipOptionalUInt32(ref cursor);
                }

                return;
            }
            default:
                throw new PdVmCompilerException("invalid debug info flag in VMBC payload");
        }
    }

    private static void SkipOptionalUInt32(ref Cursor cursor)
    {
        switch (cursor.ReadByte())
        {
            case 0:
                return;
            case 1:
                _ = cursor.ReadUInt32();
                return;
            default:
                throw new PdVmCompilerException("invalid optional u32 flag in debug payload");
        }
    }

    private static PdVmValueType ReadValueType(byte raw)
    {
        return raw switch
        {
            0 => PdVmValueType.Unknown,
            1 => PdVmValueType.Null,
            2 => PdVmValueType.Int,
            3 => PdVmValueType.Float,
            4 => PdVmValueType.Bool,
            5 => PdVmValueType.String,
            6 => PdVmValueType.Bytes,
            7 => PdVmValueType.Array,
            8 => PdVmValueType.Map,
            _ => throw new PdVmCompilerException($"invalid value type tag {raw}"),
        };
    }

    private static byte ReadByteOperand(byte[] code, ref int ip, int offset, PdVmBytecodeOpCode opCode, int expectedBytes)
    {
        if (ip >= code.Length)
        {
            throw new PdVmCompilerException(
                $"truncated operand for {opCode} at offset {offset}, expected {expectedBytes} bytes");
        }

        return code[ip++];
    }

    private static ushort ReadUInt16Operand(byte[] code, ref int ip, int offset, PdVmBytecodeOpCode opCode, int expectedBytes)
    {
        if (ip + 1 >= code.Length)
        {
            throw new PdVmCompilerException(
                $"truncated operand for {opCode} at offset {offset}, expected {expectedBytes} bytes");
        }

        var value = BitConverter.ToUInt16(code, ip);
        ip += 2;
        return value;
    }

    private static uint ReadUInt32Operand(byte[] code, ref int ip, int offset, PdVmBytecodeOpCode opCode, int expectedBytes)
    {
        if (ip + 3 >= code.Length)
        {
            throw new PdVmCompilerException(
                $"truncated operand for {opCode} at offset {offset}, expected {expectedBytes} bytes");
        }

        var value = BitConverter.ToUInt32(code, ip);
        ip += 4;
        return value;
    }

    private ref struct Cursor
    {
        private readonly ReadOnlySpan<byte> _bytes;
        private int _offset;

        public Cursor(ReadOnlySpan<byte> bytes)
        {
            _bytes = bytes;
            _offset = 0;
        }

        public bool IsEof => _offset == _bytes.Length;

        public byte ReadByte()
        {
            if (_offset >= _bytes.Length)
            {
                throw new PdVmCompilerException("unexpected end of VMBC payload");
            }

            return _bytes[_offset++];
        }

        public ushort ReadUInt16()
        {
            var value = ReadExact(2);
            return BitConverter.ToUInt16(value);
        }

        public uint ReadUInt32()
        {
            var value = ReadExact(4);
            return BitConverter.ToUInt32(value);
        }

        public long ReadInt64()
        {
            var value = ReadExact(8);
            return BitConverter.ToInt64(value);
        }

        public double ReadDouble()
        {
            var value = ReadExact(8);
            return BitConverter.ToDouble(value);
        }

        public string ReadString()
        {
            var length = checked((int)ReadUInt32());
            var bytes = ReadExact(length);
            return Encoding.UTF8.GetString(bytes);
        }

        public ReadOnlySpan<byte> ReadExact(int length)
        {
            if (_offset + length > _bytes.Length)
            {
                throw new PdVmCompilerException("unexpected end of VMBC payload");
            }

            var slice = _bytes.Slice(_offset, length);
            _offset += length;
            return slice;
        }

        public void Skip(int length)
        {
            _ = ReadExact(length);
        }
    }
}
