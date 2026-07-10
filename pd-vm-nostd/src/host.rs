use alloc::vec::Vec;

use super::{Program, Value, VmError, VmResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostError {
    message: &'static str,
}

impl HostError {
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }

    pub const fn message(self) -> &'static str {
        self.message
    }
}

pub type HostFunction<C> = fn(&mut C, &[Value]) -> Result<Option<Value>, HostError>;
pub type HostDispatcher<C> = fn(&mut C, &str, &[Value]) -> Result<Option<Value>, HostError>;

pub struct HostBinding<C> {
    name: &'static str,
    arity: u8,
    function: HostFunction<C>,
}

impl<C> HostBinding<C> {
    pub const fn new(name: &'static str, arity: u8, function: HostFunction<C>) -> Self {
        Self {
            name,
            arity,
            function,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn arity(&self) -> u8 {
        self.arity
    }
}

pub(crate) fn resolve_host_functions<C>(
    program: &Program,
    bindings: &[HostBinding<C>],
) -> VmResult<Vec<HostFunction<C>>> {
    let mut resolved = Vec::new();
    resolved
        .try_reserve_exact(program.imports().len())
        .map_err(|_| VmError::HostBindingCapacity)?;

    for import in program.imports() {
        let binding = bindings
            .iter()
            .find(|binding| binding.name == import.name)
            .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?;
        if binding.arity != import.arity {
            return Err(VmError::InvalidCallArity {
                import: import.name.clone(),
                expected: binding.arity,
                got: import.arity,
            });
        }
        resolved.push(binding.function);
    }
    Ok(resolved)
}
