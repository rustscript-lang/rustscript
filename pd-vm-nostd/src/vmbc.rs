use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::{
    CallableKind, CallablePrototype, CallableTarget, CaptureBindingMode, ExportedCallable,
    FunctionRegion, HostImport, MAX_FRAME_LOCAL_COUNT, OpCode, Program, RootCallableBinding,
    ScriptFunction, Value, ValueType, WireError,
};

const MAGIC: [u8; 4] = *b"VMBC";
const VERSION_V13: u16 = 13;
const FLAGS: u16 = 0;
const MAX_WIRE_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_WIRE_BLOB_BYTES: usize = 16 * 1024 * 1024;
const MAX_WIRE_COUNT: usize = 1_000_000;
const MAX_WIRE_AGGREGATE_ITEMS: usize = 1_000_000;
const MAX_RESOURCE_KEY_LEN: usize = 128;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_CONSTANT_DEPTH: usize = 64;

fn read_constant(cursor: &mut Cursor<'_>, depth: usize) -> Result<Value, WireError> {
    if depth >= MAX_CONSTANT_DEPTH {
        return Err(WireError::LengthTooLarge("constant nesting depth", depth));
    }
    match cursor.read_u8()? {
        0 => Ok(Value::Int(cursor.read_i64()?)),
        1 => Ok(Value::Bool(cursor.read_bool()?)),
        2 => Ok(Value::string(cursor.read_string()?)),
        3 => Ok(Value::Float(cursor.read_f64()?)),
        4 => Ok(Value::Null),
        5 => {
            let bytes = cursor.read_blob("constant bytes")?;
            let mut owned = Vec::new();
            reserve(&mut owned, "constant bytes", bytes.len())?;
            owned.extend_from_slice(bytes);
            Ok(Value::bytes(owned))
        }
        6 => {
            let count = cursor.read_count("constant array", 1)?;
            let mut values = Vec::new();
            reserve(&mut values, "constant array", count)?;
            for _ in 0..count {
                values.push(read_constant(cursor, depth + 1)?);
            }
            Ok(Value::array(values))
        }
        7 => {
            let count = cursor.read_count("constant map", 2)?;
            let mut entries = Vec::new();
            reserve(&mut entries, "constant map", count)?;
            for _ in 0..count {
                entries.push((
                    read_constant(cursor, depth + 1)?,
                    read_constant(cursor, depth + 1)?,
                ));
            }
            Ok(Value::map(entries))
        }
        tag => Err(WireError::InvalidConstantTag(tag)),
    }
}

pub fn decode_program(bytes: &[u8]) -> Result<Program, WireError> {
    if bytes.len() > MAX_WIRE_PAYLOAD_BYTES {
        return Err(WireError::LengthTooLarge("payload", bytes.len()));
    }
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.read_array::<4>()?;
    if magic != MAGIC {
        return Err(WireError::InvalidMagic(magic));
    }

    let version = cursor.read_u16()?;
    if version != VERSION_V13 {
        return Err(WireError::UnsupportedVersion(version));
    }
    let flags = cursor.read_u16()?;
    if flags != FLAGS {
        return Err(WireError::UnsupportedFlags(flags));
    }

    let constant_count = cursor.read_count("constants", 1)?;
    let mut constants = Vec::new();
    reserve(&mut constants, "constants", constant_count)?;
    for _ in 0..constant_count {
        constants.push(read_constant(&mut cursor, 0)?);
    }

    let code_bytes = cursor.read_blob("code")?;
    let mut code = Vec::new();
    reserve(&mut code, "code", code_bytes.len())?;
    code.extend_from_slice(code_bytes);
    let import_count = cursor.read_count("imports", 7)?;
    let mut imports = Vec::new();
    reserve(&mut imports, "imports", import_count)?;
    for _ in 0..import_count {
        imports.push(HostImport {
            name: cursor.read_string()?,
            arity: cursor.read_u8()?,
            return_type: read_value_type(cursor.read_u8()?)?,
        });
        match cursor.read_u8()? {
            0 => {}
            1 => skip_host_import_schema(&mut cursor)?,
            value => return Err(WireError::InvalidBool(value)),
        }
    }

    let encoded_local_count = skip_type_map(&mut cursor)?;
    skip_debug_info(&mut cursor)?;
    let (
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ) = read_callable_metadata(&mut cursor)?;
    let named_decls = read_named_cleanup_decls(&mut cursor)?;
    read_ret_cleanups(
        &mut cursor,
        &code,
        &callable_prototypes,
        &function_regions,
        &named_decls,
    )?;
    if !cursor.is_empty() {
        return Err(WireError::TrailingBytes);
    }
    validate_call_script_operands(&code, &callable_prototypes)?;

    let program = Program::new(constants, code, imports);
    let program = match encoded_local_count {
        Some(local_count) => program.with_local_count(local_count),
        None => program,
    };
    Ok(program.with_callable_metadata(
        script_functions,
        callable_prototypes,
        function_regions,
        root_callable_bindings,
        exported_callables,
    ))
}

