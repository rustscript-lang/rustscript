#[path = "../common/mod.rs"]
mod common;
use common::*;
use vm::{CompiledProgram, OpCode, TypeMap, ValueType};

#[derive(Clone, Copy)]
enum TypeMetadataExpectation<'a> {
    Exact {
        opcode: OpCode,
        expected: &'a [(ValueType, ValueType)],
    },
    LastExact {
        opcode: OpCode,
        expected: (ValueType, ValueType),
    },
    LastMissing {
        opcode: OpCode,
    },
    Present {
        opcode: OpCode,
        expected: &'a [(ValueType, ValueType)],
    },
    LocalTypesNonEmpty,
}

struct TypeInferenceCompileCase<'a> {
    name: &'a str,
    source: &'a str,
    flavor: SourceFlavor,
    metadata_expectations: &'a [TypeMetadataExpectation<'a>],
}

struct TypeInferenceRuntimeCase<'a> {
    case: RuntimeCase<'a>,
    bindings: Vec<HostBindingCase<'a>>,
    metadata_expectations: &'a [TypeMetadataExpectation<'a>],
}

fn rustscript_type_inference_compile_case<'a>(
    name: &'a str,
    source: &'a str,
    metadata_expectations: &'a [TypeMetadataExpectation<'a>],
) -> TypeInferenceCompileCase<'a> {
    TypeInferenceCompileCase {
        name,
        source,
        flavor: SourceFlavor::RustScript,
        metadata_expectations,
    }
}

fn rustscript_type_inference_runtime_case<'a>(
    name: &'a str,
    source: &'a str,
    expected_stack: Vec<Value>,
    metadata_expectations: &'a [TypeMetadataExpectation<'a>],
) -> TypeInferenceRuntimeCase<'a> {
    TypeInferenceRuntimeCase {
        case: rustscript_runtime_case(name, source, expected_stack),
        bindings: vec![],
        metadata_expectations,
    }
}

fn rustscript_bound_type_inference_runtime_case<'a>(
    name: &'a str,
    source: &'a str,
    expected_stack: Vec<Value>,
    bindings: Vec<HostBindingCase<'a>>,
    metadata_expectations: &'a [TypeMetadataExpectation<'a>],
) -> TypeInferenceRuntimeCase<'a> {
    TypeInferenceRuntimeCase {
        case: rustscript_runtime_case(name, source, expected_stack),
        bindings,
        metadata_expectations,
    }
}

fn opcode_offsets(code: &[u8], opcode: OpCode) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut ip = 0usize;
    while let Some(&raw) = code.get(ip) {
        let start = ip;
        ip += 1;
        let Ok(decoded) = OpCode::try_from(raw) else {
            break;
        };
        if decoded == opcode {
            offsets.push(start);
        }
        ip = ip.saturating_add(decoded.operand_len());
    }
    offsets
}

fn compiled_type_map(compiled: &CompiledProgram) -> &TypeMap {
    compiled
        .program
        .type_map
        .as_ref()
        .expect("compiled program should include type metadata")
}

fn assert_opcode_operand_types(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: &[(ValueType, ValueType)],
) {
    let type_map = compiled_type_map(compiled);
    let offsets = opcode_offsets(&compiled.program.code, opcode);

    assert_eq!(
        offsets.len(),
        expected.len(),
        "unexpected {opcode:?} count in bytecode"
    );
    for (offset, expected_types) in offsets.into_iter().zip(expected.iter().copied()) {
        assert_eq!(
            type_map.operand_types.get(&offset),
            Some(&expected_types),
            "unexpected operand type metadata at bytecode offset {offset}"
        );
    }
}

fn assert_last_opcode_operand_types(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: (ValueType, ValueType),
) {
    let type_map = compiled_type_map(compiled);
    let offset = *opcode_offsets(&compiled.program.code, opcode)
        .last()
        .expect("expected opcode in bytecode");
    assert_eq!(
        type_map.operand_types.get(&offset),
        Some(&expected),
        "unexpected operand type metadata at bytecode offset {offset}"
    );
}

