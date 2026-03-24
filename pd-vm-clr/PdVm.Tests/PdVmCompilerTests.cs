using System.Text;
using PdVm.Compiler;
using PdVm.Runtime;

namespace PdVm.Tests;

public sealed class PdVmCompilerTests
{
    [Fact]
    public void CompilesArithmeticAndBranchProgram()
    {
        var code = new BytecodeBuilder()
            .EmitLdc(0)
            .EmitLdc(1)
            .Emit(PdVmBytecodeOpCode.Add)
            .EmitStloc(0)
            .EmitLdc(2)
            .EmitBrfalse("use_local")
            .EmitLdc(3)
            .EmitStloc(0)
            .MarkLabel("use_local")
            .EmitLdloc(0)
            .Emit(PdVmBytecodeOpCode.Ret)
            .Build();

        var program = CompileProgram(
            constants:
            [
                PdVmValue.FromInt(1),
                PdVmValue.FromInt(2),
                PdVmValue.FromBool(false),
                PdVmValue.FromInt(999),
            ],
            code: code);

        var result = PdVmExecution.Run(program, new PdVmDelegateHost());

        Assert.Equal(PdVmStatusKind.Halted, result.Status.Kind);
        var stack = Assert.Single(program.Stack);
        Assert.Equal(3, stack.AsInt());
    }

    [Fact]
    public void RunsBuiltinLenIntrinsic()
    {
        var code = new BytecodeBuilder()
            .EmitLdc(0)
            .EmitCall(PdVmBuiltins.GetCallIndex(PdVmBuiltin.Len), 1)
            .Emit(PdVmBytecodeOpCode.Ret)
            .Build();
        var text = "ab" + char.ConvertFromUtf32(0x1F642);

        var program = CompileProgram(
            constants: [PdVmValue.FromString(text)],
            code: code);

        var result = PdVmExecution.Run(program, new PdVmDelegateHost());

        Assert.Equal(PdVmStatusKind.Halted, result.Status.Kind);
        var stack = Assert.Single(program.Stack);
        Assert.Equal(3, stack.AsInt());
    }

    [Fact]
    public void RunsSyncHostImport()
    {
        var code = new BytecodeBuilder()
            .EmitLdc(0)
            .EmitCall(0, 1)
            .Emit(PdVmBytecodeOpCode.Ret)
            .Build();

        var program = CompileProgram(
            constants: [PdVmValue.FromInt(21)],
            code: code,
            imports: [new PdVmHostImport("double", 1, PdVmValueType.Int)]);

        var host = new PdVmDelegateHost();
        host.RegisterValue("double", args => PdVmValue.FromInt(args[0].AsInt() * 2));

        var result = PdVmExecution.Run(program, host);

        Assert.Equal(PdVmStatusKind.Halted, result.Status.Kind);
        var stack = Assert.Single(program.Stack);
        Assert.Equal(42, stack.AsInt());
    }

    [Fact]
    public async Task RunsAsyncHostImport()
    {
        var code = new BytecodeBuilder()
            .EmitLdc(0)
            .EmitLdc(1)
            .EmitCall(0, 2)
            .Emit(PdVmBytecodeOpCode.Ret)
            .Build();

        var program = CompileProgram(
            constants:
            [
                PdVmValue.FromInt(20),
                PdVmValue.FromInt(22),
            ],
            code: code,
            imports: [new PdVmHostImport("delay_add", 2, PdVmValueType.Int)]);

        var host = new PdVmDelegateHost();
        host.RegisterAsyncValue(
            "delay_add",
            async (args, cancellationToken) =>
            {
                await Task.Delay(10, cancellationToken);
                return PdVmValue.FromInt(args[0].AsInt() + args[1].AsInt());
            });

        var result = await PdVmExecution.RunAsync(program, host);

        Assert.Equal(PdVmStatusKind.Halted, result.Status.Kind);
        var stack = Assert.Single(program.Stack);
        Assert.Equal(42, stack.AsInt());
    }