fn reserve<T>(items: &mut Vec<T>, field: &'static str, count: usize) -> Result<(), WireError> {
    items
        .try_reserve_exact(count)
        .map_err(|_| WireError::LengthTooLarge(field, count))
}

fn read_value_type(raw: u8) -> Result<ValueType, WireError> {
    ValueType::try_from(raw).map_err(|()| WireError::InvalidValueType(raw))
}

fn skip_type_map(cursor: &mut Cursor<'_>) -> Result<Option<usize>, WireError> {
    match cursor.read_u8()? {
        0 => Ok(None),
        1 => {
            cursor.read_bool()?;
            let local_count = cursor.read_count_with_overhead("type map locals", 4, 12)?;
            if local_count > MAX_FRAME_LOCAL_COUNT {
                return Err(WireError::LengthTooLarge("type map locals", local_count));
            }
            for _ in 0..local_count {
                read_value_type(cursor.read_u8()?)?;
            }
            for _ in 0..local_count {
                match cursor.read_u8()? {
                    0 => {}
                    1 => skip_schema(cursor, 0)?,
                    value => return Err(WireError::InvalidBool(value)),
                }
            }
            skip_bool_vector(cursor, local_count)?;
            skip_bool_vector(cursor, local_count)?;

            let operand_count = cursor.read_count("type map operands", 6)?;
            for _ in 0..operand_count {
                cursor.read_u32()?;
                read_value_type(cursor.read_u8()?)?;
                read_value_type(cursor.read_u8()?)?;
            }
            Ok(Some(local_count))
        }
        value => Err(WireError::InvalidTypeMapFlag(value)),
    }
}

fn skip_bool_vector(cursor: &mut Cursor<'_>, expected: usize) -> Result<(), WireError> {
    let count = cursor.read_u32()? as usize;
    if count != expected {
        return Err(WireError::TrailingBytes);
    }
    cursor.validate_count("type map boolean vector", count, 1)?;
    cursor.debit_count("type map boolean vector", count)?;
    for _ in 0..count {
        cursor.read_bool()?;
    }
    Ok(())
}

fn skip_host_import_schema(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    cursor.skip_string()?;
    let parameter_count = cursor.read_count("host import schema parameters", 6)?;
    for _ in 0..parameter_count {
        cursor.skip_string()?;
        skip_host_schema(cursor, 0)?;
        match cursor.read_u8()? {
            0..=3 => {}
            value => return Err(WireError::InvalidValueType(value)),
        }
    }
    skip_host_schema(cursor, 0)?;
    cursor.read_exact(8).map(|_| ())
}

fn skip_host_schema(cursor: &mut Cursor<'_>, depth: usize) -> Result<(), WireError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(WireError::SchemaTooDeep);
    }
    match cursor.read_u8()? {
        0..=7 => Ok(()),
        8..=10 => skip_host_schema(cursor, depth + 1),
        11 => {
            let parameter_count =
                cursor.read_count_with_overhead("host callable parameters", 1, 1)?;
            for _ in 0..parameter_count {
                skip_host_schema(cursor, depth + 1)?;
            }
            skip_host_schema(cursor, depth + 1)
        }
        12 => cursor.skip_string(),
        13 => {
            cursor.skip_string()?;
            let field_count = cursor.read_count_with_overhead("host named struct fields", 1, 1)?;
            for _ in 0..field_count {
                cursor.skip_string()?;
                skip_host_schema(cursor, depth + 1)?;
            }
            Ok(())
        }
        14 => {
            skip_host_schema(cursor, depth + 1)?;
            skip_host_schema(cursor, depth + 1)
        }
        value => Err(WireError::InvalidValueType(value)),
    }
}

