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
    AnonymousIn(Option<CaptureOwner>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureOwner {
    Function(u16),
    RootStatement(usize),
}

/// The standalone parser registers exactly one producer; linked compilation
/// instead requires the preserved per-declaration/per-statement source map.
/// Neither variant supplies a fallback owner for linked IR.
#[derive(Clone, Copy)]
pub(super) enum CaptureProducerPolicies<'a> {
    Linked(&'a std::collections::HashMap<String, bool>),
    Standalone {
        source: &'a str,
        policies: &'a std::collections::HashMap<String, bool>,
    },
}
impl DeclarationCaptures {
    pub(super) fn retain_admitted_producers(
        &mut self,
        ir: &super::FrontendIr,
        admission: CaptureProducerPolicies<'_>,
    ) {
        self.observations.retain(|(declaration, _)| {
            let source = match declaration {
                CaptureDeclaration::Named(index)
                | CaptureDeclaration::AnonymousIn(Some(CaptureOwner::Function(index))) => {
                    ir.function_sources.get(index).map(String::as_str)
                }
                CaptureDeclaration::AnonymousIn(Some(CaptureOwner::RootStatement(index))) => ir
                    .stmt_sources
                    .get(*index)
                    .and_then(|source| source.as_deref()),
                CaptureDeclaration::AnonymousIn(None) => return false,
            };
            let (source, policies) = match admission {
                CaptureProducerPolicies::Linked(policies) => (source, policies),
                CaptureProducerPolicies::Standalone { source, policies } => {
                    (Some(source), policies)
                }
            };
            source.and_then(|source| policies.get(source)).copied() == Some(true)
        });
    }

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
    fn missing_producer_or_policy_cannot_certify_named_or_root_captures() {
        struct Dialect;
        impl crate::ParserDialect for Dialect {}
        let parsed = crate::parse_source_with_dialect(
            "let value=41; fn named()->int {value} let anonymous=||value; named(); anonymous();",
            &Dialect,
            crate::SharedParserOptions::default(),
        )
        .unwrap();
        let policies = std::collections::HashMap::from([("producer".to_owned(), true)]);
        let mut ir = parsed.clone();
        ir.stmt_sources = vec![Some("producer".to_owned()); ir.stmts.len()];
        ir.function_sources = ir
            .function_impls
            .keys()
            .map(|index| (*index, "producer".to_owned()))
            .collect();
        let infer = |ir: &crate::FrontendIr| {
            super::super::typing::infer_types_with_capture_origins(
                ir,
                super::super::TypingMode::DynamicHints,
                &[],
                &[],
            )
            .2
        };
        let mut known = infer(&ir);
        known.retain_admitted_producers(&ir, super::CaptureProducerPolicies::Linked(&policies));
        assert!(
            known
                .observations
                .iter()
                .any(|(id, _)| matches!(id, super::CaptureDeclaration::Named(_)))
        );
        assert!(
            known
                .observations
                .iter()
                .any(|(id, _)| matches!(id, super::CaptureDeclaration::AnonymousIn(_)))
        );
        let mut unknown = infer(&parsed);
        unknown
            .retain_admitted_producers(&parsed, super::CaptureProducerPolicies::Linked(&policies));
        assert!(unknown.observations.is_empty());
        for policies in [
            std::collections::HashMap::new(),
            std::collections::HashMap::from([("producer".to_owned(), false)]),
        ] {
            let mut denied = infer(&ir);
            denied
                .retain_admitted_producers(&ir, super::CaptureProducerPolicies::Linked(&policies));
            assert!(denied.observations.is_empty());
        }
        ir.function_sources.clear();
        let mut root_only = infer(&ir);
        root_only.retain_admitted_producers(&ir, super::CaptureProducerPolicies::Linked(&policies));
        assert!(!root_only.observations.is_empty());
        assert!(root_only.observations.iter().all(|(id, _)| matches!(
            id,
            super::CaptureDeclaration::AnonymousIn(Some(super::CaptureOwner::RootStatement(_)))
        )));
        ir.stmt_sources.clear();
        let mut absent = infer(&ir);
        absent.retain_admitted_producers(&ir, super::CaptureProducerPolicies::Linked(&policies));
        assert!(absent.observations.is_empty());
    }

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
