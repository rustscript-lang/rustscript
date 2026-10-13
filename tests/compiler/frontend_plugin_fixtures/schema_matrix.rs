use super::*;
use std::collections::HashMap;

struct MatrixPlugin;
static MATRIX: MatrixPlugin = MatrixPlugin;
fn callable() -> TypeSchema {
    TypeSchema::Callable {
        params: vec![TypeSchema::Int],
        result: Box::new(TypeSchema::Int),
    }
}
fn expected() -> HashMap<&'static str, TypeSchema> {
    let optional = TypeSchema::Optional(Box::new(TypeSchema::Int));
    HashMap::from([
        ("mapper", callable()),
        ("specialized", callable()),
        (
            "nested",
            TypeSchema::Map(Box::new(TypeSchema::Array(Box::new(optional.clone())))),
        ),
        ("tuple", TypeSchema::ArrayTuple(vec![optional, callable()])),
        (
            "bundle",
            TypeSchema::Named("Bundle".to_string(), vec![TypeSchema::Int]),
        ),
        (
            "object",
            TypeSchema::Object(HashMap::from([("callback".to_string(), callable())])),
        ),
    ])
}
impl SourcePlugin for MatrixPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-schema-matrix"),
            extensions: &["matrix"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_source_with_dialect(
            source,
            &SYNTHETIC_DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                import_scan_mode: true,
                ..SharedParserOptions::default()
            },
        )
    }
    fn validate_typed_ir(&self, typed: &FrontendTypedIr<'_>) -> Result<(), CompileError> {
        for (name, schema) in expected() {
            let (_, slot) = typed
                .ir
                .local_bindings
                .iter()
                .find(|(label, _)| label == name)
                .unwrap();
            let actual = typed.schema_of_expr(&vm::Expr::Var(*slot)).unwrap();
            assert_eq!(
                typed.ir.source_schema(&actual, 0),
                schema,
                "typed query {name}"
            );
            assert_eq!(typed.local_schemas[usize::from(*slot)], Some(actual));
            if name == "mapper" || name == "specialized" {
                let initializer = typed
                    .ir
                    .stmts
                    .iter()
                    .find_map(|stmt| match stmt {
                        Stmt::Let { index, expr, .. } if index == slot => Some(expr),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    typed.schema_of_expr(initializer),
                    Some(schema),
                    "initializer query {name}"
                );
            }
        }
        Ok(())
    }
}
#[test]
fn contextual_generic_and_recursive_schemas_agree_across_query_hint_semantic_prototype_and_runtime()
{
    let source = "// 中文\r\nstruct Bundle<T> { items: [T?], callback: fn(T) -> T }\r\nfn identity<T>(value: T) -> T { value }\r\nlet mapper: fn(int) -> int = |parameter| parameter + 1;\r\nlet specialized = identity::<int>;\r\nlet nested: map<[int?]> = { \"one\": [null, 41] };\r\nlet tuple: [int?, fn(int) -> int] = [null, specialized];\r\nlet bundle: Bundle<int> = { items: [null, 41], callback: specialized };\r\nlet object = { callback: specialized };\r\nmapper(specialized(41));\r\n";
    let options = || CompileSourceFileOptions::new().with_source_plugin(&MATRIX);
    let flavor = SourceFlavor::Plugin(FrontendId::new("neutral-schema-matrix"));
    let compiled = compile_source_with_flavor_and_options(source, flavor, options()).unwrap();
    let schemas: Vec<_> = compiled
        .program
        .callable_prototypes
        .iter()
        .filter_map(|prototype| prototype.schema.as_ref())
        .collect();
    assert!(
        schemas
            .iter()
            .filter(|schema| ***schema == callable())
            .count()
            >= 2,
        "contextual closure and specialized generic prototypes carry executable signatures: {schemas:?}"
    );
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    let model = analyze_source_from_string_with_options("main.matrix", source, options()).unwrap();
    let hints = collect_inferred_local_type_hints_with_options(source, flavor, options()).unwrap();
    for (name, schema) in expected() {
        let hint = named_hint(&hints, name);
        let actual = hint.schema.as_ref().unwrap();
        assert_eq!(model.ir().source_schema(actual, 0), schema, "hint {name}");
        let span = hint.span.unwrap();
        assert_eq!(&source[span.lo..span.hi], name);
        let semantic = model
            .inferred_schema_at(vm::SourcePosition::new(0, span.lo))
            .unwrap();
        assert_eq!(semantic, *actual, "semantic hover {name}");
    }
    let parameter = named_hint(&hints, "parameter");
    assert_eq!(parameter.schema, Some(TypeSchema::Int));
    let span = parameter.span.unwrap();
    assert_eq!(&source[span.lo..span.hi], "parameter");
    let invalid = source.replace("specialized(41)", "specialized(\"wrong\")");
    let errors = [
        compile_source_with_flavor_and_options(&invalid, flavor, options())
            .err()
            .unwrap(),
        analyze_source_from_string_with_options("main.matrix", &invalid, options())
            .err()
            .unwrap(),
        collect_inferred_local_type_hints_with_options(&invalid, flavor, options())
            .err()
            .unwrap(),
    ];
    for error in &errors {
        let diagnostic = error.to_string();
        assert!(
            diagnostic.contains("function 'identity' argument 'value' type mismatch"),
            "{error}"
        );
        assert!(
            diagnostic.contains("schema type 'int'") && diagnostic.contains("string"),
            "{error}"
        );
    }
    assert_eq!(errors[0].to_string(), errors[1].to_string());
    assert_eq!(errors[0].to_string(), errors[2].to_string());
}