fn skip_schema(cursor: &mut Cursor<'_>, depth: usize) -> Result<(), WireError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(WireError::SchemaTooDeep);
    }
    cursor.debit_count("schema nodes", 1)?;
    let nested_depth = depth.checked_add(1).ok_or(WireError::SchemaTooDeep)?;
    match cursor.read_u8()? {
        0..=7 => Ok(()),
        8 => cursor.skip_string(),
        9 => {
            cursor.skip_string()?;
            let count = cursor.read_count("schema type args", 1)?;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        10 | 13 | 16 => skip_schema(cursor, nested_depth),
        11 => {
            let count = cursor.read_count("schema tuple items", 1)?;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        12 => {
            let count = cursor.read_count_with_overhead("schema tuple prefix", 1, 1)?;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            skip_schema(cursor, nested_depth)
        }
        14 => {
            let count = cursor.read_count("schema object fields", 5)?;
            for _ in 0..count {
                cursor.skip_string()?;
                skip_schema(cursor, nested_depth)?;
            }
            Ok(())
        }
        15 => {
            let count = cursor.read_count_with_overhead("schema callable params", 1, 1)?;
            for _ in 0..count {
                skip_schema(cursor, nested_depth)?;
            }
            skip_schema(cursor, nested_depth)
        }
        17 => skip_resource_key(cursor),
        18 => {
            skip_schema(cursor, nested_depth)?;
            skip_schema(cursor, nested_depth)
        }
        value => Err(WireError::InvalidValueType(value)),
    }
}

fn read_named_cleanup_decls(cursor: &mut Cursor<'_>) -> Result<Vec<NamedCleanupDecl>, WireError> {
    let count = cursor.read_count("named struct decls", 1)?;
    let mut decls: Vec<NamedCleanupDecl> = Vec::new();
    reserve(&mut decls, "named struct decls", count)?;
    for _ in 0..count {
        let name = cursor.read_string()?;
        if decls.iter().any(|existing| existing.name == name) {
            return Err(WireError::InvalidValueType(0));
        }
        let param_count = cursor.read_count("named struct type params", 1)?;
        let mut type_params: Vec<String> = Vec::new();
        reserve(&mut type_params, "named struct type params", param_count)?;
        for _ in 0..param_count {
            let param = cursor.read_string()?;
            if type_params.iter().any(|existing| existing == &param) {
                return Err(WireError::InvalidValueType(0));
            }
            type_params.push(param);
        }
        let body = read_cleanup_schema(cursor, 0)?;
        decls.push(NamedCleanupDecl {
            name,
            type_params,
            body,
        });
    }
    Ok(decls)
}

fn read_ret_cleanups(
    cursor: &mut Cursor<'_>,
    code: &[u8],
    prototypes: &[CallablePrototype],
    regions: &[FunctionRegion],
    decls: &[NamedCleanupDecl],
) -> Result<(), WireError> {
    let plan_count = cursor.read_count_with_overhead("ret cleanup plans", 8, 0)?;
    if plan_count > code.len() {
        return Err(WireError::LengthTooLarge("ret cleanup plans", plan_count));
    }
    let ret_sites = if plan_count == 0 {
        Vec::new()
    } else {
        ret_instruction_sites(code)?
    };
    let mut previous_site = None;
    let mut saw_resource = false;
    for _ in 0..plan_count {
        let site = cursor.read_u32()?;
        if previous_site.is_some_and(|previous| site <= previous) {
            return Err(WireError::InvalidRetCleanup(
                "cleanup sites are not canonical",
            ));
        }
        previous_site = Some(site);
        let frame_locals = cleanup_frame_local_count(prototypes, regions, &ret_sites, site)?;
        let local_count = cursor.read_u32()? as usize;
        if local_count > frame_locals || local_count > MAX_FRAME_LOCAL_COUNT {
            return Err(WireError::LengthTooLarge("ret cleanup locals", local_count));
        }
        cursor.validate_count("ret cleanup locals", local_count, 3)?;
        cursor.debit_count("ret cleanup locals", local_count)?;
        let mut previous_slot = None;
        for _ in 0..local_count {
            let slot = cursor.read_u16()?;
            if previous_slot.is_some_and(|previous| slot <= previous) {
                return Err(WireError::InvalidRetCleanup(
                    "cleanup locals are not canonical",
                ));
            }
            if slot as usize >= frame_locals {
                return Err(WireError::InvalidRetCleanup(
                    "cleanup local is outside the frame",
                ));
            }
            previous_slot = Some(slot);
            let schema = read_cleanup_schema(cursor, 0)?;
            if !cleanup_schema_owns(&schema, decls, &[], &[], 0, &mut 0, &mut Vec::new())? {
                return Err(WireError::InvalidRetCleanup(
                    "cleanup schema has no owned resource",
                ));
            }
            saw_resource = true;
        }
    }
    if saw_resource {
        return Err(WireError::UnsupportedCapability(
            "ret cleanup resource plans",
        ));
    }
    Ok(())
}

fn ret_instruction_sites(code: &[u8]) -> Result<Vec<u32>, WireError> {
    let mut ip = 0usize;
    let mut sites = Vec::new();
    while ip < code.len() {
        let opcode = OpCode::try_from(code[ip])
            .map_err(|_| WireError::InvalidRetCleanup("cleanup bytecode has an invalid opcode"))?;
        if opcode == OpCode::Ret {
            let site = u32::try_from(ip).map_err(|_| WireError::LengthTooLarge("code", ip))?;
            sites.push(site);
        }
        let next = ip
            .checked_add(1)
            .and_then(|value| value.checked_add(opcode.operand_len()))
            .ok_or(WireError::LengthTooLarge("code", ip))?;
        if next > code.len() {
            return Err(WireError::InvalidRetCleanup("truncated instruction"));
        }
        ip = next;
    }
    Ok(sites)
}

fn cleanup_frame_local_count(
    prototypes: &[CallablePrototype],
    regions: &[FunctionRegion],
    ret_sites: &[u32],
    site: u32,
) -> Result<usize, WireError> {
    if !ret_sites.contains(&site) {
        return Err(WireError::InvalidRetCleanup(
            "cleanup site is not a Ret instruction",
        ));
    }
    let mut frame_locals = None;
    for region in regions {
        if site < region.start_ip || site >= region.end_ip {
            continue;
        }
        let Some(prototype_id) = region.prototype_id else {
            continue;
        };
        let Some(prototype) = prototypes.get(prototype_id as usize) else {
            return Err(WireError::InvalidRetCleanup(
                "cleanup frame prototype is missing",
            ));
        };
        if !matches!(prototype.target, CallableTarget::ScriptFunction(_)) {
            return Err(WireError::InvalidRetCleanup(
                "cleanup frame is not a script function",
            ));
        }
        if frame_locals.replace(prototype.frame_local_count).is_some() {
            return Err(WireError::InvalidRetCleanup(
                "cleanup site matches multiple frames",
            ));
        }
    }
    frame_locals.ok_or(WireError::InvalidRetCleanup(
        "cleanup site is not in a callable frame",
    ))
}

const CLEANUP_SCHEMA_WALK_NODES: usize = 16_384;

struct NamedCleanupDecl {
    name: String,
    type_params: Vec<String>,
    body: WireSchema,
}

#[derive(Clone)]
enum WireSchema {
    Scalar,
    Generic(String),
    Named {
        name: String,
        args: Vec<WireSchema>,
    },
    Array(Box<WireSchema>),
    Tuple(Vec<WireSchema>),
    TupleRest {
        prefix: Vec<WireSchema>,
        rest: Box<WireSchema>,
    },
    Map(Box<WireSchema>),
    Object(Vec<(String, WireSchema)>),
    Callable {
        params: Vec<WireSchema>,
        result: Box<WireSchema>,
    },
    Optional(Box<WireSchema>),
    Resource,
    Result {
        ok: Box<WireSchema>,
        err: Box<WireSchema>,
    },
}

fn read_cleanup_schema(cursor: &mut Cursor<'_>, depth: usize) -> Result<WireSchema, WireError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(WireError::SchemaTooDeep);
    }
    cursor.debit_count("schema nodes", 1)?;
    let nested = depth.checked_add(1).ok_or(WireError::SchemaTooDeep)?;
    Ok(match cursor.read_u8()? {
        0..=7 => WireSchema::Scalar,
        8 => WireSchema::Generic(cursor.read_string()?),
        9 => {
            let name = cursor.read_string()?;
            let count = cursor.read_count("schema type args", 1)?;
            let mut args = Vec::new();
            reserve(&mut args, "schema type args", count)?;
            for _ in 0..count {
                args.push(read_cleanup_schema(cursor, nested)?);
            }
            WireSchema::Named { name, args }
        }
        10 => WireSchema::Array(Box::new(read_cleanup_schema(cursor, nested)?)),
        11 => {
            let count = cursor.read_count("schema tuple items", 1)?;
            let mut items = Vec::new();
            reserve(&mut items, "schema tuple items", count)?;
            for _ in 0..count {
                items.push(read_cleanup_schema(cursor, nested)?);
            }
            WireSchema::Tuple(items)
        }
        12 => {
            let count = cursor.read_count_with_overhead("schema tuple prefix", 1, 1)?;
            let mut prefix = Vec::new();
            reserve(&mut prefix, "schema tuple prefix", count)?;
            for _ in 0..count {
                prefix.push(read_cleanup_schema(cursor, nested)?);
            }
            WireSchema::TupleRest {
                prefix,
                rest: Box::new(read_cleanup_schema(cursor, nested)?),
            }
        }
        13 => WireSchema::Map(Box::new(read_cleanup_schema(cursor, nested)?)),
        14 => {
            let count = cursor.read_count("schema object fields", 5)?;
            let mut fields = Vec::new();
            reserve(&mut fields, "schema object fields", count)?;
            for _ in 0..count {
                fields.push((cursor.read_string()?, read_cleanup_schema(cursor, nested)?));
            }
            WireSchema::Object(fields)
        }
        15 => {
            let count = cursor.read_count_with_overhead("schema callable params", 1, 1)?;
            let mut params = Vec::new();
            reserve(&mut params, "schema callable params", count)?;
            for _ in 0..count {
                params.push(read_cleanup_schema(cursor, nested)?);
            }
            WireSchema::Callable {
                params,
                result: Box::new(read_cleanup_schema(cursor, nested)?),
            }
        }
        16 => WireSchema::Optional(Box::new(read_cleanup_schema(cursor, nested)?)),
        17 => {
            skip_resource_key(cursor)?;
            WireSchema::Resource
        }
        18 => WireSchema::Result {
            ok: Box::new(read_cleanup_schema(cursor, nested)?),
            err: Box::new(read_cleanup_schema(cursor, nested)?),
        },
        value => return Err(WireError::InvalidValueType(value)),
    })
}

