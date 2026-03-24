namespace PdVm.Runtime;

public abstract class PdVmProgramBase : IPdVmProgram
{
    private readonly List<PdVmValue> _stack = new();
    private readonly PdVmValue[] _locals;
    private PdVmStatus _lastStatus = PdVmStatus.Halted();
    private ulong? _pendingOpId;

    protected PdVmProgramBase(int localCount)
    {
        if (localCount < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(localCount));
        }

        _locals = Enumerable.Range(0, localCount).Select(_ => PdVmValue.Null()).ToArray();
    }

    public IReadOnlyList<PdVmValue> Stack => _stack;

    public IReadOnlyList<PdVmValue> Locals => _locals;

    public int InstructionPointer { get; private set; }

    public abstract PdVmStatus RunStep(IPdVmHost host);

    public void ResumePending(ulong opId, PdVmCallReturn returnValues)
    {
        if (_pendingOpId is null)
        {
            throw new InvalidOperationException($"program is not waiting on host op {opId}");
        }

        if (_pendingOpId.Value != opId)
        {
            throw new InvalidOperationException(
                $"program is waiting on host op {_pendingOpId.Value}, not {opId}");
        }

        _pendingOpId = null;
        PushReturn(returnValues);
    }

    protected void EnsureReadyToRunStep()
    {
        if (_pendingOpId.HasValue)
        {
            throw new InvalidOperationException(
                $"program is waiting on host op {_pendingOpId.Value}; call ResumePending first");
        }
    }

    protected void SetInstructionPointer(int instructionPointer) => InstructionPointer = instructionPointer;

    protected PdVmStatus GetLastStatus() => _lastStatus;

    protected PdVmStatus HaltProgram()
    {
        _lastStatus = PdVmStatus.Halted();
        return _lastStatus;
    }

    protected PdVmStatus YieldProgram()
    {
        _lastStatus = PdVmStatus.Yielded();
        return _lastStatus;
    }

    protected void PushValue(PdVmValue value) => _stack.Add(value);

    protected PdVmValue PopValue()
    {
        if (_stack.Count == 0)
        {
            throw new InvalidOperationException("stack underflow");
        }

        var index = _stack.Count - 1;
        var value = _stack[index];
        _stack.RemoveAt(index);
        return value;
    }

    protected bool PopBool() => PopValue().AsBool();

    protected void DiscardTop() => _ = PopValue();

    protected void DuplicateTop() => _stack.Add(PeekValue());

    protected void LoadLocalValue(byte index)
    {
        if (index >= _locals.Length)
        {
            throw new InvalidOperationException($"invalid local {index}");
        }

        _stack.Add(_locals[index]);
    }

    protected void StoreLocalValue(byte index)
    {
        if (index >= _locals.Length)
        {
            throw new InvalidOperationException($"invalid local {index}");
        }

        _locals[index] = PopValue();
    }

    protected void ApplyAdd() => ApplyBinary(PdVmOps.Add);

    protected void ApplySub() => ApplyBinary(PdVmOps.Sub);

    protected void ApplyMul() => ApplyBinary(PdVmOps.Mul);

    protected void ApplyDiv() => ApplyBinary(PdVmOps.Div);

    protected void ApplyMod() => ApplyBinary(PdVmOps.Mod);

    protected void ApplyNeg() => _stack.Add(PdVmOps.Neg(PopValue()));

    protected void ApplyNot() => _stack.Add(PdVmOps.Not(PopValue()));

    protected void ApplyEqual() => ApplyBinary(PdVmOps.Ceq);

    protected void ApplyLessThan() => ApplyBinary(PdVmOps.Clt);

    protected void ApplyGreaterThan() => ApplyBinary(PdVmOps.Cgt);

    protected void ApplyShl() => ApplyBinary(PdVmOps.Shl);

    protected void ApplyShr() => ApplyBinary(PdVmOps.Shr);

    protected void ApplyLshr() => ApplyBinary(PdVmOps.Lshr);

    protected void ApplyAnd() => ApplyBinary(PdVmOps.And);

    protected void ApplyOr() => ApplyBinary(PdVmOps.Or);

    protected bool DispatchCall(
        IPdVmHost host,
        PdVmHostImport[] imports,
        ushort callIndex,
        byte argc,
        int callIp,
        int nextIp)
    {
        var args = PopArgs(argc);
        PdVmCallOutcome outcome;
        if (PdVmBuiltins.TryGetBuiltin(callIndex, out var builtin))
        {
            if (PdVmBuiltins.GetArity(builtin) != argc)
            {
                throw new InvalidOperationException(
                    $"builtin {builtin} expects arity {PdVmBuiltins.GetArity(builtin)}, got {argc}");
            }

            outcome = PdVmBuiltins.Dispatch(callIndex, args);
        }
        else
        {
            if (PdVmBuiltins.IsBuiltinIndex(callIndex))
            {
                throw new NotSupportedException(
                    $"builtin call index 0x{callIndex:X4} is not supported by PdVm.Runtime yet");
            }

            if (callIndex >= imports.Length)
            {
                throw new InvalidOperationException($"invalid import index {callIndex}");
            }

            var import = imports[callIndex];
            if (import.Arity != argc)
            {
                throw new InvalidOperationException(
                    $"import '{import.Name}' expects arity {import.Arity}, got {argc}");
            }

            outcome = host.Call(import.Name, args);
        }

        switch (outcome.Kind)
        {
            case PdVmCallOutcomeKind.Return:
                PushReturn(outcome.ReturnValues);
                InstructionPointer = nextIp;
                return false;
            case PdVmCallOutcomeKind.Halt:
                _lastStatus = PdVmStatus.Halted();
                return true;
            case PdVmCallOutcomeKind.Yield:
                InstructionPointer = callIp;
                _lastStatus = PdVmStatus.Yielded();
                return true;
            case PdVmCallOutcomeKind.Pending:
                InstructionPointer = nextIp;
                _pendingOpId = outcome.PendingOpId;
                _lastStatus = PdVmStatus.Waiting(outcome.PendingOpId);
                return true;
            default:
                throw new InvalidOperationException($"unexpected call outcome {outcome.Kind}");
        }
    }

    private PdVmValue PeekValue()
    {
        if (_stack.Count == 0)
        {
            throw new InvalidOperationException("stack underflow");
        }

        return _stack[_stack.Count - 1];
    }

    private void ApplyBinary(Func<PdVmValue, PdVmValue, PdVmValue> operation)
    {
        var rhs = PopValue();
        var lhs = PopValue();
        _stack.Add(operation(lhs, rhs));
    }

    private PdVmValue[] PopArgs(int argc)
    {
        if (argc < 0 || argc > _stack.Count)
        {
            throw new InvalidOperationException("stack underflow");
        }

        var args = new PdVmValue[argc];
        for (var index = argc - 1; index >= 0; index--)
        {
            args[index] = PopValue();
        }

        return args;
    }

    private void PushReturn(PdVmCallReturn values)
    {
        foreach (var value in values.Values)
        {
            _stack.Add(value);
        }
    }
}
