use std::collections::HashMap;

use crate::builtins::BuiltinFunction;
use crate::vm::native::ROOT_FRAME_KEY;
use crate::{CallableKind, CallableTarget, OpCode, Program};

pub(crate) const MAX_INLINE_INSTRUCTIONS: usize = 32;
pub(crate) const MAX_INLINE_TOUCHED_LOCALS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum InlineRejectReason {
    NonRootCaller,
    UnknownTarget,
    #[allow(dead_code)]
    PolymorphicTarget,
    HostTarget,
    CapturedCallable,
    ArityMismatch,
    SchemaUnproven,
    Recursive,
    NestedScriptCall,
    YieldingCall,
    BackwardBranch,
    MultipleReturns,
    TooManyInstructions,
    TooManyTouchedLocals,
    TraceBudgetExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineCandidate {
    pub(crate) prototype_id: u32,
    pub(crate) entry_ip: usize,
    pub(crate) end_ip: usize,
    pub(crate) parameter_slots: Vec<u16>,
    pub(crate) touched_locals: Vec<u16>,
    pub(crate) decoded_instruction_count: usize,
}

pub(crate) fn classify_static_inline_candidate(
    program: &Program,
    caller_frame_key: u64,
    caller_prototype_id: Option<u32>,
    source_local: Option<u8>,
    argc: u8,
    remaining_trace_budget: usize,
) -> Result<InlineCandidate, InlineRejectReason> {
    if caller_frame_key != ROOT_FRAME_KEY {
        return Err(InlineRejectReason::NonRootCaller);
    }
    let source_local = source_local.ok_or(InlineRejectReason::UnknownTarget)?;
    let mut bindings = program
        .root_callable_bindings
        .iter()
        .filter(|binding| binding.local_slot == u16::from(source_local));
    let binding = bindings.next().ok_or(InlineRejectReason::UnknownTarget)?;
    if bindings.next().is_some() {
        return Err(InlineRejectReason::PolymorphicTarget);
    }
    if caller_prototype_id == Some(binding.prototype_id) {
        return Err(InlineRejectReason::Recursive);
    }
    let prototype = program
        .callable_prototypes
        .get(binding.prototype_id as usize)
        .ok_or(InlineRejectReason::UnknownTarget)?;
    if prototype.kind != CallableKind::FunctionItem
        || !prototype.capture_slots.is_empty()
        || !prototype.capture_source_slots.is_empty()
        || !prototype.capture_modes.is_empty()
    {
        return Err(InlineRejectReason::CapturedCallable);
    }
    if prototype.arity != argc
        || prototype.parameter_slots.len() != usize::from(argc)
        || prototype
            .parameter_slots
            .iter()
            .any(|slot| usize::from(*slot) >= prototype.frame_local_count)
    {
        return Err(InlineRejectReason::ArityMismatch);
    }
    let CallableTarget::ScriptFunction(function_id) = prototype.target else {
        return Err(InlineRejectReason::HostTarget);
    };
    let function = program
        .script_functions
        .get(function_id as usize)
        .ok_or(InlineRejectReason::UnknownTarget)?;
    let entry_ip = function.entry_ip as usize;
    let end_ip = function.end_ip as usize;
    if entry_ip >= end_ip || end_ip > program.code.len() {
        return Err(InlineRejectReason::UnknownTarget);
    }

    let (decoded_instruction_count, touched_locals) =
        scan_inline_region(program, entry_ip, end_ip)?;
    if decoded_instruction_count > remaining_trace_budget {
        return Err(InlineRejectReason::TraceBudgetExceeded);
    }
    Ok(InlineCandidate {
        prototype_id: binding.prototype_id,
        entry_ip,
        end_ip,
        parameter_slots: prototype.parameter_slots.clone(),
        touched_locals,
        decoded_instruction_count,
    })
}

fn scan_inline_region(
    program: &Program,
    entry_ip: usize,
    end_ip: usize,
) -> Result<(usize, Vec<u16>), InlineRejectReason> {
    let mut ip = entry_ip;
    let mut instruction_count = 0usize;
    let mut return_count = 0usize;
    let mut touched_locals = Vec::<u16>::new();

    while ip < end_ip {
        let instruction_ip = ip;
        let opcode = OpCode::try_from(
            *program
                .code
                .get(ip)
                .ok_or(InlineRejectReason::UnknownTarget)?,
        )
        .map_err(|_| InlineRejectReason::UnknownTarget)?;
        ip = ip.saturating_add(1);
        instruction_count = instruction_count.saturating_add(1);
        if instruction_count > MAX_INLINE_INSTRUCTIONS {
            return Err(InlineRejectReason::TooManyInstructions);
        }

        match opcode {
            OpCode::Ret => {
                return_count = return_count.saturating_add(1);
                if return_count > 1 || ip != end_ip {
                    return Err(InlineRejectReason::MultipleReturns);
                }
            }
            OpCode::Ldloc | OpCode::Stloc => {
                let local = *program
                    .code
                    .get(ip)
                    .ok_or(InlineRejectReason::UnknownTarget)?;
                ip = ip.saturating_add(1);
                let local = u16::from(local);
                if !touched_locals.contains(&local) {
                    touched_locals.push(local);
                    if touched_locals.len() > MAX_INLINE_TOUCHED_LOCALS {
                        return Err(InlineRejectReason::TooManyTouchedLocals);
                    }
                }
            }
            OpCode::Ldc => {
                let index =
                    read_u32(&program.code, &mut ip).ok_or(InlineRejectReason::UnknownTarget)?;
                if program.constants.get(index as usize).is_none() {
                    return Err(InlineRejectReason::UnknownTarget);
                }
            }
            OpCode::Br | OpCode::Brfalse => {
                let target = read_u32(&program.code, &mut ip)
                    .ok_or(InlineRejectReason::UnknownTarget)?
                    as usize;
                if target <= instruction_ip {
                    return Err(InlineRejectReason::BackwardBranch);
                }
                if target < entry_ip || target >= end_ip {
                    return Err(InlineRejectReason::UnknownTarget);
                }
            }
            OpCode::CallValue => return Err(InlineRejectReason::NestedScriptCall),
            OpCode::Call => {
                let index =
                    read_u16(&program.code, &mut ip).ok_or(InlineRejectReason::UnknownTarget)?;
                let argc =
                    read_u8(&program.code, &mut ip).ok_or(InlineRejectReason::UnknownTarget)?;
                let Some(builtin) = BuiltinFunction::from_call_index(index) else {
                    return Err(InlineRejectReason::YieldingCall);
                };
                if !builtin.accepts_arity(argc) {
                    return Err(InlineRejectReason::UnknownTarget);
                }
            }
            OpCode::Nop
            | OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Div
            | OpCode::Neg
            | OpCode::Ceq
            | OpCode::Clt
            | OpCode::Cgt
            | OpCode::Pop
            | OpCode::Dup
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::Mod
            | OpCode::And
            | OpCode::Or
            | OpCode::Not
            | OpCode::Lshr => {}
        }
        if ip > end_ip {
            return Err(InlineRejectReason::UnknownTarget);
        }
    }

    if return_count != 1 {
        return Err(InlineRejectReason::MultipleReturns);
    }
    touched_locals.sort_unstable();
    Ok((instruction_count, touched_locals))
}

fn read_u8(code: &[u8], ip: &mut usize) -> Option<u8> {
    let value = *code.get(*ip)?;
    *ip = ip.saturating_add(1);
    Some(value)
}

fn read_u16(code: &[u8], ip: &mut usize) -> Option<u16> {
    let bytes = code.get(*ip..ip.saturating_add(2))?;
    *ip = ip.saturating_add(2);
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32(code: &[u8], ip: &mut usize) -> Option<u32> {
    let bytes = code.get(*ip..ip.saturating_add(4))?;
    *ip = ip.saturating_add(4);
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CallSiteKey {
    pub(crate) caller_frame_key: u64,
    pub(crate) call_ip: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallSiteProfile {
    prototype_id: u32,
    observations: u64,
    mismatches: u64,
    monomorphic: bool,
}

impl CallSiteProfile {
    fn new(prototype_id: u32) -> Self {
        Self {
            prototype_id,
            observations: 1,
            mismatches: 0,
            monomorphic: true,
        }
    }

    fn observe(&mut self, prototype_id: u32) {
        self.observations = self.observations.saturating_add(1);
        if prototype_id != self.prototype_id {
            self.mismatches = self.mismatches.saturating_add(1);
            self.monomorphic = false;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitCallSiteProfile {
    pub caller_frame_key: u64,
    pub call_ip: usize,
    pub prototype_id: u32,
    pub observations: u64,
    pub mismatches: u64,
    pub monomorphic: bool,
}

pub(crate) fn observe_script_call_target(
    profiles: &mut HashMap<CallSiteKey, CallSiteProfile>,
    key: CallSiteKey,
    prototype_id: u32,
) {
    match profiles.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.get_mut().observe(prototype_id);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(CallSiteProfile::new(prototype_id));
        }
    }
}

pub(crate) fn call_site_profiles(
    profiles: &HashMap<CallSiteKey, CallSiteProfile>,
) -> Vec<JitCallSiteProfile> {
    let mut snapshot = profiles
        .iter()
        .map(|(key, profile)| JitCallSiteProfile {
            caller_frame_key: key.caller_frame_key,
            call_ip: key.call_ip,
            prototype_id: profile.prototype_id,
            observations: profile.observations,
            mismatches: profile.mismatches,
            monomorphic: profile.monomorphic,
        })
        .collect::<Vec<_>>();
    snapshot.sort_unstable_by_key(|profile| (profile.caller_frame_key, profile.call_ip));
    snapshot
}

pub(crate) fn call_site_metric_summary(
    profiles: &HashMap<CallSiteKey, CallSiteProfile>,
) -> (u64, u64, u64) {
    profiles.values().fold(
        (0u64, 0u64, 0u64),
        |(observations, monomorphic, polymorphic), profile| {
            (
                observations.saturating_add(profile.observations),
                monomorphic.saturating_add(u64::from(profile.monomorphic)),
                polymorphic.saturating_add(u64::from(!profile.monomorphic)),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallablePrototype, FunctionRegion, RootCallableBinding, ScriptFunction};

    fn function_item_program(code: Vec<u8>) -> Program {
        Program::new(Vec::new(), code.clone()).with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: 0,
                end_ip: code.len() as u32,
            }],
            vec![CallablePrototype {
                kind: CallableKind::FunctionItem,
                target: CallableTarget::ScriptFunction(0),
                arity: 2,
                frame_local_count: 2,
                parameter_slots: vec![0, 1],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![FunctionRegion {
                start_ip: 0,
                end_ip: code.len() as u32,
                prototype_id: Some(0),
            }],
            vec![RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        )
    }

    fn classify(program: &Program) -> Result<InlineCandidate, InlineRejectReason> {
        classify_static_inline_candidate(program, ROOT_FRAME_KEY, None, Some(0), 2, 64)
    }

    fn arithmetic_leaf_code() -> Vec<u8> {
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldloc as u8,
            1,
            OpCode::Add as u8,
            OpCode::Ret as u8,
        ]
    }

    #[test]
    fn inline_eligibility_accepts_short_static_arithmetic_leaf() {
        let candidate = classify(&function_item_program(arithmetic_leaf_code())).unwrap();
        assert_eq!(candidate.prototype_id, 0);
        assert_eq!(candidate.entry_ip, 0);
        assert_eq!(candidate.parameter_slots, vec![0, 1]);
        assert_eq!(candidate.touched_locals, vec![0, 1]);
        assert_eq!(candidate.decoded_instruction_count, 4);
    }

    #[test]
    fn inline_eligibility_rejects_unsupported_metadata_and_call_shapes() {
        let base = function_item_program(arithmetic_leaf_code());
        assert_eq!(
            classify_static_inline_candidate(&base, 0, None, Some(0), 2, 64),
            Err(InlineRejectReason::NonRootCaller)
        );
        assert_eq!(
            classify_static_inline_candidate(&base, ROOT_FRAME_KEY, None, None, 2, 64),
            Err(InlineRejectReason::UnknownTarget)
        );
        assert_eq!(
            classify_static_inline_candidate(&base, ROOT_FRAME_KEY, None, Some(0), 1, 64),
            Err(InlineRejectReason::ArityMismatch)
        );
        assert_eq!(
            classify_static_inline_candidate(&base, ROOT_FRAME_KEY, Some(0), Some(0), 2, 64),
            Err(InlineRejectReason::Recursive)
        );

        let mut host = base.clone();
        host.callable_prototypes[0].target = CallableTarget::HostImport(0);
        assert_eq!(classify(&host), Err(InlineRejectReason::HostTarget));

        let mut captured = base;
        captured.callable_prototypes[0].kind = CallableKind::Closure;
        captured.callable_prototypes[0].capture_slots.push(1);
        assert_eq!(
            classify(&captured),
            Err(InlineRejectReason::CapturedCallable)
        );
    }

    #[test]
    fn inline_eligibility_rejects_unsafe_regions_and_hard_limits() {
        let nested = function_item_program(vec![OpCode::CallValue as u8, 0, OpCode::Ret as u8]);
        assert_eq!(classify(&nested), Err(InlineRejectReason::NestedScriptCall));

        let yielding = function_item_program(vec![OpCode::Call as u8, 0, 0, 0, OpCode::Ret as u8]);
        assert_eq!(classify(&yielding), Err(InlineRejectReason::YieldingCall));

        let backward = function_item_program(vec![
            OpCode::Nop as u8,
            OpCode::Br as u8,
            0,
            0,
            0,
            0,
            OpCode::Ret as u8,
        ]);
        assert_eq!(classify(&backward), Err(InlineRejectReason::BackwardBranch));

        let multiple_returns = function_item_program(vec![OpCode::Ret as u8, OpCode::Ret as u8]);
        assert_eq!(
            classify(&multiple_returns),
            Err(InlineRejectReason::MultipleReturns)
        );

        let mut long = vec![OpCode::Nop as u8; MAX_INLINE_INSTRUCTIONS];
        long.push(OpCode::Ret as u8);
        assert_eq!(
            classify(&function_item_program(long)),
            Err(InlineRejectReason::TooManyInstructions)
        );

        let mut locals = Vec::new();
        for local in 0..=MAX_INLINE_TOUCHED_LOCALS as u8 {
            locals.extend([OpCode::Ldloc as u8, local, OpCode::Pop as u8]);
        }
        locals.push(OpCode::Ret as u8);
        assert_eq!(
            classify(&function_item_program(locals)),
            Err(InlineRejectReason::TooManyTouchedLocals)
        );

        let base = function_item_program(arithmetic_leaf_code());
        assert_eq!(
            classify_static_inline_candidate(&base, ROOT_FRAME_KEY, None, Some(0), 2, 3),
            Err(InlineRejectReason::TraceBudgetExceeded)
        );
    }

    #[test]
    fn inline_eligibility_uses_touched_slots_for_merged_frames() {
        let mut program = function_item_program(arithmetic_leaf_code());
        program.callable_prototypes[0].frame_local_count = 200;
        let candidate = classify(&program).unwrap();
        assert_eq!(candidate.touched_locals, vec![0, 1]);
        assert_ne!(
            program.callable_prototypes[0].frame_local_count,
            candidate.touched_locals.len()
        );
    }
}