fn cleanup_schema_owns(
    schema: &WireSchema,
    decls: &[NamedCleanupDecl],
    param_names: &[String],
    param_args: &[WireSchema],
    depth: usize,
    nodes: &mut usize,
    active: &mut Vec<String>,
) -> Result<bool, WireError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(WireError::SchemaTooDeep);
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or(WireError::LengthTooLarge("schema nodes", usize::MAX))?;
    if *nodes > CLEANUP_SCHEMA_WALK_NODES {
        return Err(WireError::LengthTooLarge("schema nodes", *nodes));
    }
    let next = depth + 1;
    match schema {
        WireSchema::Scalar => Ok(false),
        WireSchema::Generic(name) => {
            let Some(index) = param_names.iter().position(|param| param == name) else {
                return Ok(false);
            };
            cleanup_schema_owns(&param_args[index], decls, &[], &[], next, nodes, active)
        }
        WireSchema::Resource => Ok(true),
        WireSchema::Array(inner) | WireSchema::Map(inner) | WireSchema::Optional(inner) => {
            cleanup_schema_owns(inner, decls, param_names, param_args, next, nodes, active)
        }
        WireSchema::Tuple(items) => {
            any_owns(items, decls, param_names, param_args, next, nodes, active)
        }
        WireSchema::TupleRest { prefix, rest } => {
            let mut owned = any_owns(prefix, decls, param_names, param_args, next, nodes, active)?;
            owned |=
                cleanup_schema_owns(rest, decls, param_names, param_args, next, nodes, active)?;
            Ok(owned)
        }
        WireSchema::Object(fields) => {
            let mut owned = false;
            for (_, field) in fields {
                owned |= cleanup_schema_owns(
                    field,
                    decls,
                    param_names,
                    param_args,
                    next,
                    nodes,
                    active,
                )?;
            }
            Ok(owned)
        }
        WireSchema::Callable { params, result } => {
            any_owns(params, decls, param_names, param_args, next, nodes, active)?;
            cleanup_schema_owns(result, decls, param_names, param_args, next, nodes, active)?;
            Ok(false)
        }
        WireSchema::Result { ok, err } => {
            let ok_owned =
                cleanup_schema_owns(ok, decls, param_names, param_args, next, nodes, active)?;
            let err_owned =
                cleanup_schema_owns(err, decls, param_names, param_args, next, nodes, active)?;
            Ok(ok_owned || err_owned)
        }
        WireSchema::Named { name, args } => {
            let Some(decl) = decls.iter().find(|decl| decl.name == *name) else {
                return Err(WireError::InvalidRetCleanup(
                    "unresolved named cleanup schema",
                ));
            };
            if decl.type_params.len() != args.len() {
                return Err(WireError::InvalidRetCleanup("named cleanup schema arity"));
            }
            if active.iter().any(|active_name| active_name == name) {
                return Err(WireError::InvalidRetCleanup("named cleanup schema cycle"));
            }
            let resolved = args
                .iter()
                .map(|arg| substitute_schema(arg, param_names, param_args))
                .collect::<Vec<_>>();
            active.push(name.clone());
            let mut owned = cleanup_schema_owns(
                &decl.body,
                decls,
                &decl.type_params,
                &resolved,
                next,
                nodes,
                active,
            )?;
            owned |= any_owns(&resolved, decls, &[], &[], next, nodes, active)?;
            active.pop();
            Ok(owned)
        }
    }
}

