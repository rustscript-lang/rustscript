use std::collections::HashMap;

use crate::debug_info::DebugInfoBuilder;
use crate::{OpCode, Program, Value};

pub struct BytecodeBuilder {
    code: Vec<u8>,
}

#[derive(Debug)]
pub enum AssemblerError {
    DuplicateLabel(String),
    UnknownLabel(String),
}

struct Fixup {
    at: usize,
    label: String,
}

pub struct Assembler {
    code: Vec<u8>,
    constants: Vec<Value>,
    int_constants: HashMap<i64, u32>,
    float_constants: HashMap<u64, u32>,
    bool_constants: HashMap<bool, u32>,
    string_constants: HashMap<String, u32>,
    labels: HashMap<String, u32>,
    fixups: Vec<Fixup>,
    debug: DebugInfoBuilder,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            code: Vec::new(),
            constants: Vec::new(),
            int_constants: HashMap::new(),
            float_constants: HashMap::new(),
            bool_constants: HashMap::new(),
            string_constants: HashMap::new(),
            labels: HashMap::new(),
            fixups: Vec::new(),
            debug: DebugInfoBuilder::new(),
        }
    }

    pub fn position(&self) -> u32 {
        self.code.len() as u32
    }

    pub fn label(&mut self, name: &str) -> Result<(), AssemblerError> {
        if self.labels.contains_key(name) {
            return Err(AssemblerError::DuplicateLabel(name.to_string()));
        }
        let pos = self.position();
        self.labels.insert(name.to_string(), pos);
        Ok(())
    }

    pub fn set_source(&mut self, source: String) {
        self.debug.set_source(source);
    }

    pub fn mark_line(&mut self, line: u32) {
        let offset = self.code.len() as u32;
        self.debug.mark_line(offset, line);
    }

    pub fn add_function(&mut self, name: String, args: Vec<String>) {
        self.debug.add_function(name, args);
    }

    pub fn add_local(&mut self, name: String, index: u8) {
        self.debug.add_local(name, index);
    }

    pub fn add_constant(&mut self, value: Value) -> u32 {
        match value {
            Value::Int(number) => {
                if let Some(index) = self.int_constants.get(&number).copied() {
                    return index;
                }
                let index = self.constants.len() as u32;
                self.constants.push(Value::Int(number));
                self.int_constants.insert(number, index);
                index
            }
            Value::Float(number) => {
                let bits = number.to_bits();
                if let Some(index) = self.float_constants.get(&bits).copied() {
                    return index;
                }
                let index = self.constants.len() as u32;
                self.constants.push(Value::Float(number));
                self.float_constants.insert(bits, index);
                index
            }
            Value::Bool(flag) => {
                if let Some(index) = self.bool_constants.get(&flag).copied() {
                    return index;
                }
                let index = self.constants.len() as u32;
                self.constants.push(Value::Bool(flag));
                self.bool_constants.insert(flag, index);
                index
            }
            Value::String(text) => {
                if let Some(index) = self.string_constants.get(&text).copied() {
                    return index;
                }
                let index = self.constants.len() as u32;
                self.constants.push(Value::String(text.clone()));
                self.string_constants.insert(text, index);
                index
            }
            other => {
                let index = self.constants.len() as u32;
                self.constants.push(other);
                index
            }
        }
    }

    pub fn push_const(&mut self, value: Value) -> u32 {
        let index = self.add_constant(value);
        self.ldc(index);
        index
    }

    pub fn finish_program(mut self) -> Result<Program, AssemblerError> {
        for fixup in self.fixups.drain(..) {
            let target = self
                .labels
                .get(&fixup.label)
                .copied()
                .ok_or_else(|| AssemblerError::UnknownLabel(fixup.label.clone()))?;
            let bytes = target.to_le_bytes();
            self.code[fixup.at..fixup.at + 4].copy_from_slice(&bytes);
        }
        Ok(Program::with_debug(
            self.constants,
            self.code,
            self.debug.finish(),
        ))
    }

    pub fn nop(&mut self) {
        self.emit_opcode(OpCode::Nop);
    }

    pub fn ret(&mut self) {
        self.emit_opcode(OpCode::Ret);
    }

    pub fn ldc(&mut self, index: u32) {
        self.emit_opcode(OpCode::Ldc);
        self.emit_u32(index);
    }

    pub fn add(&mut self) {
        self.emit_opcode(OpCode::Add);
    }

    pub fn sub(&mut self) {
        self.emit_opcode(OpCode::Sub);
    }

    pub fn mul(&mut self) {
        self.emit_opcode(OpCode::Mul);
    }

    pub fn div(&mut self) {
        self.emit_opcode(OpCode::Div);
    }

    pub fn modulo(&mut self) {
        self.emit_opcode(OpCode::Mod);
    }

    pub fn and(&mut self) {
        self.emit_opcode(OpCode::And);
    }

    pub fn or(&mut self) {
        self.emit_opcode(OpCode::Or);
    }

    pub fn neg(&mut self) {
        self.emit_opcode(OpCode::Neg);
    }

    pub fn ceq(&mut self) {
        self.emit_opcode(OpCode::Ceq);
    }

    pub fn clt(&mut self) {
        self.emit_opcode(OpCode::Clt);
    }

    pub fn cgt(&mut self) {
        self.emit_opcode(OpCode::Cgt);
    }

    pub fn br(&mut self, target: u32) {
        self.emit_opcode(OpCode::Br);
        self.emit_u32(target);
    }

    pub fn br_label(&mut self, label: &str) {
        self.emit_opcode(OpCode::Br);
        let at = self.code.len();
        self.emit_u32(0);
        self.fixups.push(Fixup {
            at,
            label: label.to_string(),
        });
    }

    pub fn brfalse(&mut self, target: u32) {
        self.emit_opcode(OpCode::Brfalse);
        self.emit_u32(target);
    }

    pub fn brfalse_label(&mut self, label: &str) {
        self.emit_opcode(OpCode::Brfalse);
        let at = self.code.len();
        self.emit_u32(0);
        self.fixups.push(Fixup {
            at,
            label: label.to_string(),
        });
    }

    pub fn pop(&mut self) {
        self.emit_opcode(OpCode::Pop);
    }

    pub fn dup(&mut self) {
        self.emit_opcode(OpCode::Dup);
    }

    pub fn ldloc(&mut self, index: u8) {
        self.emit_opcode(OpCode::Ldloc);
        self.emit_u8(index);
    }

    pub fn stloc(&mut self, index: u8) {
        self.emit_opcode(OpCode::Stloc);
        self.emit_u8(index);
    }

    pub fn call(&mut self, index: u16, argc: u8) {
        self.emit_opcode(OpCode::Call);
        self.emit_u16(index);
        self.emit_u8(argc);
    }

    pub fn shl(&mut self) {
        self.emit_opcode(OpCode::Shl);
    }

    pub fn shr(&mut self) {
        self.emit_opcode(OpCode::Shr);
    }

    fn emit_opcode(&mut self, opcode: OpCode) {
        self.code.push(opcode as u8);
    }

    fn emit_u8(&mut self, value: u8) {
        self.code.push(value);
    }

    fn emit_u16(&mut self, value: u16) {
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn emit_u32(&mut self, value: u32) {
        self.code.extend_from_slice(&value.to_le_bytes());
    }
}

