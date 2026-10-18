//! Versioned portable bytecode, distinct from the in-memory Value graph.
use crate::backend::{CallableSignature, CompiledProgram, SourceMapEntry};
use crate::instruction::{Instruction, ValueType};
use crate::runtime::{NativeImport, NativeType};
use crate::template::{ArrayItem, ArrayTemplate};
use crate::value::{RuntimeType, Value};
use std::rc::Rc;

const MAGIC: &[u8; 4] = b"COG\0";
const VERSION: u16 = 10;
pub(crate) const LIMIT: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
pub const MAX_FILE_BYTES: usize = 128 * 1024 * 1024;
const MAX_STATIC_STRING_BYTES: usize = 64 * 1024 * 1024;

type Result<T> = std::result::Result<T, String>;

struct ConstantBudget {
    nodes: usize,
    strings: usize,
}

impl Default for ConstantBudget {
    fn default() -> Self {
        Self {
            nodes: LIMIT,
            strings: MAX_STATIC_STRING_BYTES,
        }
    }
}

impl ConstantBudget {
    fn node(&mut self, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err("bytecode constant/type nesting is too deep".into());
        }
        self.nodes = self
            .nodes
            .checked_sub(1)
            .ok_or("bytecode constant/type node limit exceeded")?;
        Ok(())
    }

    fn string(&mut self, bytes: usize) -> Result<()> {
        self.strings = self
            .strings
            .checked_sub(bytes)
            .ok_or("bytecode static string limit exceeded")?;
        Ok(())
    }

    fn value(&mut self, value: &Value) -> Result<()> {
        self.node(0)?;
        match value {
            Value::Str(text) | Value::TypeTag(text) => self.string(text.len()),
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Nil | Value::Function(_) => {
                Ok(())
            }
            _ => Err("runtime objects cannot be stored as bytecode constants".into()),
        }
    }

    fn template(&mut self, template: &ArrayTemplate, depth: usize) -> Result<()> {
        self.node(depth)?;
        match template {
            ArrayTemplate::Scalar(value) => self.value(value),
            ArrayTemplate::Array(items) => {
                for item in items {
                    if item.repeat > LIMIT {
                        return Err("array template repeat exceeds the format limit".into());
                    }
                    self.template(&item.template, depth + 1)?;
                }
                Ok(())
            }
        }
    }

    fn runtime_type(&mut self, ty: &RuntimeType, depth: usize) -> Result<()> {
        self.node(depth)?;
        match ty {
            RuntimeType::Error(name) | RuntimeType::Variant(name) | RuntimeType::Nominal(name) => {
                self.string(name.len())
            }
            RuntimeType::Array(inner) | RuntimeType::Nullable(inner) => {
                self.runtime_type(inner, depth + 1)
            }
            RuntimeType::Tuple(items) => {
                if items.len() > 1024 {
                    return Err("runtime tuple type is too large".into());
                }
                for item in items {
                    self.runtime_type(item, depth + 1)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Read an artifact without allocating from an untrusted file size or reading
/// an unbounded stream. All callers, including cache reuse, share this boundary.
pub fn read(mut input: impl std::io::Read) -> Result<CompiledProgram> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("cannot read bytecode: {error}")),
        };
        if count > MAX_FILE_BYTES - bytes.len() {
            return Err("bytecode file exceeds the format limit".into());
        }
        bytes
            .try_reserve(count)
            .map_err(|_| "cannot allocate bytecode input")?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    decode(&bytes)
}

pub fn encode(program: &CompiledProgram) -> Result<Vec<u8>> {
    validate(program)?;
    let mut w = Writer(Vec::new());
    w.data(MAGIC)?;
    w.data(&VERSION.to_le_bytes())?;
    w.data(&0u16.to_le_bytes())?;
    w.index(program.imports.len())?;
    for import in &program.imports {
        w.string(import.name())?;
        w.data(&import.version().to_le_bytes())?;
        w.index(import.parameters().len())?;
        for parameter in import.parameters() {
            w.byte(type_tag(*parameter))?;
        }
        w.byte(type_tag(import.result()))?;
    }
    w.index(program.callables.len())?;
    for (entry, signature) in &program.callables {
        w.index(*entry)?;
        w.index(signature.parameters)?;
        w.index(signature.captures)?;
        match &signature.parameter_types {
            Some(parameter_types) => {
                w.byte(1)?;
                for parameter in parameter_types {
                    w.optional_runtime_type(parameter.as_ref(), 0)?;
                }
            }
            None => w.byte(0)?,
        }
        w.optional_runtime_type(signature.result_type.as_ref(), 0)?;
    }
    w.index(program.instructions.len())?;
    for instruction in &program.instructions {
        match instruction {
            Instruction::CallNative(index) => {
                w.byte(0)?;
                w.index(*index)?;
            }
            Instruction::Add => w.byte(2)?,
            Instruction::Mul => w.byte(3)?,
            Instruction::Div => w.byte(4)?,
            Instruction::Mod => w.byte(5)?,
            Instruction::Sub => w.byte(6)?,
            Instruction::Eq => w.byte(7)?,
            Instruction::Ne => w.byte(8)?,
            Instruction::Lt => w.byte(9)?,
            Instruction::Le => w.byte(10)?,
            Instruction::Gt => w.byte(11)?,
            Instruction::Ge => w.byte(12)?,
            Instruction::Halt => w.byte(18)?,
            Instruction::Return => w.byte(19)?,
            Instruction::IsType(ty) => {
                w.byte(63)?;
                match ty {
                    ValueType::Int => w.byte(0)?,
                    ValueType::Float => w.byte(1)?,
                    ValueType::Bool => w.byte(2)?,
                    ValueType::Str => w.byte(3)?,
                    ValueType::Nil => w.byte(4)?,
                    ValueType::Error => w.byte(5)?,
                    ValueType::Enum(name) => {
                        w.byte(6)?;
                        w.string(name)?;
                    }
                    ValueType::Variant(name) => {
                        w.byte(7)?;
                        w.string(name)?;
                    }
                }
            }
            Instruction::ErrorKind => w.byte(21)?,
            Instruction::And => w.byte(26)?,
            Instruction::Or => w.byte(27)?,
            Instruction::Xor => w.byte(28)?,
            Instruction::Not => w.byte(29)?,
            Instruction::Len => w.byte(33)?,
            Instruction::Exclusive => w.byte(34)?,
            Instruction::Dup => w.byte(35)?,
            Instruction::Pop => w.byte(36)?,
            Instruction::Swap => w.byte(37)?,
            Instruction::Index => w.byte(38)?,
            Instruction::IndexSet => w.byte(39)?,
            Instruction::MakeCell => w.byte(40)?,
            Instruction::CellGet => w.byte(41)?,
            Instruction::CellSet => w.byte(42)?,
            Instruction::ArrayPush => w.byte(43)?,
            Instruction::ArrayPrepend => w.byte(44)?,
            Instruction::Push(value) => {
                w.byte(46)?;
                w.value(value)?;
            }
            Instruction::PushArray(value) => {
                w.byte(47)?;
                w.template(value, 0)?;
            }
            Instruction::Load(value) => {
                w.byte(48)?;
                w.index(*value)?;
            }
            Instruction::Store(value) => {
                w.byte(49)?;
                w.index(*value)?;
            }
            Instruction::LoadGlobal(value) => {
                w.byte(50)?;
                w.index(*value)?;
            }
            Instruction::StoreGlobal(value) => {
                w.byte(51)?;
                w.index(*value)?;
            }
            Instruction::Jump(value) => {
                w.byte(52)?;
                w.index(*value)?;
            }
            Instruction::JumpIfFalse(value) => {
                w.byte(53)?;
                w.index(*value)?;
            }
            Instruction::JumpIfTrue(value) => {
                w.byte(54)?;
                w.index(*value)?;
            }
            Instruction::Call(a, b) => {
                w.byte(55)?;
                w.index(*a)?;
                w.index(*b)?;
            }
            Instruction::CallValue(value) => {
                w.byte(56)?;
                w.index(*value)?;
            }
            Instruction::MakeRecord(value) => {
                w.byte(57)?;
                w.index(*value)?;
            }
            Instruction::MakeArray(value) => {
                w.byte(58)?;
                w.index(*value)?;
            }
            Instruction::MakeTuple(value) => {
                w.byte(65)?;
                w.index(*value)?;
            }
            Instruction::MakeTypedArray(count, ty) => {
                w.byte(66)?;
                w.index(*count)?;
                w.runtime_type(ty, 0)?;
            }
            Instruction::ConstrainArrayType(ty) => {
                w.byte(67)?;
                w.runtime_type(ty, 0)?;
            }
            Instruction::ConstrainArrayFrom => w.byte(68)?,
            Instruction::ArrayEmptyLike => w.byte(69)?,
            Instruction::MakeClosure(a, b) => {
                w.byte(59)?;
                w.index(*a)?;
                w.index(*b)?;
            }
            Instruction::LoadCapture(value) => {
                w.byte(60)?;
                w.index(*value)?;
            }
            Instruction::Crash(s) => {
                w.byte(61)?;
                w.byte(u8::from(s.is_some()))?;
                if let Some(s) = s {
                    w.string(s)?;
                }
            }
            Instruction::StackTruncate(n) => {
                w.byte(62)?;
                w.index(*n)?;
            }
        }
    }
    w.index(program.source_map.len())?;
    for entry in &program.source_map {
        w.index(entry.start)?;
        w.index(entry.end)?;
        w.string(&entry.location.file)?;
        w.index(entry.location.line)?;
        w.index(entry.location.column)?;
    }
    if w.0.len() > MAX_FILE_BYTES {
        return Err("bytecode file exceeds the format limit".into());
    }
    Ok(w.0)
}

pub fn decode(bytes: &[u8]) -> Result<CompiledProgram> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err("bytecode file exceeds the format limit".into());
    }
    let mut r = Reader {
        bytes,
        pos: 0,
        budget: ConstantBudget::default(),
    };
    if r.take(4)? != MAGIC {
        return Err("invalid bytecode magic; expected a .cogb file".into());
    }
    let version = u16::from_le_bytes(r.take(2)?.try_into().unwrap());
    if version != VERSION {
        return Err(format!(
            "unsupported bytecode version {version}; expected {VERSION}"
        ));
    }
    if r.take(2)? != [0, 0] {
        return Err("unsupported bytecode flags".into());
    }
    let import_count = r.index()?;
    if import_count > 4096 || import_count > (bytes.len() - r.pos) / 11 {
        return Err("invalid runtime import count".into());
    }
    let mut imports = Vec::new();
    imports
        .try_reserve_exact(import_count)
        .map_err(|_| "cannot allocate runtime import table")?;
    for _ in 0..import_count {
        let name = r.string()?;
        let version = u16::from_le_bytes(r.take(2)?.try_into().unwrap());
        let arity = r.index()?;
        if arity > 64 {
            return Err("invalid runtime import arity".into());
        }
        let mut parameters = Vec::new();
        parameters
            .try_reserve_exact(arity)
            .map_err(|_| "cannot allocate native parameters")?;
        for _ in 0..arity {
            parameters.push(read_type(r.byte()?)?);
        }
        let result = read_type(r.byte()?)?;
        imports.push(NativeImport::resolve(&name, version, &parameters, result)?);
    }
    let callable_count = r.index()?;
    if callable_count > LIMIT || callable_count > (bytes.len() - r.pos) / 14 {
        return Err("invalid callable count".into());
    }
    let mut callables = std::collections::BTreeMap::new();
    let mut previous = None;
    let mut callable_parameter_type_slots = 0usize;
    for _ in 0..callable_count {
        let entry = r.index()?;
        if previous.is_some_and(|previous| previous >= entry) {
            return Err("callable entries must be unique and sorted".into());
        }
        previous = Some(entry);
        let parameters = r.index()?;
        let captures = r.index()?;
        if parameters > LIMIT || captures > LIMIT {
            return Err("invalid callable descriptor size".into());
        }
        let parameter_types = match r.byte()? {
            0 => None,
            1 => {
                callable_parameter_type_slots = callable_parameter_type_slots
                    .checked_add(parameters)
                    .filter(|slots| *slots <= LIMIT)
                    .ok_or("callable parameter type slot limit exceeded")?;
                let remaining = r.bytes.len().saturating_sub(r.pos);
                if remaining == 0 || parameters > remaining - 1 {
                    return Err("invalid callable descriptor size".into());
                }
                let mut types = Vec::new();
                types
                    .try_reserve_exact(parameters)
                    .map_err(|_| "cannot allocate callable parameter types")?;
                for _ in 0..parameters {
                    types.push(r.optional_runtime_type(0)?);
                }
                if !types.iter().any(Option::is_some) {
                    return Err("noncanonical all-dynamic callable parameter types".into());
                }
                Some(types)
            }
            _ => return Err("invalid callable parameter type presence tag".into()),
        };
        callables.insert(
            entry,
            CallableSignature {
                parameters,
                captures,
                parameter_types,
                result_type: r.optional_runtime_type(0)?,
            },
        );
    }
    let count = r.index()?;
    if count > LIMIT || count > r.bytes.len() - r.pos {
        return Err("truncated bytecode instruction table".into());
    }
    let mut program = Vec::new();
    program
        .try_reserve_exact(count)
        .map_err(|_| "cannot allocate bytecode instruction table")?;
    for _ in 0..count {
        let instruction = match r.byte()? {
            0 => Instruction::CallNative(r.index()?),
            2 => Instruction::Add,
            3 => Instruction::Mul,
            4 => Instruction::Div,
            5 => Instruction::Mod,
            6 => Instruction::Sub,
            7 => Instruction::Eq,
            8 => Instruction::Ne,
            9 => Instruction::Lt,
            10 => Instruction::Le,
            11 => Instruction::Gt,
            12 => Instruction::Ge,
            18 => Instruction::Halt,
            19 => Instruction::Return,
            63 => Instruction::IsType(match r.byte()? {
                0 => ValueType::Int,
                1 => ValueType::Float,
                2 => ValueType::Bool,
                3 => ValueType::Str,
                4 => ValueType::Nil,
                5 => ValueType::Error,
                6 => ValueType::Enum(r.string()?),
                7 => ValueType::Variant(r.string()?),
                tag => return Err(format!("unknown type-test tag {tag}")),
            }),
            21 => Instruction::ErrorKind,
            26 => Instruction::And,
            27 => Instruction::Or,
            28 => Instruction::Xor,
            29 => Instruction::Not,
            33 => Instruction::Len,
            34 => Instruction::Exclusive,
            35 => Instruction::Dup,
            36 => Instruction::Pop,
            37 => Instruction::Swap,
            38 => Instruction::Index,
            39 => Instruction::IndexSet,
            40 => Instruction::MakeCell,
            41 => Instruction::CellGet,
            42 => Instruction::CellSet,
            43 => Instruction::ArrayPush,
            44 => Instruction::ArrayPrepend,
            46 => Instruction::Push(r.value()?),
            47 => Instruction::PushArray(r.template(0)?),
            48 => Instruction::Load(r.index()?),
            49 => Instruction::Store(r.index()?),
            50 => Instruction::LoadGlobal(r.index()?),
            51 => Instruction::StoreGlobal(r.index()?),
            52 => Instruction::Jump(r.index()?),
            53 => Instruction::JumpIfFalse(r.index()?),
            54 => Instruction::JumpIfTrue(r.index()?),
            55 => Instruction::Call(r.index()?, r.index()?),
            56 => Instruction::CallValue(r.index()?),
            57 => Instruction::MakeRecord(r.index()?),
            58 => Instruction::MakeArray(r.index()?),
            65 => Instruction::MakeTuple(r.index()?),
            66 => Instruction::MakeTypedArray(r.index()?, r.runtime_type(0)?),
            67 => Instruction::ConstrainArrayType(r.runtime_type(0)?),
            68 => Instruction::ConstrainArrayFrom,
            69 => Instruction::ArrayEmptyLike,
            59 => Instruction::MakeClosure(r.index()?, r.index()?),
            60 => Instruction::LoadCapture(r.index()?),
            62 => Instruction::StackTruncate(r.index()?),
            61 => Instruction::Crash(match r.byte()? {
                0 => None,
                1 => Some(r.string()?),
                _ => return Err("invalid optional string tag".into()),
            }),
            tag => return Err(format!("unknown bytecode opcode {tag}")),
        };
        program.push(instruction);
    }
    let source_map_count = r.index()?;
    if source_map_count > count {
        return Err("invalid source map entry count".into());
    }
    let mut source_map = Vec::new();
    source_map
        .try_reserve_exact(source_map_count)
        .map_err(|_| "cannot allocate bytecode source map")?;
    for _ in 0..source_map_count {
        source_map.push(SourceMapEntry {
            start: r.index()?,
            end: r.index()?,
            location: crate::diagnostic::SourceLocation {
                file: r.owned_string()?,
                line: r.index()?,
                column: r.index()?,
            },
        });
    }
    if r.pos != bytes.len() {
        return Err("unexpected trailing bytes in bytecode".into());
    }
    let program = CompiledProgram {
        instructions: program,
        imports,
        callables,
        source_map,
        ..CompiledProgram::default()
    };
    validate(&program)?;
    Ok(program)
}