fn any_owns(
    items: &[WireSchema],
    decls: &[NamedCleanupDecl],
    param_names: &[String],
    param_args: &[WireSchema],
    depth: usize,
    nodes: &mut usize,
    active: &mut Vec<String>,
) -> Result<bool, WireError> {
    let mut owned = false;
    for item in items {
        owned |= cleanup_schema_owns(item, decls, param_names, param_args, depth, nodes, active)?;
    }
    Ok(owned)
}

fn substitute_schema(
    schema: &WireSchema,
    param_names: &[String],
    param_args: &[WireSchema],
) -> WireSchema {
    match schema {
        WireSchema::Generic(name) => param_names
            .iter()
            .position(|param| param == name)
            .map(|index| param_args[index].clone())
            .unwrap_or_else(|| schema.clone()),
        WireSchema::Named { name, args } => WireSchema::Named {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| substitute_schema(arg, param_names, param_args))
                .collect(),
        },
        WireSchema::Array(inner) => {
            WireSchema::Array(Box::new(substitute_schema(inner, param_names, param_args)))
        }
        WireSchema::Optional(inner) => {
            WireSchema::Optional(Box::new(substitute_schema(inner, param_names, param_args)))
        }
        WireSchema::Map(inner) => {
            WireSchema::Map(Box::new(substitute_schema(inner, param_names, param_args)))
        }
        WireSchema::Tuple(items) => WireSchema::Tuple(
            items
                .iter()
                .map(|item| substitute_schema(item, param_names, param_args))
                .collect(),
        ),
        WireSchema::TupleRest { prefix, rest } => WireSchema::TupleRest {
            prefix: prefix
                .iter()
                .map(|item| substitute_schema(item, param_names, param_args))
                .collect(),
            rest: Box::new(substitute_schema(rest, param_names, param_args)),
        },
        WireSchema::Object(fields) => WireSchema::Object(
            fields
                .iter()
                .map(|(name, field)| {
                    (
                        name.clone(),
                        substitute_schema(field, param_names, param_args),
                    )
                })
                .collect(),
        ),
        WireSchema::Callable { params, result } => WireSchema::Callable {
            params: params
                .iter()
                .map(|param| substitute_schema(param, param_names, param_args))
                .collect(),
            result: Box::new(substitute_schema(result, param_names, param_args)),
        },
        WireSchema::Result { ok, err } => WireSchema::Result {
            ok: Box::new(substitute_schema(ok, param_names, param_args)),
            err: Box::new(substitute_schema(err, param_names, param_args)),
        },
        WireSchema::Scalar | WireSchema::Resource => schema.clone(),
    }
}