impl Default for BytecodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BytecodeBuilder {
    pub fn new() -> Self {
        Self { code: Vec::new() }
    }

    pub fn position(&self) -> u32 {
        self.code.len() as u32
    }

    pub fn finish(self) -> Vec<u8> {
        self.code
    }

    pub fn nop(&mut self) {
        self.emit_opcode(OpCode::Nop);
    }

    pub fn ret(&mut self) {
        self.emit_opcode(OpCode::Ret);
    }

    pub fn ldc(&mut self, index: u32) {
        self.emit_opcode(OpCode::Ldc);
        self.emit_u32(index);
    }

    pub fn add(&mut self) {
        self.emit_opcode(OpCode::Add);
    }

    pub fn sub(&mut self) {
        self.emit_opcode(OpCode::Sub);
    }

    pub fn mul(&mut self) {
        self.emit_opcode(OpCode::Mul);
    }

    pub fn div(&mut self) {
        self.emit_opcode(OpCode::Div);
    }

    pub fn modulo(&mut self) {
        self.emit_opcode(OpCode::Mod);
    }

    pub fn and(&mut self) {
        self.emit_opcode(OpCode::And);
    }

    pub fn or(&mut self) {
        self.emit_opcode(OpCode::Or);
    }

    pub fn neg(&mut self) {
        self.emit_opcode(OpCode::Neg);
    }

    pub fn ceq(&mut self) {
        self.emit_opcode(OpCode::Ceq);
    }

    pub fn clt(&mut self) {
        self.emit_opcode(OpCode::Clt);
    }

    pub fn cgt(&mut self) {
        self.emit_opcode(OpCode::Cgt);
    }

