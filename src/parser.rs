use crate::error::ParseError;
use crate::instruction::Instruction;
use crate::template::{ArrayItem, ArrayTemplate};
use crate::value::Value;
use std::rc::Rc;

struct RawInstr<'a> {
    line: usize,
    mnemonic: &'a str,
    operand: Option<&'a str>,
}

fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            ';' | '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_string_literal(token: &str, line: usize) -> Result<Rc<str>, ParseError> {
    if token.len() < 2 || !token.starts_with('"') || !token.ends_with('"') {
        return Err(ParseError::InvalidOperand {
            line,
            token: token.to_string(),
        });
    }
    let inner = &token[1..token.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => result.push('"'),
            Some('\\') => result.push('\\'),
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => {
                return Err(ParseError::InvalidOperand {
                    line,
                    token: token.to_string(),
                });
            }
        }
    }
    Ok(Rc::from(result))
}

fn parse_value(token: &str, line: usize) -> Result<Value, ParseError> {
    if token.starts_with('"') {
        return parse_string_literal(token, line).map(Value::Str);
    }
    match token {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "nil" => Ok(Value::Nil),
        _ if token.contains('.') => {
            token
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| ParseError::InvalidOperand {
                    line,
                    token: token.to_string(),
                })
        }
        _ => token
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| ParseError::InvalidOperand {
                line,
                token: token.to_string(),
            }),
    }
}

fn parse_index(token: &str, line: usize) -> Result<usize, ParseError> {
    token
        .parse::<usize>()
        .map_err(|_| ParseError::InvalidOperand {
            line,
            token: token.to_string(),
        })
}

struct ArrayParser<'a> {
    src: &'a str,
    // A byte cursor avoids materializing one `(usize, char)` pair per scalar.
    offset: usize,
    line: usize,
    depth: usize,
}

impl<'a> ArrayParser<'a> {
    fn new(src: &'a str, line: usize) -> Self {
        ArrayParser {
            src,
            offset: 0,
            line,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.offset..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.src[self.offset..].chars().nth(offset)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek();
        if let Some(c) = c {
            self.offset += c.len_utf8();
        }
        c
    }

    fn byte_offset(&self) -> usize {
        self.offset
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.advance();
        }
    }

    fn err(&self) -> ParseError {
        ParseError::InvalidOperand {
            line: self.line,
            token: self.src.to_string(),
        }
    }

    fn parse_array(&mut self) -> Result<ArrayTemplate, ParseError> {
        if self.depth >= 64 {
            return Err(ParseError::ResourceLimit {
                resource: "assembler array nesting",
            });
        }
        self.depth += 1;
        let result = self.parse_array_bounded();
        self.depth -= 1;
        result
    }

    fn parse_array_bounded(&mut self) -> Result<ArrayTemplate, ParseError> {
        self.skip_ws();
        if self.advance() != Some('[') {
            return Err(self.err());
        }
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(ArrayTemplate::Array(items));
        }
        loop {
            items.push(self.parse_item()?);
            self.skip_ws();
            match self.advance() {
                Some(',') => self.skip_ws(),
                Some(']') => break,
                _ => return Err(self.err()),
            }
        }
        Ok(ArrayTemplate::Array(items))
    }

    fn parse_item(&mut self) -> Result<ArrayItem, ParseError> {
        self.skip_ws();
        let template = if self.peek() == Some('[') {
            self.parse_array()?
        } else {
            self.parse_scalar()?
        };
        self.skip_ws();
        let repeat = if self.try_consume_repeat_marker() {
            self.skip_ws();
            self.parse_number()?
        } else {
            1
        };
        Ok(ArrayItem { template, repeat })
    }

    fn try_consume_repeat_marker(&mut self) -> bool {
        if self.peek() == Some('x') {
            let after = self.peek_at(1);
            if after.is_none() || after.unwrap().is_whitespace() || after.unwrap().is_ascii_digit()
            {
                self.advance();
                return true;
            }
        }
        false
    }

    fn parse_number(&mut self) -> Result<usize, ParseError> {
        self.skip_ws();
        let start = self.byte_offset();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.advance();
        }
        let end = self.byte_offset();
        self.src[start..end]
            .parse::<usize>()
            .map_err(|_| self.err())
    }

    fn parse_scalar(&mut self) -> Result<ArrayTemplate, ParseError> {
        self.skip_ws();
        let start = self.byte_offset();
        if self.peek() == Some('"') {
            self.advance();
            let mut escape = false;
            loop {
                match self.advance() {
                    None => return Err(self.err()),
                    Some('\\') if !escape => escape = true,
                    Some('"') if !escape => break,
                    Some(_) => escape = false,
                }
            }
        } else {
            while let Some(c) = self.peek() {
                if c == ',' || c == ']' || c.is_whitespace() {
                    break;
                }
                self.advance();
            }
        }
        let end = self.byte_offset();
        let token = &self.src[start..end];
        if token.is_empty() {
            return Err(self.err());
        }
        parse_value(token, self.line).map(ArrayTemplate::Scalar)
    }
}