fn skip_resource_key(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    let bytes = cursor.read_blob("schema resource key")?;
    if bytes.is_empty() || bytes.len() > MAX_RESOURCE_KEY_LEN {
        return Err(WireError::InvalidResourceKey);
    }
    core::str::from_utf8(bytes).map_err(|_| WireError::InvalidUtf8)?;
    let mut segment_start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let allowed = byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'.');
        if !allowed {
            return Err(WireError::InvalidResourceKey);
        }
        if byte == b'.' {
            if index == segment_start {
                return Err(WireError::InvalidResourceKey);
            }
            segment_start = index + 1;
        }
    }
    if segment_start == bytes.len() {
        return Err(WireError::InvalidResourceKey);
    }
    Ok(())
}

type CallableMetadata = (
    Vec<ScriptFunction>,
    Vec<CallablePrototype>,
    Vec<FunctionRegion>,
    Vec<RootCallableBinding>,
    Vec<ExportedCallable>,
);

fn read_callable_metadata(cursor: &mut Cursor<'_>) -> Result<CallableMetadata, WireError> {
    let function_count = cursor.read_count("script functions", 8)?;
    let mut script_functions = Vec::new();
    reserve(&mut script_functions, "script functions", function_count)?;
    for _ in 0..function_count {
        script_functions.push(ScriptFunction {
            entry_ip: cursor.read_u32()?,
            end_ip: cursor.read_u32()?,
        });
    }

    let prototype_count = cursor.read_count("callable prototypes", 29)?;
    let mut prototypes = Vec::new();
    reserve(&mut prototypes, "callable prototypes", prototype_count)?;
    for _ in 0..prototype_count {
        let kind = match cursor.read_u8()? {
            0 => CallableKind::FunctionItem,
            1 => CallableKind::Closure,
            2 => CallableKind::HostFunction,
            value => return Err(WireError::InvalidValueType(value)),
        };
        let target_tag = cursor.read_u8()?;
        let target_id = cursor.read_u32()?;
        let target = match target_tag {
            0 => CallableTarget::ScriptFunction(target_id),
            1 => CallableTarget::HostImport(u16::try_from(target_id).map_err(|_| {
                WireError::LengthTooLarge("host callable target", target_id as usize)
            })?),
            value => return Err(WireError::InvalidValueType(value)),
        };
        let arity = cursor.read_u8()?;
        let frame_local_count = cursor.read_limited_count("callable frame locals")?;
        let parameter_count = cursor.read_count("callable parameters", 2)?;
        let mut parameter_slots = Vec::new();
        reserve(&mut parameter_slots, "callable parameters", parameter_count)?;
        for _ in 0..parameter_count {
            parameter_slots.push(cursor.read_u16()?);
        }
        let capture_source_count = cursor.read_count("callable capture sources", 2)?;
        let mut capture_source_slots = Vec::new();
        reserve(
            &mut capture_source_slots,
            "callable capture sources",
            capture_source_count,
        )?;
        for _ in 0..capture_source_count {
            capture_source_slots.push(cursor.read_u16()?);
        }
        let capture_count = cursor.read_count("callable captures", 2)?;
        let mut capture_slots = Vec::new();
        reserve(&mut capture_slots, "callable captures", capture_count)?;
        for _ in 0..capture_count {
            capture_slots.push(cursor.read_u16()?);
        }
        let capture_mode_count = cursor.read_count("callable capture modes", 1)?;
        let mut capture_modes = Vec::new();
        reserve(
            &mut capture_modes,
            "callable capture modes",
            capture_mode_count,
        )?;
        for _ in 0..capture_mode_count {
            capture_modes.push(match cursor.read_u8()? {
                0 => CaptureBindingMode::Copy,
                1 => CaptureBindingMode::Borrow,
                2 => CaptureBindingMode::BorrowMut,
                3 => CaptureBindingMode::Move,
                other => return Err(WireError::InvalidCaptureBindingMode(other)),
            });
        }
        let self_slot = cursor.read_bool()?.then(|| cursor.read_u16()).transpose()?;
        if cursor.read_bool()? {
            skip_schema(cursor, 0)?;
        }
        prototypes.push(CallablePrototype {
            kind,
            target,
            arity,
            frame_local_count,
            parameter_slots,
            capture_source_slots,
            capture_slots,
            capture_modes,
            self_slot,
        });
    }

    let region_count = cursor.read_count("function regions", 9)?;
    let mut regions = Vec::new();
    reserve(&mut regions, "function regions", region_count)?;
    for _ in 0..region_count {
        let start_ip = cursor.read_u32()?;
        let end_ip = cursor.read_u32()?;
        let prototype_id = cursor.read_bool()?.then(|| cursor.read_u32()).transpose()?;
        regions.push(FunctionRegion {
            start_ip,
            end_ip,
            prototype_id,
        });
    }

    let binding_count = cursor.read_count("root callable bindings", 6)?;
    let mut bindings = Vec::new();
    reserve(&mut bindings, "root callable bindings", binding_count)?;
    for _ in 0..binding_count {
        bindings.push(RootCallableBinding {
            local_slot: cursor.read_u16()?,
            prototype_id: cursor.read_u32()?,
        });
    }
    let export_count = cursor.read_count("exported callables", 6)?;
    let mut exported_callables = Vec::new();
    reserve(&mut exported_callables, "exported callables", export_count)?;
    for _ in 0..export_count {
        exported_callables.push(ExportedCallable {
            name: cursor.read_string()?,
            local_slot: cursor.read_u16()?,
        });
    }
    Ok((
        script_functions,
        prototypes,
        regions,
        bindings,
        exported_callables,
    ))
}

