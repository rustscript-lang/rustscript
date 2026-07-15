use crate::bytecode::Value;
use crate::vm::{CallReturn, Vm, VmError, VmResult};

use super::{arg, return_one};

fn iterator_slot(args: &[Value], index: usize, label: &'static str) -> VmResult<usize> {
    let raw = arg::<i64>(args, index, label)?;
    usize::try_from(raw).map_err(|_| VmError::HostError(format!("invalid map iterator slot {raw}")))
}

pub(super) fn init(vm: &mut Vm, args: &[Value]) -> VmResult<CallReturn> {
    let map = match args.first() {
        Some(Value::Map(map)) => map.clone(),
        _ => return Err(VmError::TypeMismatch("map")),
    };
    let slot = iterator_slot(args, 1, "map iterator slot")?;
    if map.iter().any(|(key, _)| !matches!(key, Value::String(_))) {
        return Err(VmError::HostError(
            "borrowed map iteration requires string keys".to_string(),
        ));
    }
    vm.init_map_iterator(slot, map.clone())?;
    Ok(return_one(Value::Map(map)))
}

pub(super) fn next(vm: &mut Vm, args: &[Value]) -> VmResult<CallReturn> {
    let slot = iterator_slot(args, 0, "map iterator slot")?;
    vm.advance_map_iterator(slot).map(return_one)
}

pub(super) fn take_key(vm: &mut Vm, args: &[Value]) -> VmResult<CallReturn> {
    let slot = iterator_slot(args, 0, "map iterator slot")?;
    vm.take_map_iterator_key(slot).map(return_one)
}

pub(super) fn take_value(vm: &mut Vm, args: &[Value]) -> VmResult<CallReturn> {
    let slot = iterator_slot(args, 0, "map iterator slot")?;
    vm.take_map_iterator_value(slot).map(return_one)
}

pub(super) fn close(vm: &mut Vm, args: &[Value]) -> VmResult<CallReturn> {
    let map = match args.first() {
        Some(Value::Map(map)) => map.clone(),
        _ => return Err(VmError::TypeMismatch("map")),
    };
    let slot = iterator_slot(args, 1, "map iterator slot")?;
    vm.close_map_iterator(slot)?;
    Ok(return_one(Value::Map(map)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program};

    #[test]
    fn init_accepts_compaction_independent_ids_and_rejects_oversized_ids() {
        let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]).with_local_count(1);
        let mut vm = Vm::new(program);

        init(&mut vm, &[Value::map(Vec::new()), Value::Int(2)])
            .expect("logical iterator ids must not depend on compacted local count");
        let err = init(
            &mut vm,
            &[Value::map(Vec::new()), Value::Int(i64::from(u8::MAX) + 1)],
        )
        .expect_err("oversized iterator id should fail without allocating");
        assert!(err.to_string().contains("invalid map iterator id"));
    }
}
