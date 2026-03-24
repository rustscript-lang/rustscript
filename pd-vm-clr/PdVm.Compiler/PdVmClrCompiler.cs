using System.Reflection;
using System.Reflection.Emit;
using PdVm.Runtime;

namespace PdVm.Compiler;

public sealed class PdVmCompileOptions
{
    public string? AssemblyName { get; init; }

    public string? ModuleName { get; init; }

    public string TypeName { get; init; } = "PdVm.Generated.Program";
}

public static class PdVmClrCompiler
{
    private static readonly ConstructorInfo ProgramBaseConstructor =
        typeof(PdVmProgramBase).GetConstructor(
            BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null,
            types: new[] { typeof(int) },
            modifiers: null) ?? throw new InvalidOperationException("PdVmProgramBase(int) constructor not found");

    private static readonly ConstructorInfo HostImportConstructor =
        typeof(PdVmHostImport).GetConstructor(new[] { typeof(string), typeof(byte), typeof(PdVmValueType) }) ??
        throw new InvalidOperationException("PdVmHostImport constructor not found");

    private static readonly ConstructorInfo InvalidOperationConstructor =
        typeof(InvalidOperationException).GetConstructor(new[] { typeof(string) }) ??
        throw new InvalidOperationException("InvalidOperationException(string) constructor not found");

    private static readonly MethodInfo EnsureReadyToRunStepMethod =
        GetBaseMethod("EnsureReadyToRunStep");

    private static readonly MethodInfo YieldProgramMethod =
        GetBaseMethod("YieldProgram");

    private static readonly MethodInfo HaltProgramMethod =
        GetBaseMethod("HaltProgram");

    private static readonly MethodInfo SetInstructionPointerMethod =
        GetBaseMethod("SetInstructionPointer", typeof(int));

    private static readonly MethodInfo PushValueMethod =
        GetBaseMethod("PushValue", typeof(PdVmValue));

    private static readonly MethodInfo PopValueMethod =
        GetBaseMethod("PopValue");

    private static readonly MethodInfo DuplicateTopMethod =
        GetBaseMethod("DuplicateTop");

    private static readonly MethodInfo DiscardTopMethod =
        GetBaseMethod("DiscardTop");

    private static readonly MethodInfo LoadLocalValueMethod =
        GetBaseMethod("LoadLocalValue", typeof(byte));

    private static readonly MethodInfo StoreLocalValueMethod =
        GetBaseMethod("StoreLocalValue", typeof(byte));

    private static readonly MethodInfo PopBoolMethod =
        GetBaseMethod("PopBool");

    private static readonly MethodInfo DispatchCallMethod =
        GetBaseMethod(
            "DispatchCall",
            typeof(IPdVmHost),
            typeof(PdVmHostImport[]),
            typeof(ushort),
            typeof(byte),
            typeof(int),
            typeof(int));

    private static readonly MethodInfo GetLastStatusMethod =
        GetBaseMethod("GetLastStatus");

    private static readonly MethodInfo InstructionPointerGetter =
        typeof(PdVmProgramBase).GetProperty(nameof(IPdVmProgram.InstructionPointer))?.GetMethod ??
        throw new InvalidOperationException("InstructionPointer getter not found");