pub fn validate(artifact: &CompiledProgram) -> Result<()> {
    let mut budget = ConstantBudget::default();
    if artifact.imports.len() > 4096 {
        return Err("invalid runtime import count".into());
    }
    for pair in artifact.imports.windows(2) {
        if pair[0].name() >= pair[1].name() {
            return Err("runtime imports must be unique and sorted by identity".into());
        }
    }
    for import in &artifact.imports {
        budget.string(import.name().len())?;
    }
    if artifact.callables.len() > LIMIT {
        return Err("invalid callable count".into());
    }
    let mut callable_parameter_type_slots = 0usize;
    for (entry, signature) in &artifact.callables {
        if *entry >= artifact.instructions.len()
            || signature.parameters > LIMIT
            || signature.captures > LIMIT
            || signature.parameter_types.as_ref().is_some_and(|types| {
                types.len() != signature.parameters || !types.iter().any(Option::is_some)
            })
        {
            return Err("invalid callable descriptor".into());
        }
        if let Some(parameter_types) = &signature.parameter_types {
            callable_parameter_type_slots = callable_parameter_type_slots
                .checked_add(parameter_types.len())
                .filter(|slots| *slots <= LIMIT)
                .ok_or("callable parameter type slot limit exceeded")?;
            for ty in parameter_types.iter().flatten() {
                validate_runtime_type(ty, 0)?;
                budget.runtime_type(ty, 0)?;
            }
        }
        if let Some(ty) = &signature.result_type {
            validate_runtime_type(ty, 0)?;
            budget.runtime_type(ty, 0)?;
        }
    }
    let program = &artifact.instructions;
    if program.is_empty() || program.len() > LIMIT {
        return Err("invalid bytecode instruction count".into());
    }
    if artifact.source_map.len() > program.len() {
        return Err("invalid source map entry count".into());
    }
    let mut previous_end = 0;
    let mut previous_entry: Option<&SourceMapEntry> = None;
    for entry in &artifact.source_map {
        if entry.start < previous_end
            || entry.start >= entry.end
            || entry.end > program.len()
            || entry.location.line == 0
            || entry.location.column == 0
        {
            return Err("invalid or noncanonical source map range".into());
        }
        if previous_entry.is_some_and(|previous| {
            previous.end == entry.start && previous.location == entry.location
        }) {
            return Err("adjacent identical source map ranges are not canonical".into());
        }
        budget.string(entry.location.file.len())?;
        previous_end = entry.end;
        previous_entry = Some(entry);
    }
    for instruction in program {
        match instruction {
            Instruction::Push(value) => budget.value(value)?,
            Instruction::PushArray(template) => budget.template(template, 0)?,
            Instruction::MakeTypedArray(_, ty) | Instruction::ConstrainArrayType(ty) => {
                budget.runtime_type(ty, 0)?;
                validate_runtime_type(ty, 0)?;
            }
            Instruction::IsType(ValueType::Enum(name) | ValueType::Variant(name))
            | Instruction::Crash(Some(name)) => budget.string(name.len())?,
            _ => {}
        }
        let target = match instruction {
            Instruction::Jump(n)
            | Instruction::JumpIfFalse(n)
            | Instruction::JumpIfTrue(n)
            | Instruction::Call(n, _)
            | Instruction::MakeClosure(n, _)
            | Instruction::Push(Value::Function(n)) => Some(*n),
            _ => None,
        };
        if let Some(target) = target
            && target >= program.len()
        {
            return Err(format!(
                "bytecode target {target} is outside the instruction table"
            ));
        }
        if let Instruction::PushArray(template) = instruction {
            validate_template(template, program.len(), 0)?;
            template_size(template)?;
        }
        match instruction {
            Instruction::IsType(ValueType::Enum(name)) if name.is_empty() => {
                return Err("empty enum identity in type test".into());
            }
            Instruction::IsType(ValueType::Variant(name))
                if !name
                    .rsplit_once("::")
                    .is_some_and(|(parent, item)| !parent.is_empty() && !item.is_empty()) =>
            {
                return Err("invalid variant identity in type test".into());
            }
            Instruction::CallNative(index) if *index >= artifact.imports.len() => {
                return Err(format!(
                    "native import index {index} is outside the import table"
                ));
            }
            Instruction::Load(n)
            | Instruction::Store(n)
            | Instruction::LoadGlobal(n)
            | Instruction::StoreGlobal(n)
            | Instruction::LoadCapture(n)
            | Instruction::StackTruncate(n)
                if *n >= LIMIT =>
            {
                return Err(format!("bytecode slot {n} exceeds the format limit"));
            }
            Instruction::Call(_, n)
            | Instruction::CallValue(n)
            | Instruction::MakeClosure(_, n)
            | Instruction::MakeArray(n)
            | Instruction::MakeTuple(n)
            | Instruction::MakeTypedArray(n, _)
            | Instruction::MakeRecord(n)
                if *n > LIMIT =>
            {
                return Err("bytecode operand count exceeds the format limit".into());
            }
            _ => {}
        }
    }
    crate::verifier::verify(artifact)
}