    [Fact]
    public void SupportsArrayConcatenationWithAddOpcode()
    {
        var code = new BytecodeBuilder()
            .EmitCall(PdVmBuiltins.GetCallIndex(PdVmBuiltin.ArrayNew), 0)
            .EmitLdc(0)
            .EmitCall(PdVmBuiltins.GetCallIndex(PdVmBuiltin.ArrayPush), 2)
            .EmitCall(PdVmBuiltins.GetCallIndex(PdVmBuiltin.ArrayNew), 0)
            .EmitLdc(1)
            .EmitCall(PdVmBuiltins.GetCallIndex(PdVmBuiltin.ArrayPush), 2)
            .Emit(PdVmBytecodeOpCode.Add)
            .Emit(PdVmBytecodeOpCode.Ret)
            .Build();

        var program = CompileProgram(
            constants:
            [
                PdVmValue.FromInt(1),
                PdVmValue.FromInt(2),
            ],
            code: code);

        var result = PdVmExecution.Run(program, new PdVmDelegateHost());

        Assert.Equal(PdVmStatusKind.Halted, result.Status.Kind);
        var array = Assert.Single(program.Stack).AsArray();
        Assert.Equal(2, array.Count);
        Assert.Equal(1, array[0].AsInt());
        Assert.Equal(2, array[1].AsInt());
    }

    [Fact]
    public void DispatchesRegexJsonAndMathBuiltins()
    {
        var payload = PdVmValue.FromMap(
        [
            new KeyValuePair<PdVmValue, PdVmValue>(PdVmValue.FromString("score"), PdVmValue.FromInt(12)),
        ]);

        var jsonOutcome = PdVmBuiltins.Dispatch(
            PdVmBuiltins.GetCallIndex(PdVmBuiltin.JsonEncode),
            [payload]);
        var json = Assert.Single(jsonOutcome.ReturnValues.Values).AsString();

        var decodedOutcome = PdVmBuiltins.Dispatch(
            PdVmBuiltins.GetCallIndex(PdVmBuiltin.JsonDecode),
            [PdVmValue.FromString(json)]);
        var decoded = Assert.Single(decodedOutcome.ReturnValues.Values).AsMap();

        var regexOutcome = PdVmBuiltins.Dispatch(
            PdVmBuiltins.GetCallIndex(PdVmBuiltin.ReMatch),
            [PdVmValue.FromString("(?i)^rustscript$"), PdVmValue.FromString("RUSTSCRIPT")]);

        var mathOutcome = PdVmBuiltins.Dispatch(
            PdVmBuiltins.GetCallIndex(PdVmBuiltin.MathRound),
            [PdVmValue.FromFloat(1.6)]);

        Assert.True(decoded.TryGetValue(PdVmValue.FromString("score"), out var score));
        Assert.Equal(12, score.AsInt());
        Assert.True(Assert.Single(regexOutcome.ReturnValues.Values).AsBool());
        Assert.Equal(2d, Assert.Single(mathOutcome.ReturnValues.Values).FloatValue);
    }

    private static IPdVmProgram CompileProgram(
        IReadOnlyList<PdVmValue> constants,
        byte[] code,
        IReadOnlyList<PdVmHostImport>? imports = null)
    {
        var payload = EncodeVmbc(constants, code, imports ?? Array.Empty<PdVmHostImport>());
        var outputPath = Path.Combine(
            Path.GetTempPath(),
            "pd-vm-clr-tests",
            $"{Guid.NewGuid():N}.dll");

        PdVmClrCompiler.Compile(
            payload,
            outputPath,
            new PdVmCompileOptions
            {
                AssemblyName = $"PdVm.Generated.{Guid.NewGuid():N}",
                TypeName = $"PdVm.Generated.Program_{Guid.NewGuid():N}",
            });

        return PdVmAssemblyLoader.LoadProgram(outputPath);
    }