/// Deterministically reject malformed or inconsistent `CallScript` operands,
/// mirroring the std VMBC V12 decoder: truncated operands, out-of-range
/// prototype ids, prototypes that do not target a script function, and argc
/// values that disagree with the prototype arity.
fn validate_call_script_operands(
    code: &[u8],
    prototypes: &[CallablePrototype],
) -> Result<(), WireError> {
    let mut ip = 0usize;
    while ip < code.len() {
        let opcode_byte = code[ip];
        let Ok(opcode) = OpCode::try_from(opcode_byte) else {
            // Unknown opcodes surface as `InvalidOpcode` at run time; skip a
            // single byte so the walk stays aligned for the opcodes that
            // follow.
            ip = ip.saturating_add(1);
            continue;
        };
        let operand_len = opcode.operand_len();
        let operands_start = ip.saturating_add(1);
        let operands_end = operands_start
            .checked_add(operand_len)
            .ok_or(WireError::LengthTooLarge("code", code.len()))?;
        if operands_end > code.len() {
            return Err(WireError::TruncatedOperand {
                opcode: opcode_byte,
                expected_bytes: operand_len,
            });
        }
        if matches!(opcode, OpCode::CallScript) {
            let prototype_id = u32::from_le_bytes(
                code[operands_start..operands_start + 4]
                    .try_into()
                    .expect("operand width validated above"),
            );
            let argc = code[operands_start + 4];
            let Some(prototype) = prototypes.get(prototype_id as usize) else {
                return Err(WireError::InvalidCallScriptTarget { prototype_id });
            };
            // `CallScript` is a static script-function call: a host-import
            // prototype must never be routed to the host path, so reject it
            // deterministically here as well.
            if !matches!(prototype.target, CallableTarget::ScriptFunction(_)) {
                return Err(WireError::InvalidCallScriptTarget { prototype_id });
            }
            if argc != prototype.arity {
                return Err(WireError::InvalidCallScriptArity {
                    prototype_id,
                    expected: prototype.arity,
                    got: argc,
                });
            }
        }
        ip = operands_end;
    }
    Ok(())
}

fn skip_debug_info(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    match cursor.read_u8()? {
        0 => Ok(()),
        1 => {
            match cursor.read_u8()? {
                0 => {}
                1 => cursor.skip_string()?,
                value => return Err(WireError::InvalidDebugFlag(value)),
            }

            let line_count = cursor.read_count("debug lines", 8)?;
            cursor.skip_count("debug lines", line_count, 8)?;

            let function_count = cursor.read_count("debug functions", 8)?;
            for _ in 0..function_count {
                cursor.skip_string()?;
                let arg_count = cursor.read_count("debug function args", 5)?;
                for _ in 0..arg_count {
                    cursor.skip_string()?;
                    cursor.read_u8()?;
                }
            }

            let local_count = cursor.read_count("debug locals", 7)?;
            for _ in 0..local_count {
                cursor.skip_string()?;
                cursor.read_u8()?;
                skip_optional_u32(cursor)?;
                skip_optional_u32(cursor)?;
            }
            Ok(())
        }
        value => Err(WireError::InvalidDebugFlag(value)),
    }
}

