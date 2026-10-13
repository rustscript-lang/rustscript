//! Compiler-private declaration observations; never serialized with Program.
use super::{CompiledProgram, LocalSlot, TypeSchema};
use crate::{CallablePrototype, CaptureBindingMode};

/// An immutable compiler-issued pair. Extracting the ordinary compilation
/// deliberately discards all capture authority.
pub struct CertifiedCompilation {
    pub(super) compiled: CompiledProgram,
    pub(super) facts: CaptureCertification,
}
impl CertifiedCompilation {
    pub fn into_uncertified(self) -> CompiledProgram {
        let Self { compiled, facts } = self;
        drop(facts);
        compiled
    }
    #[cfg(feature = "runtime")]
    pub fn into_vm(self) -> crate::vm::VmResult<crate::vm::Vm> {
        let mut vm = self.compiled.into_vm()?;
        vm.attach_capture_certification(self.facts);
        Ok(vm)
    }
}

#[derive(Clone, Default)]
pub(crate) struct CaptureCertification {
    pub(crate) prototypes: Vec<Vec<CaptureFact>>,
    pub(crate) creation_sites: Vec<(usize, u32)>,
}
#[derive(Clone)]
pub(crate) struct CaptureFact {
    pub(crate) source: LocalSlot,
    pub(crate) target: LocalSlot,
    pub(crate) mode: CaptureBindingMode,
    pub(crate) schema: Option<TypeSchema>,
}
#[derive(Clone, Default)]
pub(super) struct DeclarationCaptures {
    // Capture layouts refer to logical slots until allocation remaps them.
    observations: Vec<(CaptureDeclaration, Vec<CaptureFact>)>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureDeclaration {
    Named(u16),
    AnonymousIn(Option<u16>),
}
impl DeclarationCaptures {
    pub(super) fn observe(&mut self, declaration: CaptureDeclaration, facts: Vec<CaptureFact>) {
        if facts.is_empty() {
            return;
        }
        if let Some((_, existing)) = self.observations.iter_mut().find(|(id, captures)| {
            *id == declaration
                && captures.len() == facts.len()
                && captures
                    .iter()
                    .zip(&facts)
                    .all(|(a, b)| a.source == b.source && a.target == b.target && a.mode == b.mode)
        }) {
            for (old, new) in existing.iter_mut().zip(facts) {
                if old.schema != new.schema {
                    old.schema = None;
                }
            }
        } else {
            self.observations.push((declaration, facts));
        }
    }
    pub(super) fn extend(&mut self, other: Self) {
        for (id, captures) in other.observations {
            self.observe(id, captures);
        }
    }
    pub(super) fn remap(&mut self, mapping: &[LocalSlot]) {
        for (_, captures) in &mut self.observations {
            for capture in captures {
                capture.source = mapping[capture.source as usize];
                capture.target = mapping[capture.target as usize];
            }
        }
    }
    pub(super) fn for_prototype(
        &self,
        declaration: CaptureDeclaration,
        prototype: &CallablePrototype,
    ) -> Vec<CaptureFact> {
        let mut result: Option<Vec<CaptureFact>> = None;
        for (id, captures) in &self.observations {
            if *id != declaration
                || captures.len() != prototype.capture_slots.len()
                || captures.iter().enumerate().any(|(i, c)| {
                    prototype.capture_source_slots.get(i) != Some(&c.source)
                        || prototype.capture_slots.get(i) != Some(&c.target)
                        || prototype.capture_modes.get(i) != Some(&c.mode)
                })
            {
                continue;
            }
            match &mut result {
                None => result = Some(captures.clone()),
                Some(existing) => {
                    for (old, new) in existing.iter_mut().zip(captures) {
                        if old.schema != new.schema {
                            old.schema = None;
                        }
                    }
                }
            }
        }
        result.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ordinary_conversion_preserves_source_program_and_errors() {
        let source = "let value = 41; value + 1;";
        let ordinary = crate::compile_source(source).unwrap();
        let issued = crate::compile_source_with_capture_certification(source)
            .unwrap()
            .into_uncertified();
        assert_eq!(ordinary.program.code, issued.program.code);
        assert_eq!(ordinary.program.constants, issued.program.constants);
        assert_eq!(ordinary.locals, issued.locals);
        let old = crate::compile_source("let =;").err().unwrap().to_string();
        let new = crate::compile_source_with_capture_certification("let =;")
            .err()
            .unwrap()
            .to_string();
        assert_eq!(old, new);
    }
}