fn parse_array_literal(op: &str, line: usize) -> Result<ArrayTemplate, ParseError> {
    let mut p = ArrayParser::new(op, line);
    let template = p.parse_array()?;
    p.skip_ws();
    if p.offset != p.src.len() {
        return Err(p.err());
    }
    Ok(template)
}

fn resolve_address(
    token: &str,
    line: usize,
    labels: &std::collections::HashMap<String, usize>,
) -> Result<usize, ParseError> {
    // NOTE: numeric tokens are resolved as raw addresses
    if let Ok(n) = token.parse::<usize>() {
        return Ok(n);
    }
    labels
        .get(token)
        .copied()
        .ok_or_else(|| ParseError::UnknownLabel {
            line,
            name: token.to_string(),
        })
}

fn require_no_operand(r: &RawInstr, instr: Instruction) -> Result<Instruction, ParseError> {
    if r.operand.is_some() {
        Err(ParseError::UnexpectedOperand {
            line: r.line,
            instr: r.mnemonic.to_string(),
        })
    } else {
        Ok(instr)
    }
}

pub fn assemble(source: &str) -> Result<crate::backend::CompiledProgram, ParseError> {
    if source.len() > crate::limits::CompilerLimits::default().max_source_bytes {
        return Err(ParseError::ResourceLimit {
            resource: "assembler source byte",
        });
    }
    let mut labels: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut raw: Vec<RawInstr> = Vec::new();
    let mut declarations = Vec::new();
    let mut index = 0usize;
    let mut entries = 0usize;

    for (line_no, original) in source.lines().enumerate() {
        let line = line_no + 1;
        let stripped = strip_comment(original).trim();
        if stripped.is_empty() {
            continue;
        }
        entries += 1;
        if entries > crate::bytecode::LIMIT {
            return Err(ParseError::ResourceLimit {
                resource: "assembler entry",
            });
        }
        if let Some(declaration) = stripped.strip_prefix(".function ") {
            declarations.push((line, declaration));
            continue;
        }

        if let Some(name) = stripped.strip_suffix(':') {
            let name = name.trim();
            if name.is_empty() {
                return Err(ParseError::EmptyLabel { line });
            }
            if labels.insert(name.to_string(), index).is_some() {
                return Err(ParseError::DuplicateLabel {
                    line,
                    name: name.to_string(),
                });
            }
            continue;
        }

        let mut parts = stripped.splitn(2, char::is_whitespace);
        let mnemonic = parts.next().unwrap_or("");
        let operand = parts.next().map(|s| s.trim()).filter(|s| !s.is_empty());

        raw.push(RawInstr {
            line,
            mnemonic,
            operand,
        });
        index += 1;
    }

    let mut program = Vec::with_capacity(raw.len());
    let mut callables = std::collections::BTreeMap::new();
    for (line, declaration) in declarations {
        let parts: Vec<_> = declaration.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(ParseError::InvalidOperand {
                line,
                token: declaration.into(),
            });
        }
        let entry = resolve_address(parts[0], line, &labels)?;
        let signature = crate::backend::CallableSignature::dynamic(
            parse_index(parts[1], line)?,
            parse_index(parts[2], line)?,
        );
        if callables.insert(entry, signature).is_some() {
            return Err(ParseError::DuplicateLabel {
                line,
                name: parts[0].into(),
            });
        }
    }
    let mut imports: Vec<crate::runtime::NativeImport> = Vec::new();
    for r in raw {
        let instr = match r.mnemonic.to_ascii_lowercase().as_str() {
            "push" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                if let Some(label) = op.strip_prefix('@') {
                    Instruction::Push(Value::Function(resolve_address(label, r.line, &labels)?))
                } else if op.starts_with('[') {
                    Instruction::PushArray(parse_array_literal(op, r.line)?)
                } else {
                    Instruction::Push(parse_value(op, r.line)?)
                }
            }
            "store" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::Store(parse_index(op, r.line)?)
            }
            "load" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::Load(parse_index(op, r.line)?)
            }
            "store_global" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::StoreGlobal(parse_index(op, r.line)?)
            }
            "load_global" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::LoadGlobal(parse_index(op, r.line)?)
            }
            "jmp" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::Jump(resolve_address(op, r.line, &labels)?)
            }
            "jz" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::JumpIfFalse(resolve_address(op, r.line, &labels)?)
            }
            "jnz" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::JumpIfTrue(resolve_address(op, r.line, &labels)?)
            }
            "call" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                let (target, count) = op.split_once(char::is_whitespace).ok_or_else(|| {
                    ParseError::InvalidOperand {
                        line: r.line,
                        token: op.to_string(),
                    }
                })?;
                Instruction::Call(
                    resolve_address(target, r.line, &labels)?,
                    parse_index(count.trim(), r.line)?,
                )
            }
            "ret" => require_no_operand(&r, Instruction::Return)?,
            "is_type" => {
                use crate::instruction::ValueType;
                let operand = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_owned(),
                })?;
                let ty = if operand == "error" {
                    ValueType::Error
                } else if let Some(ty) = ValueType::primitive(operand) {
                    ty
                } else if let Some(name) = operand
                    .strip_prefix("enum ")
                    .filter(|name| !name.is_empty())
                {
                    ValueType::Enum(name.into())
                } else if let Some(name) = operand
                    .strip_prefix("variant ")
                    .filter(|name| !name.is_empty())
                {
                    ValueType::Variant(name.into())
                } else {
                    return Err(ParseError::InvalidOperand {
                        line: r.line,
                        token: operand.to_owned(),
                    });
                };
                Instruction::IsType(ty)
            }
            "error_kind" => require_no_operand(&r, Instruction::ErrorKind)?,
            "add" => require_no_operand(&r, Instruction::Add)?,
            "arrpush" => require_no_operand(&r, Instruction::ArrayPush)?,
            "arrprepend" => require_no_operand(&r, Instruction::ArrayPrepend)?,
            "sub" => require_no_operand(&r, Instruction::Sub)?,
            "mul" => require_no_operand(&r, Instruction::Mul)?,
            "div" => require_no_operand(&r, Instruction::Div)?,
            "mod" => require_no_operand(&r, Instruction::Mod)?,
            "eq" => require_no_operand(&r, Instruction::Eq)?,
            "ne" => require_no_operand(&r, Instruction::Ne)?,
            "lt" => require_no_operand(&r, Instruction::Lt)?,
            "le" => require_no_operand(&r, Instruction::Le)?,
            "gt" => require_no_operand(&r, Instruction::Gt)?,
            "ge" => require_no_operand(&r, Instruction::Ge)?,
            "call_native" => {
                let name = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.into(),
                })?;
                let import = crate::runtime::NativeImport::named(name).map_err(|_| {
                    ParseError::InvalidOperand {
                        line: r.line,
                        token: name.into(),
                    }
                })?;
                let index = imports
                    .iter()
                    .position(|item| item.name() == name)
                    .unwrap_or_else(|| {
                        imports.push(import);
                        imports.len() - 1
                    });
                Instruction::CallNative(index)
            }
            "halt" => require_no_operand(&r, Instruction::Halt)?,
            "and" => require_no_operand(&r, Instruction::And)?,
            "or" => require_no_operand(&r, Instruction::Or)?,
            "xor" => require_no_operand(&r, Instruction::Xor)?,
            "not" => require_no_operand(&r, Instruction::Not)?,
            "len" => require_no_operand(&r, Instruction::Len)?,
            "exclusive" => require_no_operand(&r, Instruction::Exclusive)?,
            "dup" => require_no_operand(&r, Instruction::Dup)?,
            "pop" => require_no_operand(&r, Instruction::Pop)?,
            "swap" => require_no_operand(&r, Instruction::Swap)?,
            "callv" | "array" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_owned(),
                })?;
                let n = parse_index(op, r.line)?;
                if r.mnemonic.eq_ignore_ascii_case("callv") {
                    Instruction::CallValue(n)
                } else {
                    Instruction::MakeArray(n)
                }
            }
            "index" => require_no_operand(&r, Instruction::Index)?,
            "indexset" => require_no_operand(&r, Instruction::IndexSet)?,
            "cell" => require_no_operand(&r, Instruction::MakeCell)?,
            "cellget" => require_no_operand(&r, Instruction::CellGet)?,
            "cellset" => require_no_operand(&r, Instruction::CellSet)?,
            "record" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::MakeRecord(parse_index(op, r.line)?)
            }
            "loadcap" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                Instruction::LoadCapture(parse_index(op, r.line)?)
            }
            "crash" => match r.operand {
                Some(op) => Instruction::Crash(Some(parse_string_literal(op, r.line)?)),
                None => Instruction::Crash(None),
            },
            "closure" => {
                let op = r.operand.ok_or_else(|| ParseError::MissingOperand {
                    line: r.line,
                    instr: r.mnemonic.to_string(),
                })?;
                let mut tokens = op.split_whitespace();
                let label = tokens
                    .next()
                    .and_then(|t| t.strip_prefix('@'))
                    .ok_or_else(|| ParseError::InvalidOperand {
                        line: r.line,
                        token: op.to_string(),
                    })?;
                let count_tok = tokens.next().ok_or_else(|| ParseError::InvalidOperand {
                    line: r.line,
                    token: op.to_string(),
                })?;
                if tokens.next().is_some() {
                    return Err(ParseError::InvalidOperand {
                        line: r.line,
                        token: op.to_string(),
                    });
                }
                let addr = resolve_address(label, r.line, &labels)?;
                let count = parse_index(count_tok, r.line)?;
                Instruction::MakeClosure(addr, count)
            }
            other => {
                return Err(ParseError::UnknownInstruction {
                    line: r.line,
                    text: other.to_string(),
                });
            }
        };
        if let Instruction::Call(entry, parameters) = &instr {
            callables
                .entry(*entry)
                .or_insert_with(|| crate::backend::CallableSignature::dynamic(*parameters, 0));
        }
        program.push(instr);
    }

    let mut order: Vec<usize> = (0..imports.len()).collect();
    order.sort_by_key(|index| imports[*index].name());
    let mut remap = vec![0; order.len()];
    let imports = order
        .iter()
        .enumerate()
        .map(|(new, old)| {
            remap[*old] = new;
            imports[*old].clone()
        })
        .collect();
    for instruction in &mut program {
        if let Instruction::CallNative(index) = instruction {
            *index = remap[*index];
        }
    }
    Ok(crate::backend::CompiledProgram {
        instructions: program,
        imports,
        callables,
        ..Default::default()
    })
}