    pub fn br(&mut self, target: u32) {
        self.emit_opcode(OpCode::Br);
        self.emit_u32(target);
    }

    pub fn brfalse(&mut self, target: u32) {
        self.emit_opcode(OpCode::Brfalse);
        self.emit_u32(target);
    }

    pub fn pop(&mut self) {
        self.emit_opcode(OpCode::Pop);
    }

    pub fn dup(&mut self) {
        self.emit_opcode(OpCode::Dup);
    }

    pub fn ldloc(&mut self, index: u8) {
        self.emit_opcode(OpCode::Ldloc);
        self.emit_u8(index);
    }

    pub fn stloc(&mut self, index: u8) {
        self.emit_opcode(OpCode::Stloc);
        self.emit_u8(index);
    }

    pub fn call(&mut self, index: u16, argc: u8) {
        self.emit_opcode(OpCode::Call);
        self.emit_u16(index);
        self.emit_u8(argc);
    }

    pub fn shl(&mut self) {
        self.emit_opcode(OpCode::Shl);
    }

    pub fn shr(&mut self) {
        self.emit_opcode(OpCode::Shr);
    }

    fn emit_opcode(&mut self, opcode: OpCode) {
        self.code.push(opcode as u8);
    }

    fn emit_u8(&mut self, value: u8) {
        self.code.push(value);
    }