    private static byte[] EncodeVmbc(
        IReadOnlyList<PdVmValue> constants,
        byte[] code,
        IReadOnlyList<PdVmHostImport> imports)
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true);

        writer.Write("VMBC"u8.ToArray());
        writer.Write((ushort)8);
        writer.Write((ushort)0);
        writer.Write((uint)constants.Count);
        foreach (var constant in constants)
        {
            WriteConstant(writer, constant);
        }

        writer.Write((uint)code.Length);
        writer.Write(code);
        writer.Write((uint)imports.Count);
        foreach (var import in imports)
        {
            WriteString(writer, import.Name);
            writer.Write(import.Arity);
            writer.Write((byte)import.ReturnType);
        }

        writer.Write((byte)0);
        writer.Write((byte)0);
        writer.Flush();
        return stream.ToArray();
    }

    private static void WriteConstant(BinaryWriter writer, PdVmValue value)
    {
        switch (value.Kind)
        {
            case PdVmValueKind.Null:
                writer.Write((byte)4);
                return;
            case PdVmValueKind.Int:
                writer.Write((byte)0);
                writer.Write(value.IntValue);
                return;
            case PdVmValueKind.Bool:
                writer.Write((byte)1);
                writer.Write((byte)(value.BoolValue ? 1 : 0));
                return;
            case PdVmValueKind.String:
                writer.Write((byte)2);
                WriteString(writer, value.AsString());
                return;
            case PdVmValueKind.Float:
                writer.Write((byte)3);
                writer.Write(value.FloatValue);
                return;
            case PdVmValueKind.Bytes:
            {
                writer.Write((byte)5);
                var bytes = value.AsBytes();
                writer.Write((uint)bytes.Length);
                writer.Write(bytes);
                return;
            }
            default:
                throw new InvalidOperationException($"test constant kind {value.Kind} is not supported");
        }
    }

    private static void WriteString(BinaryWriter writer, string value)
    {
        var bytes = Encoding.UTF8.GetBytes(value);
        writer.Write((uint)bytes.Length);
        writer.Write(bytes);
    }

    private sealed class BytecodeBuilder
    {
        private readonly List<byte> _code = new();
        private readonly Dictionary<string, int> _labels = new(StringComparer.Ordinal);
        private readonly List<(int Position, string Label)> _jumps = new();

        public BytecodeBuilder MarkLabel(string label)
        {
            _labels[label] = _code.Count;
            return this;
        }

        public BytecodeBuilder Emit(PdVmBytecodeOpCode opCode)
        {
            _code.Add((byte)opCode);
            return this;
        }

        public BytecodeBuilder EmitLdc(uint constantIndex)
        {
            _code.Add((byte)PdVmBytecodeOpCode.Ldc);
            _code.AddRange(BitConverter.GetBytes(constantIndex));
            return this;
        }

        public BytecodeBuilder EmitLdloc(byte index)
        {
            _code.Add((byte)PdVmBytecodeOpCode.Ldloc);
            _code.Add(index);
            return this;
        }

        public BytecodeBuilder EmitStloc(byte index)
        {
            _code.Add((byte)PdVmBytecodeOpCode.Stloc);
            _code.Add(index);
            return this;
        }

        public BytecodeBuilder EmitCall(ushort callIndex, byte argCount)
        {
            _code.Add((byte)PdVmBytecodeOpCode.Call);
            _code.AddRange(BitConverter.GetBytes(callIndex));
            _code.Add(argCount);
            return this;
        }

        public BytecodeBuilder EmitBrfalse(string label)
        {
            _code.Add((byte)PdVmBytecodeOpCode.Brfalse);
            _jumps.Add((_code.Count, label));
            _code.AddRange(new byte[4]);
            return this;
        }

        public byte[] Build()
        {
            foreach (var (position, label) in _jumps)
            {
                if (!_labels.TryGetValue(label, out var target))
                {
                    throw new InvalidOperationException($"undefined bytecode label '{label}'");
                }

                var bytes = BitConverter.GetBytes((uint)target);
                for (var index = 0; index < bytes.Length; index++)
                {
                    _code[position + index] = bytes[index];
                }
            }

            return _code.ToArray();
        }
    }
}