    private static readonly MethodInfo ValueNullMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.Null), Type.EmptyTypes) ??
        throw new InvalidOperationException("PdVmValue.Null not found");

    private static readonly MethodInfo ValueFromIntMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.FromInt), new[] { typeof(long) }) ??
        throw new InvalidOperationException("PdVmValue.FromInt not found");

    private static readonly MethodInfo ValueFromFloatMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.FromFloat), new[] { typeof(double) }) ??
        throw new InvalidOperationException("PdVmValue.FromFloat not found");

    private static readonly MethodInfo ValueFromBoolMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.FromBool), new[] { typeof(bool) }) ??
        throw new InvalidOperationException("PdVmValue.FromBool not found");

    private static readonly MethodInfo ValueFromStringMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.FromString), new[] { typeof(string) }) ??
        throw new InvalidOperationException("PdVmValue.FromString not found");

    private static readonly MethodInfo ValueFromBytesMethod =
        typeof(PdVmValue).GetMethod(nameof(PdVmValue.FromBytes), new[] { typeof(IEnumerable<byte>) }) ??
        throw new InvalidOperationException("PdVmValue.FromBytes not found");

    private static readonly Dictionary<PdVmBytecodeOpCode, MethodInfo> UnaryOpcodeMethods = new()
    {
        [PdVmBytecodeOpCode.Neg] = GetBaseMethod("ApplyNeg"),
        [PdVmBytecodeOpCode.Not] = GetBaseMethod("ApplyNot"),
    };

    private static readonly Dictionary<PdVmBytecodeOpCode, MethodInfo> BinaryOpcodeMethods = new()
    {
        [PdVmBytecodeOpCode.Add] = GetBaseMethod("ApplyAdd"),
        [PdVmBytecodeOpCode.Sub] = GetBaseMethod("ApplySub"),
        [PdVmBytecodeOpCode.Mul] = GetBaseMethod("ApplyMul"),
        [PdVmBytecodeOpCode.Div] = GetBaseMethod("ApplyDiv"),
        [PdVmBytecodeOpCode.Mod] = GetBaseMethod("ApplyMod"),
        [PdVmBytecodeOpCode.Ceq] = GetBaseMethod("ApplyEqual"),
        [PdVmBytecodeOpCode.Clt] = GetBaseMethod("ApplyLessThan"),
        [PdVmBytecodeOpCode.Cgt] = GetBaseMethod("ApplyGreaterThan"),
        [PdVmBytecodeOpCode.Shl] = GetBaseMethod("ApplyShl"),
        [PdVmBytecodeOpCode.Shr] = GetBaseMethod("ApplyShr"),
        [PdVmBytecodeOpCode.Lshr] = GetBaseMethod("ApplyLshr"),
        [PdVmBytecodeOpCode.And] = GetBaseMethod("ApplyAnd"),
        [PdVmBytecodeOpCode.Or] = GetBaseMethod("ApplyOr"),
    };

    private static readonly Dictionary<PdVmBuiltin, (MethodInfo Method, bool ReturnsValue)> IntrinsicBuiltins = new()
    {
        [PdVmBuiltin.Len] = (GetBuiltinMethod(nameof(PdVmBuiltins.LenValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Slice] = (GetBuiltinMethod(nameof(PdVmBuiltins.SliceValue), typeof(PdVmValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Concat] = (GetBuiltinMethod(nameof(PdVmBuiltins.ConcatValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.ArrayNew] = (GetBuiltinMethod(nameof(PdVmBuiltins.ArrayNewValue)), true),
        [PdVmBuiltin.ArrayPush] = (GetBuiltinMethod(nameof(PdVmBuiltins.ArrayPushValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.MapNew] = (GetBuiltinMethod(nameof(PdVmBuiltins.MapNewValue)), true),
        [PdVmBuiltin.Get] = (GetBuiltinMethod(nameof(PdVmBuiltins.GetValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Has] = (GetBuiltinMethod(nameof(PdVmBuiltins.HasValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Set] = (GetBuiltinMethod(nameof(PdVmBuiltins.SetValue), typeof(PdVmValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Keys] = (GetBuiltinMethod(nameof(PdVmBuiltins.KeysValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Count] = (GetBuiltinMethod(nameof(PdVmBuiltins.CountValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.FormatTemplate] = (GetBuiltinMethod(nameof(PdVmBuiltins.FormatTemplateValue), typeof(PdVmValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.ToString] = (GetBuiltinMethod(nameof(PdVmBuiltins.ToStringValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.TypeOf] = (GetBuiltinMethod(nameof(PdVmBuiltins.TypeOfValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.Assert] = (GetBuiltinMethod(nameof(PdVmBuiltins.AssertValue), typeof(PdVmValue)), false),
        [PdVmBuiltin.BytesFromUtf8] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesFromUtf8Value), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesToUtf8] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesToUtf8Value), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesToUtf8Lossy] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesToUtf8LossyValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesFromHex] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesFromHexValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesToHex] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesToHexValue), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesFromBase64] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesFromBase64Value), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesToBase64] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesToBase64Value), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesFromArrayU8] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesFromArrayU8Value), typeof(PdVmValue)), true),
        [PdVmBuiltin.BytesToArrayU8] = (GetBuiltinMethod(nameof(PdVmBuiltins.BytesToArrayU8Value), typeof(PdVmValue)), true),
    };

    public static string CompileFile(string inputPath, string outputPath, PdVmCompileOptions? options = null)
    {
        if (inputPath is null)
        {
            throw new ArgumentNullException(nameof(inputPath));
        }

        return Compile(PdVmVmbcReader.ReadFile(inputPath), outputPath, options);
    }

    public static string Compile(byte[] bytes, string outputPath, PdVmCompileOptions? options = null) =>
        Compile(PdVmVmbcReader.ReadBytes(bytes), outputPath, options);

    public static string Compile(PdVmProgramModel program, string outputPath, PdVmCompileOptions? options = null)
    {
        if (program is null)
        {
            throw new ArgumentNullException(nameof(program));
        }

        if (outputPath is null)
        {
            throw new ArgumentNullException(nameof(outputPath));
        }

        options ??= new PdVmCompileOptions();
        var fullOutputPath = Path.GetFullPath(outputPath);
        Directory.CreateDirectory(Path.GetDirectoryName(fullOutputPath)!);

        var assemblyName = string.IsNullOrWhiteSpace(options.AssemblyName)
            ? Path.GetFileNameWithoutExtension(fullOutputPath)
            : options.AssemblyName;
        var moduleName = string.IsNullOrWhiteSpace(options.ModuleName)
            ? Path.GetFileName(fullOutputPath)
            : options.ModuleName;

        var assemblyBuilder = new PersistedAssemblyBuilder(new AssemblyName(assemblyName), typeof(object).Assembly);
        var moduleBuilder = assemblyBuilder.DefineDynamicModule(moduleName);
        var typeBuilder = moduleBuilder.DefineType(
            options.TypeName,
            TypeAttributes.Public | TypeAttributes.Class | TypeAttributes.Sealed,
            typeof(PdVmProgramBase));

        var constantsField = typeBuilder.DefineField(
            "s_constants",
            typeof(PdVmValue[]),
            FieldAttributes.Private | FieldAttributes.Static | FieldAttributes.InitOnly);
        var importsField = typeBuilder.DefineField(
            "s_imports",
            typeof(PdVmHostImport[]),
            FieldAttributes.Private | FieldAttributes.Static | FieldAttributes.InitOnly);

        EmitTypeInitializer(typeBuilder, constantsField, importsField, program);
        EmitConstructor(typeBuilder, program.LocalCount);
        EmitRunStep(typeBuilder, constantsField, importsField, program);
        typeBuilder.CreateType();
        assemblyBuilder.Save(fullOutputPath);
        return fullOutputPath;
    }

    private static void EmitTypeInitializer(
        TypeBuilder typeBuilder,
        FieldBuilder constantsField,
        FieldBuilder importsField,
        PdVmProgramModel program)
    {
        var cctor = typeBuilder.DefineTypeInitializer();
        var il = cctor.GetILGenerator();

        EmitInt32(il, program.Constants.Count);
        il.Emit(OpCodes.Newarr, typeof(PdVmValue));
        for (var index = 0; index < program.Constants.Count; index++)
        {
            il.Emit(OpCodes.Dup);
            EmitInt32(il, index);
            EmitConstant(il, program.Constants[index]);
            il.Emit(OpCodes.Stelem_Ref);
        }
        il.Emit(OpCodes.Stsfld, constantsField);

        EmitInt32(il, program.Imports.Count);
        il.Emit(OpCodes.Newarr, typeof(PdVmHostImport));
        for (var index = 0; index < program.Imports.Count; index++)
        {
            var import = program.Imports[index];
            il.Emit(OpCodes.Dup);
            EmitInt32(il, index);
            il.Emit(OpCodes.Ldstr, import.Name);
            EmitInt32(il, import.Arity);
            EmitInt32(il, (int)import.ReturnType);
            il.Emit(OpCodes.Newobj, HostImportConstructor);
            il.Emit(OpCodes.Stelem_Ref);
        }
        il.Emit(OpCodes.Stsfld, importsField);
        il.Emit(OpCodes.Ret);
    }

    private static void EmitConstructor(TypeBuilder typeBuilder, int localCount)
    {
        var ctor = typeBuilder.DefineConstructor(
            MethodAttributes.Public,
            CallingConventions.HasThis,
            Type.EmptyTypes);
        var il = ctor.GetILGenerator();
        il.Emit(OpCodes.Ldarg_0);
        EmitInt32(il, localCount);
        il.Emit(OpCodes.Call, ProgramBaseConstructor);
        il.Emit(OpCodes.Ret);
    }

    private static void EmitRunStep(
        TypeBuilder typeBuilder,
        FieldBuilder constantsField,
        FieldBuilder importsField,
        PdVmProgramModel program)
    {
        var method = typeBuilder.DefineMethod(
            nameof(IPdVmProgram.RunStep),
            MethodAttributes.Public | MethodAttributes.HideBySig | MethodAttributes.Virtual,
            typeof(PdVmStatus),
            new[] { typeof(IPdVmHost) });
        typeBuilder.DefineMethodOverride(method, typeof(IPdVmProgram).GetMethod(nameof(IPdVmProgram.RunStep))!);

        var il = method.GetILGenerator();
        var instructionPointerLocal = il.DeclareLocal(typeof(int));
        var tmp0 = il.DeclareLocal(typeof(PdVmValue));
        var tmp1 = il.DeclareLocal(typeof(PdVmValue));
        var tmp2 = il.DeclareLocal(typeof(PdVmValue));
        var labels = program.Instructions.ToDictionary(instruction => instruction.Offset, _ => il.DefineLabel());

        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Call, EnsureReadyToRunStepMethod);
        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Call, InstructionPointerGetter);
        il.Emit(OpCodes.Stloc, instructionPointerLocal);

        foreach (var instruction in program.Instructions)
        {
            il.Emit(OpCodes.Ldloc, instructionPointerLocal);
            EmitInt32(il, instruction.Offset);
            il.Emit(OpCodes.Beq, labels[instruction.Offset]);
        }

        EmitThrowInvalidInstructionPointer(il);

        foreach (var instruction in program.Instructions)
        {
            il.MarkLabel(labels[instruction.Offset]);
            EmitInstruction(il, constantsField, importsField, instruction, tmp0, tmp1, tmp2);
        }
    }

    private static void EmitInstruction(
        ILGenerator il,
        FieldBuilder constantsField,
        FieldBuilder importsField,
        PdVmInstruction instruction,
        LocalBuilder tmp0,
        LocalBuilder tmp1,
        LocalBuilder tmp2)
    {
        if (BinaryOpcodeMethods.TryGetValue(instruction.OpCode, out var binaryMethod))
        {
            il.Emit(OpCodes.Ldarg_0);
            il.Emit(OpCodes.Call, binaryMethod);
            EmitAdvanceAndYield(il, instruction.NextOffset);
            return;
        }

        if (UnaryOpcodeMethods.TryGetValue(instruction.OpCode, out var unaryMethod))
        {
            il.Emit(OpCodes.Ldarg_0);
            il.Emit(OpCodes.Call, unaryMethod);
            EmitAdvanceAndYield(il, instruction.NextOffset);
            return;
        }

        switch (instruction.OpCode)
        {
            case PdVmBytecodeOpCode.Nop:
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Ret:
                il.Emit(OpCodes.Ldarg_0);
                il.Emit(OpCodes.Call, HaltProgramMethod);
                il.Emit(OpCodes.Ret);
                return;
            case PdVmBytecodeOpCode.Ldc:
                il.Emit(OpCodes.Ldarg_0);
                il.Emit(OpCodes.Ldsfld, constantsField);
                EmitInt32(il, instruction.ConstantIndex!.Value);
                il.Emit(OpCodes.Ldelem_Ref);
                il.Emit(OpCodes.Call, PushValueMethod);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Br:
                EmitAdvanceAndYield(il, instruction.JumpTarget!.Value);
                return;
            case PdVmBytecodeOpCode.Brfalse:
            {
                var fallthrough = il.DefineLabel();
                il.Emit(OpCodes.Ldarg_0);
                il.Emit(OpCodes.Call, PopBoolMethod);
                il.Emit(OpCodes.Brtrue, fallthrough);
                EmitAdvanceAndYield(il, instruction.JumpTarget!.Value);
                il.MarkLabel(fallthrough);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            }
            case PdVmBytecodeOpCode.Pop:
                il.Emit(OpCodes.Ldarg_0);
                il.Emit(OpCodes.Call, DiscardTopMethod);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Dup:
                il.Emit(OpCodes.Ldarg_0);
                il.Emit(OpCodes.Call, DuplicateTopMethod);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Ldloc:
                il.Emit(OpCodes.Ldarg_0);
                EmitInt32(il, instruction.LocalIndex!.Value);
                il.Emit(OpCodes.Call, LoadLocalValueMethod);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Stloc:
                il.Emit(OpCodes.Ldarg_0);
                EmitInt32(il, instruction.LocalIndex!.Value);
                il.Emit(OpCodes.Call, StoreLocalValueMethod);
                EmitAdvanceAndYield(il, instruction.NextOffset);
                return;
            case PdVmBytecodeOpCode.Call:
                EmitCallInstruction(il, importsField, instruction, tmp0, tmp1, tmp2);
                return;
            default:
                throw new PdVmCompilerException($"unsupported opcode {instruction.OpCode}");
        }
    }

    private static void EmitCallInstruction(
        ILGenerator il,
        FieldBuilder importsField,
        PdVmInstruction instruction,
        LocalBuilder tmp0,
        LocalBuilder tmp1,
        LocalBuilder tmp2)
    {
        if (instruction.CallIndex is ushort callIndex &&
            PdVmBuiltins.TryGetBuiltin(callIndex, out var builtin) &&
            IntrinsicBuiltins.TryGetValue(builtin, out var intrinsic))
        {
            EmitPopArgs(il, instruction.ArgCount!.Value, tmp0, tmp1, tmp2);
            if (intrinsic.ReturnsValue)
            {
                il.Emit(OpCodes.Ldarg_0);
                EmitIntrinsicArgs(il, instruction.ArgCount!.Value, tmp0, tmp1, tmp2);
                il.Emit(OpCodes.Call, intrinsic.Method);
                il.Emit(OpCodes.Call, PushValueMethod);
            }
            else
            {
                EmitIntrinsicArgs(il, instruction.ArgCount!.Value, tmp0, tmp1, tmp2);
                il.Emit(OpCodes.Call, intrinsic.Method);
            }

            EmitAdvanceAndYield(il, instruction.NextOffset);
            return;
        }

        var continueLabel = il.DefineLabel();
        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Ldarg_1);
        il.Emit(OpCodes.Ldsfld, importsField);
        EmitInt32(il, instruction.CallIndex!.Value);
        EmitInt32(il, instruction.ArgCount!.Value);
        EmitInt32(il, instruction.Offset);
        EmitInt32(il, instruction.NextOffset);
        il.Emit(OpCodes.Call, DispatchCallMethod);
        il.Emit(OpCodes.Brfalse, continueLabel);
        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Call, GetLastStatusMethod);
        il.Emit(OpCodes.Ret);
        il.MarkLabel(continueLabel);
        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Call, YieldProgramMethod);
        il.Emit(OpCodes.Ret);
    }

    private static void EmitPopArgs(
        ILGenerator il,
        int argc,
        LocalBuilder tmp0,
        LocalBuilder tmp1,
        LocalBuilder tmp2)
    {
        var locals = new[] { tmp0, tmp1, tmp2 };
        if (argc < 0 || argc > locals.Length)
        {
            throw new PdVmCompilerException($"intrinsic arity {argc} is not supported");
        }

        for (var index = argc - 1; index >= 0; index--)
        {
            il.Emit(OpCodes.Ldarg_0);
            il.Emit(OpCodes.Call, PopValueMethod);
            il.Emit(OpCodes.Stloc, locals[index]);
        }
    }

    private static void EmitIntrinsicArgs(
        ILGenerator il,
        int argc,
        LocalBuilder tmp0,
        LocalBuilder tmp1,
        LocalBuilder tmp2)
    {
        if (argc >= 1)
        {
            il.Emit(OpCodes.Ldloc, tmp0);
        }

        if (argc >= 2)
        {
            il.Emit(OpCodes.Ldloc, tmp1);
        }

        if (argc >= 3)
        {
            il.Emit(OpCodes.Ldloc, tmp2);
        }
    }

    private static void EmitAdvanceAndYield(ILGenerator il, int nextOffset)
    {
        il.Emit(OpCodes.Ldarg_0);
        EmitInt32(il, nextOffset);
        il.Emit(OpCodes.Call, SetInstructionPointerMethod);
        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Call, YieldProgramMethod);
        il.Emit(OpCodes.Ret);
    }

    private static void EmitConstant(ILGenerator il, PdVmValue value)
    {
        switch (value.Kind)
        {
            case PdVmValueKind.Null:
                il.Emit(OpCodes.Call, ValueNullMethod);
                return;
            case PdVmValueKind.Int:
                il.Emit(OpCodes.Ldc_I8, value.IntValue);
                il.Emit(OpCodes.Call, ValueFromIntMethod);
                return;
            case PdVmValueKind.Float:
                il.Emit(OpCodes.Ldc_R8, value.FloatValue);
                il.Emit(OpCodes.Call, ValueFromFloatMethod);
                return;
            case PdVmValueKind.Bool:
                EmitInt32(il, value.BoolValue ? 1 : 0);
                il.Emit(OpCodes.Call, ValueFromBoolMethod);
                return;
            case PdVmValueKind.String:
                il.Emit(OpCodes.Ldstr, value.AsString());
                il.Emit(OpCodes.Call, ValueFromStringMethod);
                return;
            case PdVmValueKind.Bytes:
            {
                var bytes = value.AsBytes();
                EmitInt32(il, bytes.Length);
                il.Emit(OpCodes.Newarr, typeof(byte));
                for (var index = 0; index < bytes.Length; index++)
                {
                    il.Emit(OpCodes.Dup);
                    EmitInt32(il, index);
                    EmitInt32(il, bytes[index]);
                    il.Emit(OpCodes.Stelem_I1);
                }
                il.Emit(OpCodes.Call, ValueFromBytesMethod);
                return;
            }
            default:
                throw new PdVmCompilerException($"VMBC constant kind {value.Kind} is not supported");
        }
    }

    private static void EmitThrowInvalidInstructionPointer(ILGenerator il)
    {
        il.Emit(OpCodes.Ldstr, "invalid instruction pointer");
        il.Emit(OpCodes.Newobj, InvalidOperationConstructor);
        il.Emit(OpCodes.Throw);
    }

    private static void EmitInt32(ILGenerator il, int value)
    {
        switch (value)
        {
            case -1:
                il.Emit(OpCodes.Ldc_I4_M1);
                return;
            case 0:
                il.Emit(OpCodes.Ldc_I4_0);
                return;
            case 1:
                il.Emit(OpCodes.Ldc_I4_1);
                return;
            case 2:
                il.Emit(OpCodes.Ldc_I4_2);
                return;
            case 3:
                il.Emit(OpCodes.Ldc_I4_3);
                return;
            case 4:
                il.Emit(OpCodes.Ldc_I4_4);
                return;
            case 5:
                il.Emit(OpCodes.Ldc_I4_5);
                return;
            case 6:
                il.Emit(OpCodes.Ldc_I4_6);
                return;
            case 7:
                il.Emit(OpCodes.Ldc_I4_7);
                return;
            case 8:
                il.Emit(OpCodes.Ldc_I4_8);
                return;
        }

        if (value >= sbyte.MinValue && value <= sbyte.MaxValue)
        {
            il.Emit(OpCodes.Ldc_I4_S, (sbyte)value);
            return;
        }

        il.Emit(OpCodes.Ldc_I4, value);
    }

    private static MethodInfo GetBaseMethod(string name, params Type[] parameterTypes) =>
        typeof(PdVmProgramBase).GetMethod(
            name,
            BindingFlags.Instance | BindingFlags.NonPublic | BindingFlags.Public,
            binder: null,
            types: parameterTypes,
            modifiers: null) ?? throw new InvalidOperationException($"PdVmProgramBase.{name} not found");

    private static MethodInfo GetBuiltinMethod(string name, params Type[] parameterTypes) =>
        typeof(PdVmBuiltins).GetMethod(
            name,
            BindingFlags.Static | BindingFlags.Public,
            binder: null,
            types: parameterTypes,
            modifiers: null) ?? throw new InvalidOperationException($"PdVmBuiltins.{name} not found");
}