    fn emit_u16(&mut self, value: u16) {
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn emit_u32(&mut self, value: u32) {
        self.code.extend_from_slice(&value.to_le_bytes());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsmParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for AsmParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for AsmParseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsmSection {
    Data,
    Code,
}

pub fn assemble(source: &str) -> Result<Program, AsmParseError> {
    let mut assembler = Assembler::new();
    assembler.set_source(source.to_string());
    let mut consts: HashMap<String, u32> = HashMap::new();
    let mut locals: HashMap<String, u8> = HashMap::new();
    let mut next_local: u8 = 0;
    let mut section = AsmSection::Code;

    for (line_idx, raw_line) in source.lines().enumerate() {
        let line_no = line_idx + 1;
        let line = strip_comments(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if line.ends_with(':') {
            return Err(AsmParseError {
                line: line_no,
                message: "label definitions must use '.label NAME'".to_string(),
            });
        }

        if let Some(rest) = line.strip_prefix('.') {
            let mut parts = rest.split_whitespace();
            let directive = parts.next().unwrap_or("").to_ascii_lowercase();
            match directive.as_str() {
                "data" => {
                    section = AsmSection::Data;
                }
                "code" => {
                    section = AsmSection::Code;
                }
                "label" => {
                    let name = next_token(&mut parts, line_no, "label name")?;
                    if section != AsmSection::Code {
                        return Err(AsmParseError {
                            line: line_no,
                            message: "labels are only valid in code section".to_string(),
                        });
                    }
                    assembler.label(name).map_err(|err| AsmParseError {
                        line: line_no,
                        message: format!("label error: {err:?}"),
                    })?;
                }
                "const" => {
                    let name = next_token(&mut parts, line_no, "const name")?;
                    if consts.contains_key(name) {
                        return Err(AsmParseError {
                            line: line_no,
                            message: format!("duplicate const '{name}'"),
                        });
                    }
                    let rest = rest_after_n_tokens(line, 2).unwrap_or("");
                    if rest.is_empty() {
                        return Err(AsmParseError {
                            line: line_no,
                            message: "missing const value".to_string(),
                        });
                    }
                    let value = parse_literal(rest, line_no)?;
                    let index = assembler.add_constant(value);
                    consts.insert(name.to_string(), index);
                }
                "local" => {
                    let name = next_token(&mut parts, line_no, "local name")?;
                    if locals.contains_key(name) {
                        return Err(AsmParseError {
                            line: line_no,
                            message: format!("duplicate local '{name}'"),
                        });
                    }

                    let index = if let Some(token) = parts.next() {
                        parse_u8(token, line_no)?
                    } else {
                        let index = next_local;
                        next_local = next_local.checked_add(1).ok_or(AsmParseError {
                            line: line_no,
                            message: "local index overflow".to_string(),
                        })?;
                        index
                    };
                    locals.insert(name.to_string(), index);
                }
                other => {
                    return Err(AsmParseError {
                        line: line_no,
                        message: format!("unknown directive '.{other}'"),
                    });
                }
            }

            if parts.next().is_some() {
                return Err(AsmParseError {
                    line: line_no,
                    message: "unexpected extra tokens".to_string(),
                });
            }
            continue;
        }

        let mut parts = line.split_whitespace();
        let op = parts.next().ok_or_else(|| AsmParseError {
            line: line_no,
            message: "missing opcode".to_string(),
        })?;
        let op = op.to_ascii_lowercase();

        if section == AsmSection::Data {
            match op.as_str() {
                "const" => {
                    let name = next_token(&mut parts, line_no, "const name")?;
                    if consts.contains_key(name) {
                        return Err(AsmParseError {
                            line: line_no,
                            message: format!("duplicate const '{name}'"),
                        });
                    }
                    let rest = rest_after_n_tokens(line, 2).unwrap_or("");
                    if rest.is_empty() {
                        return Err(AsmParseError {
                            line: line_no,
                            message: "missing const value".to_string(),
                        });
                    }
                    let value = parse_literal(rest, line_no)?;
                    let index = assembler.add_constant(value);
                    consts.insert(name.to_string(), index);
                }
                "string" => {
                    let name = next_token(&mut parts, line_no, "string name")?;
                    if consts.contains_key(name) {
                        return Err(AsmParseError {
                            line: line_no,
                            message: format!("duplicate const '{name}'"),
                        });
                    }
                    let rest = rest_after_n_tokens(line, 2).unwrap_or("");
                    if rest.is_empty() {
                        return Err(AsmParseError {
                            line: line_no,
                            message: "missing string literal".to_string(),
                        });
                    }
                    let value = Value::String(parse_string_literal(rest, line_no)?);
                    let index = assembler.add_constant(value);
                    consts.insert(name.to_string(), index);
                }
                other => {
                    return Err(AsmParseError {
                        line: line_no,
                        message: format!("unexpected opcode '{other}' in data section"),
                    });
                }
            }
            continue;
        }

        assembler.mark_line(line_no as u32);
        let mut check_extra = true;
        let opcode = OpCode::parse_mnemonic(op.as_str()).ok_or_else(|| AsmParseError {
            line: line_no,
            message: format!("unknown opcode '{op}'"),
        })?;
        match opcode {
            OpCode::Nop => assembler.nop(),
            OpCode::Ret => assembler.ret(),
            OpCode::Ldc => {
                check_extra = false;
                let rest = rest_after_n_tokens(line, 1).unwrap_or("");
                if rest.is_empty() {
                    return Err(AsmParseError {
                        line: line_no,
                        message: "missing ldc literal".to_string(),
                    });
                }
                if let Some(&index) = consts.get(rest) {
                    assembler.ldc(index);
                } else {
                    assembler.push_const(parse_literal(rest, line_no)?);
                }
            }
            OpCode::Add => assembler.add(),
            OpCode::Sub => assembler.sub(),
            OpCode::Mul => assembler.mul(),
            OpCode::Div => assembler.div(),
            OpCode::Neg => assembler.neg(),
            OpCode::Ceq => assembler.ceq(),
            OpCode::Clt => assembler.clt(),
            OpCode::Cgt => assembler.cgt(),
            OpCode::Br => {
                let target = next_token(&mut parts, line_no, "jump target")?;
                if target.parse::<u32>().is_ok() {
                    return Err(AsmParseError {
                        line: line_no,
                        message: "numeric jump targets are not supported".to_string(),
                    });
                }
                assembler.br_label(target);
            }
            OpCode::Brfalse => {
                let target = next_token(&mut parts, line_no, "jump target")?;
                if target.parse::<u32>().is_ok() {
                    return Err(AsmParseError {
                        line: line_no,
                        message: "numeric jump targets are not supported".to_string(),
                    });
                }
                assembler.brfalse_label(target);
            }
            OpCode::Pop => assembler.pop(),
            OpCode::Dup => assembler.dup(),
            OpCode::Ldloc => {
                let token = next_token(&mut parts, line_no, "local index")?;
                let index = if let Ok(value) = token.parse::<u8>() {
                    value
                } else {
                    *locals.get(token).ok_or(AsmParseError {
                        line: line_no,
                        message: format!("unknown local '{token}'"),
                    })?
                };
                assembler.ldloc(index);
            }
            OpCode::Stloc => {
                let token = next_token(&mut parts, line_no, "local index")?;
                let index = if let Ok(value) = token.parse::<u8>() {
                    value
                } else {
                    *locals.get(token).ok_or(AsmParseError {
                        line: line_no,
                        message: format!("unknown local '{token}'"),
                    })?
                };
                assembler.stloc(index);
            }
            OpCode::Call => {
                let index = parse_u16(next_token(&mut parts, line_no, "call id")?, line_no)?;
                let argc = parse_u8(next_token(&mut parts, line_no, "arg count")?, line_no)?;
                assembler.call(index, argc);
            }
            OpCode::Shl => assembler.shl(),
            OpCode::Shr => assembler.shr(),
            OpCode::Mod => assembler.modulo(),
            OpCode::And => assembler.and(),
            OpCode::Or => assembler.or(),
        }

        if check_extra && parts.next().is_some() {
            return Err(AsmParseError {
                line: line_no,
                message: "unexpected extra tokens".to_string(),
            });
        }
    }

    assembler.finish_program().map_err(|err| AsmParseError {
        line: 0,
        message: format!("assembler error: {err:?}"),
    })
}

fn strip_comments(line: &str) -> &str {
    let hash_idx = line.find('#');
    let slash_idx = line.find("//");
    match (hash_idx, slash_idx) {
        (Some(h), Some(s)) => &line[..h.min(s)],
        (Some(h), None) => &line[..h],
        (None, Some(s)) => &line[..s],
        (None, None) => line,
    }
}

fn next_token<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
    line_no: usize,
    what: &str,
) -> Result<&'a str, AsmParseError> {
    parts.next().ok_or_else(|| AsmParseError {
        line: line_no,
        message: format!("missing {what}"),
    })
}

fn parse_u8(token: &str, line_no: usize) -> Result<u8, AsmParseError> {
    token.parse::<u8>().map_err(|_| AsmParseError {
        line: line_no,
        message: format!("invalid u8 '{token}'"),
    })
}

fn parse_u16(token: &str, line_no: usize) -> Result<u16, AsmParseError> {
    token.parse::<u16>().map_err(|_| AsmParseError {
        line: line_no,
        message: format!("invalid u16 '{token}'"),
    })
}

fn parse_f64(token: &str, line_no: usize, what: &str) -> Result<f64, AsmParseError> {
    token.parse::<f64>().map_err(|_| AsmParseError {
        line: line_no,
        message: format!("invalid {what} '{token}'"),
    })
}

fn parse_literal(token: &str, line_no: usize) -> Result<Value, AsmParseError> {
    let token = token.trim();
    if token.starts_with('"') {
        return Ok(Value::String(parse_string_literal(token, line_no)?));
    }
    if token.eq_ignore_ascii_case("true") {
        Ok(Value::Bool(true))
    } else if token.eq_ignore_ascii_case("false") {
        Ok(Value::Bool(false))
    } else {
        match token.parse::<i64>() {
            Ok(value) => Ok(Value::Int(value)),
            Err(_) => parse_f64(token, line_no, "const literal").map(Value::Float),
        }
    }
}

fn parse_string_literal(token: &str, line_no: usize) -> Result<String, AsmParseError> {
    let mut chars = token.char_indices();
    if chars.next().map(|(_, ch)| ch) != Some('"') {
        return Err(AsmParseError {
            line: line_no,
            message: "string literal must start with '\"'".to_string(),
        });
    }

    let mut out = String::new();
    let mut escaped = false;
    let mut end_idx = None;

    for (idx, ch) in chars {
        if escaped {
            let mapped = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                '0' => '\0',
                other => {
                    return Err(AsmParseError {
                        line: line_no,
                        message: format!("invalid escape '\\{other}'"),
                    });
                }
            };
            out.push(mapped);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => {
                end_idx = Some(idx);
                break;
            }
            other => out.push(other),
        }
    }

    let Some(end_idx) = end_idx else {
        return Err(AsmParseError {
            line: line_no,
            message: "unterminated string literal".to_string(),
        });
    };

    if token[end_idx + 1..].trim().is_empty() {
        Ok(out)
    } else {
        Err(AsmParseError {
            line: line_no,
            message: "unexpected trailing characters after string literal".to_string(),
        })
    }
}

fn rest_after_n_tokens(line: &str, n: usize) -> Option<&str> {
    let mut count = 0;
    let mut in_token = false;
    let mut end_idx = 0;
    for (idx, ch) in line.char_indices() {
        if ch.is_whitespace() {
            if in_token {
                in_token = false;
                count += 1;
                if count == n {
                    end_idx = idx;
                    break;
                }
            }
        } else if !in_token {
            in_token = true;
        }
    }

    if in_token {
        count += 1;
        end_idx = line.len();
    }

    if count < n {
        None
    } else {
        Some(line[end_idx..].trim_start())
    }
}
