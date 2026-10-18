//! Invocation-local ownership projection; never a resource authority or registry.
use std::collections::HashMap;
use std::sync::Arc;

use crate::bytecode::{CallableEnvironment, SharedCaptureCell};
use crate::compiler::TypeSchema;
use crate::result::PendingScriptResource;
use crate::{ResourceHandle, Value, Vm, VmError, VmResult};

const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 64;
const DISCARDED: u8 = 1;
const SURVIVING: u8 = 2;

#[cfg(test)]
mod quarantine_tests;
#[cfg(test)]
mod tests;

#[derive(Clone)]
enum Hold {
    Value(Value),
    Environment(Arc<CallableEnvironment>, u32),
    Cell(SharedCaptureCell),
}
impl Hold {
    fn identity(&self) -> Option<(u8, usize)> {
        Some(match self {
            Self::Value(Value::Array(v)) => (0, Arc::as_ptr(v) as usize),
            Self::Value(Value::Map(v)) => (1, Arc::as_ptr(v) as usize),
            Self::Value(Value::Callable(v)) => (2, Arc::as_ptr(v) as usize),
            Self::Environment(v, _) => (3, Arc::as_ptr(v) as usize),
            Self::Cell(v) => (4, Arc::as_ptr(v) as usize),
            _ => return None,
        })
    }
    fn strong_count(&self) -> usize {
        match self {
            Self::Value(Value::Array(v)) => Arc::strong_count(v),
            Self::Value(Value::Map(v)) => Arc::strong_count(v),
            Self::Value(Value::Callable(v)) => Arc::strong_count(v),
            Self::Environment(v, _) => Arc::strong_count(v),
            Self::Cell(v) => Arc::strong_count(v),
            _ => 0,
        }
    }
}
struct Node {
    hold: Hold,
    incoming: usize,
    roots: u8,
    children: Vec<usize>,
    handle: Option<ResourceHandle>,
}
struct Graph<'a> {
    vm: &'a Vm,
    journal: &'a [PendingScriptResource],
    nodes: Vec<Node>,
    identities: HashMap<(u8, usize), usize>,
    edges: usize,
}
impl Graph<'_> {
    fn add(
        &mut self,
        hold: Hold,
        schema: Option<&TypeSchema>,
        incoming: bool,
        roots: u8,
        depth: usize,
    ) -> VmResult<usize> {
        self.edges += 1;
        if self.edges > MAX_NODES || depth > MAX_DEPTH {
            return Err(VmError::InvalidFrameState("callback ownership graph limit"));
        }
        let identity = hold.identity();
        if let Some(index) = identity.and_then(|id| self.identities.get(&id).copied()) {
            let node = &mut self.nodes[index];
            node.incoming += usize::from(incoming);
            node.roots |= roots;
            return Ok(index);
        }
        let mut owned_schema = schema.cloned();
        for _ in 0..MAX_DEPTH {
            owned_schema = match owned_schema {
                Some(TypeSchema::Optional(inner)) => Some(*inner),
                Some(TypeSchema::Named(ref name, ref args)) => super::super::NamedStructLookup {
                    host: &self.vm.host.named_struct_schemas,
                    guest: &self.vm.program.named_struct_decls,
                }
                .body(name, args)
                .map(|(body, _)| body),
                other => {
                    owned_schema = other;
                    break;
                }
            };
        }
        let schema = owned_schema.as_ref();
        let handle = match &hold {
            Hold::Value(Value::Int(raw)) => self
                .journal
                .iter()
                .find(|entry| {
                    // Compare only this invocation's typed publications. An Int
                    // schema is data, even when its bits equal a journaled handle.
                    entry.handle.raw() == *raw as u64
                        && match schema {
                            Some(TypeSchema::Resource(key)) => key.as_str() == entry.key,
                            None | Some(TypeSchema::Unknown | TypeSchema::GenericParam(_)) => true,
                            _ => false,
                        }
                })
                .map(|entry| entry.handle),
            _ => None,
        };
        let index = self.nodes.len();
        self.nodes.push(Node {
            hold,
            incoming: usize::from(incoming),
            roots,
            children: Vec::new(),
            handle,
        });
        if let Some(id) = identity {
            self.identities.insert(id, index);
        }
        // Snapshot holds are counted separately from actual graph edges. These
        // temporary expansion clones are gone before external-root detection.
        let held = self.nodes[index].hold.clone();
        let mut children = Vec::new();
        match held {
            Hold::Value(Value::Array(values)) => {
                for (ordinal, value) in values.iter().enumerate() {
                    let child_schema = match schema {
                        Some(TypeSchema::Array(inner)) => Some(inner.as_ref()),
                        Some(TypeSchema::ArrayTuple(items)) => items.get(ordinal),
                        Some(TypeSchema::ArrayTupleRest { prefix, rest }) => {
                            Some(prefix.get(ordinal).unwrap_or(rest))
                        }
                        _ => None,
                    };
                    children.push(self.add(
                        Hold::Value(value.clone()),
                        child_schema,
                        true,
                        0,
                        depth + 1,
                    )?);
                }
            }
            Hold::Value(Value::Map(values)) => {
                for (key, value) in values.iter() {
                    let child_schema = match (schema, key) {
                        (Some(TypeSchema::Result { ok, err }), Value::String(name))
                            if name.as_str() == "payload" =>
                        {
                            match values.get(&Value::string("tag")) {
                                Some(Value::Bool(true)) => Some(ok.as_ref()),
                                Some(Value::Bool(false)) => Some(err.as_ref()),
                                _ => None,
                            }
                        }
                        (Some(TypeSchema::Object(fields)), Value::String(name)) => {
                            fields.get(name.as_str())
                        }
                        (Some(TypeSchema::Map(inner)), _) => Some(inner.as_ref()),
                        _ => None,
                    };
                    children.push(self.add(Hold::Value(key.clone()), None, true, 0, depth + 1)?);
                    children.push(self.add(
                        Hold::Value(value.clone()),
                        child_schema,
                        true,
                        0,
                        depth + 1,
                    )?);
                }
            }
            Hold::Value(Value::Callable(callable)) => {
                if let Some(env) = &callable.env {
                    children.push(self.add(
                        Hold::Environment(env.clone(), callable.prototype_id),
                        None,
                        true,
                        0,
                        depth + 1,
                    )?);
                }
            }
            Hold::Environment(env, prototype_id) => {
                let cells = env
                    .cells
                    .lock()
                    .map_err(|_| VmError::InvalidFrameState("callback capture lock is poisoned"))?;
                for (ordinal, cell) in cells.iter().enumerate() {
                    let schema = self
                        .vm
                        .program
                        .callable_prototypes
                        .get(prototype_id as usize)
                        .and_then(|prototype| prototype.capture_source_slots.get(ordinal))
                        .and_then(|slot| {
                            self.vm
                                .program
                                .type_map
                                .as_ref()?
                                .local_schemas
                                .get(usize::from(*slot))
                        })
                        .and_then(Option::as_ref);
                    children.push(self.add(
                        Hold::Cell(cell.clone()),
                        schema,
                        true,
                        0,
                        depth + 1,
                    )?);
                }
            }
            Hold::Cell(cell) => {
                let value = cell
                    .lock()
                    .map_err(|_| VmError::InvalidFrameState("callback capture lock is poisoned"))?;
                children.push(self.add(Hold::Value(value.clone()), schema, true, 0, depth + 1)?);
            }
            _ => {}
        }
        self.nodes[index].children = children;
        Ok(index)
    }
    fn value(&mut self, value: &Value, schema: Option<&TypeSchema>, roots: u8) -> VmResult<()> {
        self.add(Hold::Value(value.clone()), schema, true, roots, 0)
            .map(|_| ())
    }
    fn abandoned(mut self) -> Vec<ResourceHandle> {
        let mut pending = Vec::new();
        for (index, node) in self.nodes.iter_mut().enumerate() {
            // A live Arc outside the projected VM graph is a surviving owner:
            // e.g. a host-retained callback, container, environment or cell.
            // Weak identity records are observations, never owning roots.
            if node.hold.strong_count() > node.incoming + 1 {
                node.roots |= SURVIVING;
            }
            if node.roots != 0 {
                pending.push((index, node.roots));
            }
        }
        let mut seen = vec![0u8; self.nodes.len()];
        while let Some((index, roots)) = pending.pop() {
            let new = roots & !seen[index];
            if new == 0 {
                continue;
            }
            seen[index] |= new;
            pending.extend(self.nodes[index].children.iter().map(|child| (*child, new)));
        }
        let protected = self
            .nodes
            .iter()
            .zip(seen)
            .filter_map(|(node, roots)| (roots & SURVIVING != 0).then_some(node.handle).flatten())
            .collect::<std::collections::HashSet<_>>();
        // The retained journal is itself the invocation's typed owning root.
        // A carrier already dropped inside the failed body is abandoned too;
        // absence from the disposable Values must not erase that ownership.
        self.journal
            .iter()
            .filter_map(|entry| (!protected.contains(&entry.handle)).then_some(entry.handle))
            .collect()
    }
}