fn skip_optional_u32(cursor: &mut Cursor<'_>) -> Result<(), WireError> {
    match cursor.read_u8()? {
        0 => Ok(()),
        1 => {
            cursor.read_u32()?;
            Ok(())
        }
        value => Err(WireError::InvalidDebugFlag(value)),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
    remaining_budget: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            remaining_budget: MAX_WIRE_AGGREGATE_ITEMS,
        }
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_u8(&mut self) -> Result<u8, WireError> {
        let value = *self
            .bytes
            .get(self.offset)
            .ok_or(WireError::UnexpectedEof)?;
        self.offset += 1;
        Ok(value)
    }

    fn read_bool(&mut self) -> Result<bool, WireError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(WireError::InvalidBool(value)),
        }
    }

    fn read_u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_le_bytes(self.read_array()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let bytes = self.read_exact(N)?;
        bytes.try_into().map_err(|_| WireError::UnexpectedEof)
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(WireError::LengthTooLarge("payload", length))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(WireError::UnexpectedEof)?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_blob(&mut self, field: &'static str) -> Result<&'a [u8], WireError> {
        let length = self.read_u32()? as usize;
        if length > MAX_WIRE_BLOB_BYTES {
            return Err(WireError::LengthTooLarge(field, length));
        }
        self.read_exact(length)
    }

    fn read_string(&mut self) -> Result<String, WireError> {
        let bytes = self.read_blob("string")?;
        let text = core::str::from_utf8(bytes).map_err(|_| WireError::InvalidUtf8)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| WireError::LengthTooLarge("string", text.len()))?;
        owned.push_str(text);
        Ok(owned)
    }

    fn skip_string(&mut self) -> Result<(), WireError> {
        self.read_blob("string").map(|_| ())
    }

    fn read_count(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
    ) -> Result<usize, WireError> {
        self.read_count_with_overhead(field, min_item_bytes, 0)
    }

    fn read_count_with_overhead(
        &mut self,
        field: &'static str,
        min_item_bytes: usize,
        fixed_bytes: usize,
    ) -> Result<usize, WireError> {
        let count = self.read_u32()? as usize;
        self.validate_count_with_overhead(field, count, min_item_bytes, fixed_bytes)?;
        self.debit_count(field, count)?;
        Ok(count)
    }

    fn read_limited_count(&mut self, field: &'static str) -> Result<usize, WireError> {
        let count = self.read_u32()? as usize;
        if count > MAX_FRAME_LOCAL_COUNT {
            return Err(WireError::LengthTooLarge(field, count));
        }
        self.debit_count(field, count)?;
        Ok(count)
    }

    fn debit_count(&mut self, field: &'static str, count: usize) -> Result<(), WireError> {
        if count > MAX_WIRE_COUNT {
            return Err(WireError::LengthTooLarge(field, count));
        }
        self.remaining_budget = self
            .remaining_budget
            .checked_sub(count)
            .ok_or(WireError::LengthTooLarge(field, count))?;
        Ok(())
    }

    fn validate_count(
        &self,
        field: &'static str,
        count: usize,
        min_item_bytes: usize,
    ) -> Result<(), WireError> {
        self.validate_count_with_overhead(field, count, min_item_bytes, 0)
    }

    fn validate_count_with_overhead(
        &self,
        field: &'static str,
        count: usize,
        min_item_bytes: usize,
        fixed_bytes: usize,
    ) -> Result<(), WireError> {
        if count > MAX_WIRE_COUNT {
            return Err(WireError::LengthTooLarge(field, count));
        }
        let item_bytes = count
            .checked_mul(min_item_bytes)
            .ok_or(WireError::LengthTooLarge(field, count))?;
        let required_bytes = item_bytes
            .checked_add(fixed_bytes)
            .ok_or(WireError::LengthTooLarge(field, count))?;
        if required_bytes > self.remaining() {
            return Err(WireError::LengthTooLarge(field, count));
        }
        Ok(())
    }

    fn skip_count(
        &mut self,
        field: &'static str,
        count: usize,
        item_size: usize,
    ) -> Result<(), WireError> {
        let length = count
            .checked_mul(item_size)
            .ok_or(WireError::LengthTooLarge(field, count))?;
        self.read_exact(length).map(|_| ())
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn bool_vectors_debit_one_shared_checked_budget() {
        const COUNT: usize = 40_000;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(COUNT as u32).to_le_bytes());
        bytes.extend(core::iter::repeat_n(0, COUNT));
        bytes.extend_from_slice(&(COUNT as u32).to_le_bytes());
        bytes.extend(core::iter::repeat_n(0, COUNT));
        let mut cursor = Cursor::new(&bytes);
        cursor.remaining_budget = COUNT * 2 - 1;

        skip_bool_vector(&mut cursor, COUNT).unwrap();
        assert_eq!(
            skip_bool_vector(&mut cursor, COUNT),
            Err(WireError::LengthTooLarge("type map boolean vector", COUNT))
        );
    }

    #[test]
    fn count_size_arithmetic_overflow_is_rejected() {
        let cursor = Cursor::new(&[]);
        assert_eq!(
            cursor.validate_count_with_overhead("overflow", 2, usize::MAX, 0),
            Err(WireError::LengthTooLarge("overflow", 2))
        );
    }
}