fn validate_template(template: &ArrayTemplate, instructions: usize, depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err("bytecode constant nesting is too deep".into());
    }
    match template {
        ArrayTemplate::Scalar(Value::Function(n)) if *n >= instructions => {
            return Err("array constant function target is outside the instruction table".into());
        }
        ArrayTemplate::Array(items) => {
            for item in items {
                validate_template(&item.template, instructions, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_runtime_type(ty: &RuntimeType, depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err("runtime type nesting is too deep".into());
    }
    match ty {
        RuntimeType::Error(name) | RuntimeType::Nominal(name) if name.is_empty() => {
            Err("empty identity in runtime type descriptor".into())
        }
        RuntimeType::Variant(name) if name.is_empty() => {
            Err("empty variant identity in runtime type descriptor".into())
        }
        RuntimeType::Array(inner) | RuntimeType::Nullable(inner) => {
            validate_runtime_type(inner, depth + 1)
        }
        RuntimeType::Tuple(items) => {
            if items.len() > 1024 {
                return Err("runtime tuple type is too large".into());
            }
            for item in items {
                validate_runtime_type(item, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

struct Writer(Vec<u8>);
impl Writer {
    fn data(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_FILE_BYTES - self.0.len() {
            return Err("bytecode file exceeds the format limit".into());
        }
        self.0
            .try_reserve(bytes.len())
            .map_err(|_| "cannot allocate bytecode output")?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn byte(&mut self, value: u8) -> Result<()> {
        self.data(&[value])
    }
    fn index(&mut self, value: usize) -> Result<()> {
        let n = u32::try_from(value).map_err(|_| "bytecode operand does not fit u32")?;
        self.data(&n.to_le_bytes())?;
        Ok(())
    }
    fn string(&mut self, value: &str) -> Result<()> {
        self.index(value.len())?;
        self.data(value.as_bytes())?;
        Ok(())
    }
    fn value(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Int(n) => {
                self.byte(0)?;
                self.data(&n.to_le_bytes())?;
            }
            Value::Float(n) => {
                self.byte(1)?;
                self.data(&n.to_le_bytes())?;
            }
            Value::Bool(b) => {
                self.byte(2)?;
                self.byte(u8::from(*b))?;
            }
            Value::Nil => self.byte(3)?,
            Value::Str(s) => {
                self.byte(4)?;
                self.string(s)?;
            }
            Value::Function(n) => {
                self.byte(5)?;
                self.index(*n)?;
            }
            Value::TypeTag(tag) => {
                self.byte(6)?;
                self.string(tag)?;
            }
            _ => return Err("runtime objects cannot be stored as bytecode constants".into()),
        }
        Ok(())
    }
    fn template(&mut self, template: &ArrayTemplate, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err("bytecode constant nesting is too deep".into());
        }
        match template {
            ArrayTemplate::Scalar(value) => {
                self.byte(0)?;
                self.value(value)?;
            }
            ArrayTemplate::Array(items) => {
                self.byte(1)?;
                self.index(items.len())?;
                for item in items {
                    self.index(item.repeat)?;
                    self.template(&item.template, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    fn runtime_type(&mut self, ty: &RuntimeType, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err("runtime type nesting is too deep".into());
        }
        let tag = match ty {
            RuntimeType::Int => 0,
            RuntimeType::Float => 1,
            RuntimeType::Bool => 2,
            RuntimeType::Nil => 3,
            RuntimeType::Str => 4,
            RuntimeType::Error(_) => 5,
            RuntimeType::Array(_) => 6,
            RuntimeType::Record => 7,
            RuntimeType::Variant(_) => 8,
            RuntimeType::Function => 9,
            RuntimeType::Other => 10,
            RuntimeType::Nominal(_) => 11,
            RuntimeType::Nullable(_) => 12,
            RuntimeType::Tuple(_) => 13,
        };
        self.byte(tag)?;
        match ty {
            RuntimeType::Error(name) | RuntimeType::Variant(name) | RuntimeType::Nominal(name) => {
                self.string(name)?
            }
            RuntimeType::Array(inner) | RuntimeType::Nullable(inner) => {
                self.runtime_type(inner, depth + 1)?
            }
            RuntimeType::Tuple(items) => {
                if items.len() > 1024 {
                    return Err("runtime tuple type is too large".into());
                }
                self.index(items.len())?;
                for item in items {
                    self.runtime_type(item, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn optional_runtime_type(&mut self, ty: Option<&RuntimeType>, depth: usize) -> Result<()> {
        match ty {
            Some(ty) => {
                self.byte(1)?;
                self.runtime_type(ty, depth)
            }
            None => self.byte(0),
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    budget: ConstantBudget,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("truncated bytecode")?;
        let result = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(result)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn index(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn string_slice(&mut self) -> Result<&'a str> {
        let count = self.index()?;
        self.budget.string(count)?;
        let bytes = self.take(count)?;
        std::str::from_utf8(bytes).map_err(|_| "invalid UTF-8 in bytecode string".into())
    }
    fn string(&mut self) -> Result<Rc<str>> {
        Ok(Rc::from(self.string_slice()?))
    }
    fn string_buffer(capacity: usize) -> Result<String> {
        let mut buffer = String::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(|_| "cannot allocate bytecode string")?;
        Ok(buffer)
    }
    fn owned_string(&mut self) -> Result<String> {
        let text = self.string_slice()?;
        let mut owned = Self::string_buffer(text.len())?;
        owned.push_str(text);
        Ok(owned)
    }
    fn value(&mut self) -> Result<Value> {
        self.budget.node(0)?;
        Ok(match self.byte()? {
            0 => Value::Int(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            1 => Value::Float(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            2 => Value::Bool(match self.byte()? {
                0 => false,
                1 => true,
                _ => return Err("invalid bytecode boolean".into()),
            }),
            3 => Value::Nil,
            4 => Value::Str(self.string()?),
            5 => Value::Function(self.index()?),
            6 => Value::TypeTag(self.string()?),
            _ => return Err("unknown bytecode constant tag".into()),
        })
    }
    fn runtime_type(&mut self, depth: usize) -> Result<RuntimeType> {
        self.budget.node(depth)?;
        if depth >= MAX_DEPTH {
            return Err("runtime type nesting is too deep".into());
        }
        Ok(match self.byte()? {
            0 => RuntimeType::Int,
            1 => RuntimeType::Float,
            2 => RuntimeType::Bool,
            3 => RuntimeType::Nil,
            4 => RuntimeType::Str,
            5 => RuntimeType::Error(self.owned_string()?),
            6 => RuntimeType::Array(Box::new(self.runtime_type(depth + 1)?)),
            7 => RuntimeType::Record,
            8 => RuntimeType::Variant(self.owned_string()?),
            9 => RuntimeType::Function,
            10 => RuntimeType::Other,
            11 => RuntimeType::Nominal(self.owned_string()?),
            12 => RuntimeType::Nullable(Box::new(self.runtime_type(depth + 1)?)),
            13 => {
                let count = self.index()?;
                if count > 1024 {
                    return Err("runtime tuple type is too large".into());
                }
                let mut items = Vec::new();
                items
                    .try_reserve_exact(count)
                    .map_err(|_| "cannot allocate tuple type")?;
                for _ in 0..count {
                    items.push(self.runtime_type(depth + 1)?);
                }
                RuntimeType::Tuple(items)
            }
            tag => return Err(format!("invalid runtime type tag {tag}")),
        })
    }
    fn optional_runtime_type(&mut self, depth: usize) -> Result<Option<RuntimeType>> {
        match self.byte()? {
            0 => Ok(None),
            1 => Ok(Some(self.runtime_type(depth)?)),
            _ => Err("invalid optional runtime type tag".into()),
        }
    }
    fn template(&mut self, depth: usize) -> Result<ArrayTemplate> {
        self.budget.node(depth)?;
        if depth >= MAX_DEPTH {
            return Err("bytecode constant nesting is too deep".into());
        }
        match self.byte()? {
            0 => Ok(ArrayTemplate::Scalar(self.value()?)),
            1 => {
                let count = self.index()?;
                if count > LIMIT || count > (self.bytes.len() - self.pos) / 5 {
                    return Err("invalid array template size".into());
                }
                let mut items = Vec::new();
                items
                    .try_reserve_exact(count)
                    .map_err(|_| "cannot allocate array template")?;
                for _ in 0..count {
                    let repeat = self.index()?;
                    if repeat > LIMIT {
                        return Err("array template repeat exceeds the format limit".into());
                    }
                    items.push(ArrayItem {
                        repeat,
                        template: self.template(depth + 1)?,
                    });
                }
                let template = ArrayTemplate::Array(items);
                template_size(&template)?;
                Ok(template)
            }
            _ => Err("unknown array template tag".into()),
        }
    }
}

fn template_size(template: &ArrayTemplate) -> Result<usize> {
    match template {
        ArrayTemplate::Scalar(_) => Ok(1),
        ArrayTemplate::Array(items) => {
            let mut total = 1usize;
            for item in items {
                total = template_size(&item.template)?
                    .checked_mul(item.repeat)
                    .and_then(|n| total.checked_add(n))
                    .filter(|n| *n <= LIMIT)
                    .ok_or("array template exceeds the format limit")?;
            }
            Ok(total)
        }
    }
}

fn type_tag(ty: NativeType) -> u8 {
    match ty {
        NativeType::Any => 0,
        NativeType::Int => 1,
        NativeType::Str => 2,
        NativeType::Bytes => 3,
        NativeType::Handle => 4,
        NativeType::Nil => 5,
        NativeType::Number => 6,
        NativeType::Array => 7,
        NativeType::Record => 8,
        NativeType::Float => 9,
        NativeType::Bool => 10,
        NativeType::StrBuf => 11,
    }
}
fn read_type(tag: u8) -> Result<NativeType> {
    Ok(match tag {
        0 => NativeType::Any,
        1 => NativeType::Int,
        2 => NativeType::Str,
        3 => NativeType::Bytes,
        4 => NativeType::Handle,
        5 => NativeType::Nil,
        6 => NativeType::Number,
        7 => NativeType::Array,
        8 => NativeType::Record,
        9 => NativeType::Float,
        10 => NativeType::Bool,
        11 => NativeType::StrBuf,
        _ => return Err(format!("unknown native ABI type tag {tag}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_type_names_round_trip_and_charge_the_string_budget_once() {
        for ty in [
            RuntimeType::Error("Ω".into()),
            RuntimeType::Variant("Ω".into()),
            RuntimeType::Nominal("Ω".into()),
        ] {
            let mut writer = Writer(Vec::new());
            writer.runtime_type(&ty, 0).unwrap();
            let mut reader = Reader {
                bytes: &writer.0,
                pos: 0,
                budget: ConstantBudget {
                    nodes: 1,
                    strings: 2,
                },
            };
            assert_eq!(reader.runtime_type(0).unwrap(), ty);
            assert_eq!(reader.budget.strings, 0);
            assert_eq!(reader.pos, writer.0.len());
        }
    }

    #[test]
    fn owned_string_rejects_truncation_utf8_and_budget_exhaustion() {
        for bytes in [&[2, 0, 0, 0, b'a'][..], &[1, 0, 0, 0, 0xff]] {
            let mut reader = Reader {
                bytes,
                pos: 0,
                budget: ConstantBudget::default(),
            };
            assert!(reader.owned_string().is_err());
        }
        let mut reader = Reader {
            bytes: &[1, 0, 0, 0, b'a'],
            pos: 0,
            budget: ConstantBudget {
                nodes: 1,
                strings: 0,
            },
        };
        assert!(
            reader
                .owned_string()
                .unwrap_err()
                .contains("static string limit")
        );
        assert!(
            Reader::string_buffer(usize::MAX)
                .unwrap_err()
                .contains("cannot allocate")
        );
    }
}