/// Reuse the admitted callable's authoritative schema for deferred arguments.
fn parameters<'a>(vm: &'a Vm, value: &Value) -> Option<&'a [TypeSchema]> {
    let Value::Callable(callable) = value else {
        return None;
    };
    match vm
        .program
        .callable_prototypes
        .get(callable.prototype_id as usize)?
        .schema
        .as_ref()?
    {
        TypeSchema::Callable { params, .. } => Some(params),
        _ => None,
    }
}

/// Project the exact roots *before* ordinary abort drains them. Only journaled
/// handles can be returned, and their monomorphized close revalidates the live
/// type/key/arena/generation afterward. Opaque native state must take a resource
/// to acquire Rust ownership; retaining a naked encoded integer is not a take.
/// Shared heap values retained by native state are detected by actual Arc edges.
pub(super) fn abandoned(
    vm: &Vm,
    journal: &[PendingScriptResource],
    stack_base: usize,
    local_base: usize,
    frame_count: usize,
) -> VmResult<Vec<ResourceHandle>> {
    let mut graph = Graph {
        vm,
        journal,
        nodes: Vec::new(),
        identities: HashMap::new(),
        edges: 0,
    };
    let local_base = vm
        .instance
        .execution_frames
        .get(frame_count)
        .map_or(local_base, |frame| local_base.min(frame.local_base));
    for (index, value) in vm.instance.locals.iter().enumerate() {
        let schema = if index < local_base {
            vm.program
                .type_map
                .as_ref()
                .and_then(|map| map.local_schemas.get(index))
                .and_then(Option::as_ref)
        } else {
            // Parameter schemas are authoritative; ordinary temporaries and
            // operands have no frame-relative schema metadata in this ABI.
            vm.instance.execution_frames.iter().rev().find_map(|frame| {
                let prototype = vm
                    .program
                    .callable_prototypes
                    .get(frame.prototype_id? as usize)?;
                let ordinal = prototype
                    .parameter_slots
                    .iter()
                    .position(|slot| frame.local_base + usize::from(*slot) == index)?;
                match prototype.schema.as_ref()? {
                    TypeSchema::Callable { params, .. } => params.get(ordinal),
                    _ => None,
                }
            })
        };
        graph.value(
            value,
            schema,
            if index < local_base {
                SURVIVING
            } else {
                DISCARDED
            },
        )?;
    }
    for (index, value) in vm.instance.stack.iter().enumerate() {
        graph.value(
            value,
            None,
            if index < stack_base {
                SURVIVING
            } else {
                DISCARDED
            },
        )?;
    }
    for value in vm
        .program
        .constants
        .iter()
        .chain(vm.instance.host_return.iter())
        .chain(vm.instance.completed_callable_results.iter())
    {
        graph.value(value, None, SURVIVING)?;
    }
    for queued in &vm.instance.queued_callables {
        graph.value(&queued.callable, None, SURVIVING)?;
        let schemas = parameters(vm, &queued.callable);
        for (index, value) in queued.args.iter().enumerate() {
            graph.value(
                value,
                schemas.and_then(|schemas| schemas.get(index)),
                SURVIVING,
            )?;
        }
    }
    if let Some(state) = &vm.instance.invocation {
        use super::super::invocation::InvocationPhase;
        if let InvocationPhase::EventPending(value) | InvocationPhase::CompletePending(value) =
            &state.phase
        {
            graph.value(value, None, SURVIVING)?;
        }
    }
    if let Some(stream) = &vm.instance.host_stream {
        graph.value(&stream.callback, None, SURVIVING)?;
        if let Some(value) = &stream.open_callback {
            graph.value(value, None, SURVIVING)?;
        }
        let callback = match stream.item_callback {
            super::super::async_host::HostStreamCallback::Event => Some(&stream.callback),
            super::super::async_host::HostStreamCallback::Open => stream.open_callback.as_ref(),
        };
        let schemas = callback.and_then(|value| parameters(vm, value));
        for (index, value) in stream.item.iter().flatten().enumerate() {
            graph.value(
                value,
                schemas.and_then(|schemas| schemas.get(index)),
                SURVIVING,
            )?;
        }
    }
    // Include externally held callbacks even if no VM Value points at them.
    // Upgrade adds only an observation hold, not an incoming ownership edge.
    for weak in &vm.instance.owned_callables {
        if let Some(callable) = weak.upgrade() {
            graph.add(Hold::Value(Value::Callable(callable)), None, false, 0, 0)?;
        }
    }
    for (index, cell) in &vm.instance.capture_cells {
        graph.add(
            Hold::Cell(cell.clone()),
            None,
            true,
            if *index < local_base {
                SURVIVING
            } else {
                DISCARDED
            },
            0,
        )?;
    }
    let retained_iterators = vm
        .instance
        .execution_frames
        .iter()
        .take(frame_count)
        .filter(|frame| frame.prototype_id.is_some())
        .count()
        .saturating_add(1);
    for (depth, iterators) in vm.instance.map_iterators.iter().enumerate() {
        for iterator in iterators.iter().flatten() {
            let roots = if depth < retained_iterators {
                SURVIVING
            } else {
                DISCARDED
            };
            let (map, key, value) = iterator.publication_roots();
            graph.value(&Value::Map(map.clone()), None, roots)?;
            for value in key.into_iter().chain(value) {
                graph.value(value, None, roots)?;
            }
        }
    }
    Ok(graph.abandoned())
}