fn assert_last_opcode_has_no_operand_types(compiled: &CompiledProgram, opcode: OpCode) {
    let type_map = compiled_type_map(compiled);
    let offset = *opcode_offsets(&compiled.program.code, opcode)
        .last()
        .expect("expected opcode in bytecode");
    assert!(
        !type_map.operand_types.contains_key(&offset),
        "expected no operand type metadata at bytecode offset {offset}"
    );
}

fn assert_opcode_operand_types_present(
    compiled: &CompiledProgram,
    opcode: OpCode,
    expected: &[(ValueType, ValueType)],
) {
    let type_map = compiled_type_map(compiled);
    let mut actual = opcode_offsets(&compiled.program.code, opcode)
        .into_iter()
        .filter_map(|offset| type_map.operand_types.get(&offset).copied())
        .collect::<Vec<_>>();

    for expected_types in expected {
        let index = actual
            .iter()
            .position(|actual_types| actual_types == expected_types)
            .expect("expected operand type metadata was not emitted");
        actual.swap_remove(index);
    }
}

fn assert_type_metadata_expectations(
    compiled: &CompiledProgram,
    case_name: &str,
    expectations: &[TypeMetadataExpectation<'_>],
) {
    for expectation in expectations {
        match expectation {
            TypeMetadataExpectation::Exact { opcode, expected } => {
                assert_opcode_operand_types(compiled, *opcode, expected);
            }
            TypeMetadataExpectation::LastExact { opcode, expected } => {
                assert_last_opcode_operand_types(compiled, *opcode, *expected);
            }
            TypeMetadataExpectation::LastMissing { opcode } => {
                assert_last_opcode_has_no_operand_types(compiled, *opcode);
            }
            TypeMetadataExpectation::Present { opcode, expected } => {
                assert_opcode_operand_types_present(compiled, *opcode, expected);
            }
            TypeMetadataExpectation::LocalTypesNonEmpty => {
                assert!(
                    !compiled_type_map(compiled).local_types.is_empty(),
                    "type metadata should include local slot entries for case '{case_name}'"
                );
            }
        }
    }
}

fn run_type_inference_compile_cases(cases: &[TypeInferenceCompileCase<'_>]) {
    for case in cases {
        let compiled = compile_source_with_flavor(case.source, case.flavor)
            .unwrap_or_else(|err| panic!("case '{}': compile should succeed: {err}", case.name));
        assert_type_metadata_expectations(&compiled, case.name, case.metadata_expectations);
    }
}

fn run_type_inference_runtime_cases(cases: &[TypeInferenceRuntimeCase<'_>]) {
    for case in cases {
        let compiled = compile_source_with_flavor(case.case.source, case.case.flavor)
            .unwrap_or_else(|err| {
                panic!("case '{}': compile should succeed: {err}", case.case.name)
            });
        if let Some(expected_locals) = case.case.expected_locals {
            assert_eq!(
                compiled.locals, expected_locals,
                "unexpected local count for case '{}'",
                case.case.name
            );
        }
        assert_type_metadata_expectations(&compiled, case.case.name, case.metadata_expectations);

        let mut vm = Vm::new(compiled.program);
        for binding in &case.bindings {
            vm.bind_function(binding.name, (binding.factory)());
        }
        let status = vm
            .run()
            .unwrap_or_else(|err| panic!("case '{}': vm should run: {err}", case.case.name));
        assert_eq!(
            status,
            VmStatus::Halted,
            "vm did not halt for case '{}'",
            case.case.name
        );
        assert_eq!(
            vm.stack(),
            case.case.expected_stack.as_slice(),
            "unexpected stack for case '{}'",
            case.case.name
        );
    }
}

#[test]
fn compiler_type_inference_runtime_cases_cover_operator_and_callable_flows() {
    let cases = vec![
        rustscript_type_inference_runtime_case(
            "known arithmetic operand types are attached to bytecode",
            r#"
                let x = 2 + 3;
                let y = 1.5 + 2.5;
                let z = "a" + "b";
                x;
                y;
                z;
            "#,
            vec![Value::Int(5), Value::Float(4.0), Value::string("ab")],
            &[
                TypeMetadataExpectation::Exact {
                    opcode: OpCode::Add,
                    expected: &[
                        (ValueType::Int, ValueType::Int),
                        (ValueType::Float, ValueType::Float),
                        (ValueType::String, ValueType::String),
                    ],
                },
                TypeMetadataExpectation::LocalTypesNonEmpty,
            ],
        ),
        rustscript_type_inference_runtime_case(
            "callable return types propagate through functions and closures",
            r#"
                fn add_one(value) {
                    value + 1;
                }

                fn apply_twice(func, value) {
                    let once = func(value);
                    func(once);
                }

                let named = add_one;
                let inc = |x| x + 1;
                let direct = add_one(40) + 1;
                let via_named_local = named(40) + 1;
                let via_closure_local = inc(40) + 1;
                let via_named_param = apply_twice(named, 40) + 1;
                let via_closure_param = apply_twice(inc, 40) + 1;
                direct;
                via_named_local;
                via_closure_local;
                via_named_param;
                via_closure_param;
            "#,
            vec![
                Value::Int(42),
                Value::Int(42),
                Value::Int(42),
                Value::Int(43),
                Value::Int(43),
            ],
            &[TypeMetadataExpectation::Exact {
                opcode: OpCode::Add,
                expected: &[
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                ],
            }],
        ),
        rustscript_type_inference_runtime_case(
            "string plus number flows stay string concat",
            r#"
                fn label(value) {
                    "v=" + value;
                }

                let number = 123;
                let formatter = |value| value + "!";
                let a = "text" + 123;
                let b = "text" + number;
                let c = label(number);
                let d = formatter(number);
                let joined = c + d;
                a;
                b;
                joined;
            "#,
            vec![
                Value::string("text123"),
                Value::string("text123"),
                Value::string("v=123123!"),
            ],
            &[TypeMetadataExpectation::Exact {
                opcode: OpCode::Add,
                expected: &[
                    (ValueType::String, ValueType::String),
                    (ValueType::String, ValueType::String),
                    (ValueType::String, ValueType::String),
                    (ValueType::String, ValueType::String),
                    (ValueType::String, ValueType::String),
                ],
            }],
        ),
        rustscript_type_inference_runtime_case(
            "named function plus operands infer from consistent calls",
            r#"
                fn addme(x) {
                    x + x
                }

                addme(21);
            "#,
            vec![Value::Int(42)],
            &[TypeMetadataExpectation::Exact {
                opcode: OpCode::Add,
                expected: &[(ValueType::Int, ValueType::Int)],
            }],
        ),
        rustscript_type_inference_runtime_case(
            "unused named functions do not force inferred plus operand metadata",
            r#"
                fn addme(x) {
                    x + x
                }

                42;
            "#,
            vec![Value::Int(42)],
            &[],
        ),
    ];

    run_type_inference_runtime_cases(&cases);
}

#[test]
fn compiler_type_inference_runtime_cases_cover_loop_and_container_flows() {
    let cases = vec![
        rustscript_type_inference_runtime_case(
            "for loop counter types stay stable after the loop",
            r#"
                let mut total = 0;
                for (let mut i = 0; i < 4; i = i + 1) {
                    total = total + i;
                }
                let after = total + 1;
                after;
            "#,
            vec![Value::Int(7)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "while loop float types stay stable after the loop",
            r#"
                let mut total = 1.5;
                let mut remaining = 2;
                while remaining > 0 {
                    total = total + 0.5;
                    remaining = remaining - 1;
                }
                let after = total + 1.0;
                after;
            "#,
            vec![Value::Float(3.5)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Float, ValueType::Float),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "outer loop types remain concrete across nested loops",
            r#"
                let mut total = 0;
                for (let mut i = 0; i < 2; i = i + 1) {
                    for (let mut j = 0; j < 2; j = j + 1) {
                        total = total + i + j;
                    }
                }
                total + 1;
            "#,
            vec![Value::Int(5)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "homogeneous container get results infer concrete element types",
            r#"
                let array = [1, 2, 3];
                let map = {"a": 4, "b": 5};
                let keys = [10, 20].keys;
                let array_value = array[0] + 3;
                let map_value = map["a"] + 2;
                let key_value = keys[0] + 1;
                array_value;
                map_value;
                key_value;
            "#,
            vec![Value::Int(4), Value::Int(6), Value::Int(1)],
            &[TypeMetadataExpectation::Exact {
                opcode: OpCode::Add,
                expected: &[
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                    (ValueType::Int, ValueType::Int),
                ],
            }],
        ),
        rustscript_type_inference_runtime_case(
            "hidden slice bindings inside function bodies infer concrete lengths",
            r#"
                fn tail_len(text) {
                    text[1:].length + 1;
                }

                tail_len("abcd");
            "#,
            vec![Value::Int(4)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "dynamic slice end bindings inside function bodies keep inference stable",
            r#"
                fn first_hex(text, i) {
                    let hex_lookup = {
                        "a": 10,
                        "b": 11
                    };
                    hex_lookup[text[i:(i + 1)]];
                }

                first_hex("ab", 0);
            "#,
            vec![Value::Int(10)],
            &[],
        ),
        rustscript_type_inference_runtime_case(
            "loop typeof remains stable after container element types widen",
            r#"
                let mut pending = [{a: 1}];
                let mut first = "";
                let mut second = "";
                let mut steps = 0;

                while (&pending).length > 0 {
                    let next_index = (&pending).length - 1;
                    let item = (&pending)[next_index].copy();
                    pending = pending[0:next_index];

                    if steps == 0 {
                        first = type(item);
                        pending[pending.length] = 1;
                    } else {
                        second = type(item);
                    }

                    steps = steps + 1;
                }

                first + second;
            "#,
            vec![Value::string("mapint")],
            &[],
        ),
    ];

    run_type_inference_runtime_cases(&cases);
}

#[test]
fn compiler_type_inference_runtime_cases_cover_schema_and_optional_flows() {
    let cases = vec![
        rustscript_type_inference_runtime_case(
            "unwrap_or on optional chain results produces concrete inner types",
            r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: 41 } };
                let score = profile?.stats?.score.unwrap_or(0);
                score + 1;
            "#,
            vec![Value::Int(42)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "non-null refinement on optional chain results narrows to concrete inner types",
            r#"
                struct Stats { score: int }
                struct Profile { stats: Stats }

                let profile: Profile = { stats: { score: 41 } };
                let score = profile?.stats?.score;
                if score != null {
                    score + 1;
                } else {
                    0;
                }
            "#,
            vec![Value::Int(42)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "Some match bindings expose the concrete inner type",
            r#"
                struct Data { values: [int] }
                let data: Data = { values: [41] };
                let result = match data?.values?.[0] {
                    None => 0,
                    Some(value) => value + 1,
                    _ => 0,
                };
                result;
            "#,
            vec![Value::Int(42)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "declared object schemas propagate through nested fields and arrays",
            r#"
                struct Age { first: int }
                struct User {
                    name: string,
                    age: Age,
                    colors: [int],
                }

                let some_map = {
                    name: "Ada",
                    age: { first: 41 },
                    colors: [7, 8]
                };
                let user: User = some_map;
                let total = user.age.first + user.colors[0];
                total;
            "#,
            vec![Value::Int(48)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_type_inference_runtime_case(
            "object literal shapes infer nested field types without an explicit alias",
            r#"
                let value = { age: { first: 10 } }.age.first + 2;
                value;
            "#,
            vec![Value::Int(12)],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
    ];

    run_type_inference_runtime_cases(&cases);
}

#[test]
fn compiler_type_inference_runtime_cases_cover_host_signature_flows() {
    let cases = vec![
        rustscript_bound_type_inference_runtime_case(
            "declared host return type signatures propagate to call sites",
            r#"
                fn add_one(x) -> int;
                let value = add_one(41);
                value + 1;
            "#,
            vec![Value::Int(43)],
            vec![HostBindingCase {
                name: "add_one",
                factory: make_add_one,
            }],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Add,
                expected: (ValueType::Int, ValueType::Int),
            }],
        ),
        rustscript_bound_type_inference_runtime_case(
            "edge host namespace signatures propagate to comparisons",
            r#"
                use runtime;
                let slept = runtime::sleep(1);
                slept == true;
            "#,
            vec![Value::Bool(true)],
            vec![HostBindingCase {
                name: "runtime::sleep",
                factory: make_runtime_sleep,
            }],
            &[TypeMetadataExpectation::LastExact {
                opcode: OpCode::Ceq,
                expected: (ValueType::Bool, ValueType::Bool),
            }],
        ),
    ];

    run_type_inference_runtime_cases(&cases);
}

#[test]
fn compiler_type_inference_compile_only_metadata_cases_work() {
    let cases = vec![
        rustscript_type_inference_compile_case(
            "unstable loop-carried types drop operand metadata after conflicts",
            r#"
                let mut value = 0;
                let values = ["x", 1];
                for (let mut i = 0; i < 2; i = i + 1) {
                    value = values[i].copy();
                }
                value + 1;
            "#,
            &[TypeMetadataExpectation::LastMissing {
                opcode: OpCode::Add,
            }],
        ),
        rustscript_type_inference_compile_case(
            "generated builtin namespaces attach declared return signatures",
            r#"
                use json;
                use re;
                use jit;
                use math;
                let encoded = json::encode({"a": 1});
                let matched = re::match("a", "a");
                let enabled = jit::set_enabled(false);
                let pi = math::pi();
                encoded + "!";
                matched == true;
                enabled == false;
                pi + 1.0;
            "#,
            &[
                TypeMetadataExpectation::Exact {
                    opcode: OpCode::Add,
                    expected: &[
                        (ValueType::String, ValueType::String),
                        (ValueType::Float, ValueType::Float),
                    ],
                },
                TypeMetadataExpectation::Present {
                    opcode: OpCode::Ceq,
                    expected: &[
                        (ValueType::Bool, ValueType::Bool),
                        (ValueType::Bool, ValueType::Bool),
                    ],
                },
            ],
        ),
    ];

    run_type_inference_compile_cases(&cases);
}

#[test]
fn compiler_type_inference_compile_rejections_work() {
    let cases = vec![
        SourceErrorCase {
            name: "mixed if else branch types are rejected",
            source: r#"
                let mut value = 1;
                if true {
                    value = 2;
                } else {
                    value = "x";
                }
                value + 1;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["int", "string"],
        },
        SourceErrorCase {
            name: "shadowed if else branch mismatches are rejected",
            source: r#"
                let total = 1;
                let total = if true => {
                    "222"
                } else => {
                    let bumped = total + 1;
                    bumped
                };
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["string", "int"],
        },
        SourceErrorCase {
            name: "loop-carried shadowed if else branch mismatches are rejected",
            source: r#"
                let mut total = 0;
                for (let mut i = 0; i < 4; i = i + 1) {
                    total = total + i;
                }

                let total = if true => {
                    "222"
                } else => {
                    let bumped = total + 1;
                    bumped
                };
                total;
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(CompileErrorKind::IfElseBranchTypeMismatch),
            expected_contains_all: &["string", "int"],
        },
        SourceErrorCase {
            name: "conflicting named function operand flows are rejected",
            source: r#"
                fn addme(x) {
                    x + x
                }

                addme(1);
                addme("as");
            "#,
            flavor: SourceFlavor::RustScript,
            expected_kind: SourceErrorKind::Compile(
                CompileErrorKind::FunctionParameterTypeConflict,
            ),
            expected_contains_all: &["addme", "int", "string"],
        },
    ];

    run_source_error_cases(&cases);
}
