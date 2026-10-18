//! Native ABI and managed host resources. Portable artifacts contain identities
//! and signatures; executable imports retain a resolved function pointer.
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{IteratorValue, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::rc::{Rc, Weak};
use std::sync::OnceLock;

mod fs;
mod host;
mod json;
mod path;
pub(crate) mod text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeType {
    Any,
    Int,
    Str,
    Bytes,
    Handle,
    Nil,
    Number,
    Array,
    Record,
    Float,
    Bool,
    StrBuf,
}

/// Trusted implementation facts for native results whose portable ABI is Any.
/// These are conservative alternatives, not additional execution authority.
pub(crate) enum NativeResultContract {
    Declared,
    OptionalString,
    Json,
    Iterator,
    Error,
    StructuralClone,
}

impl NativeType {
    pub(crate) fn accepts(self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Self::Any, _)
                | (Self::Int, Value::Int(_))
                | (Self::Str, Value::Str(_))
                | (Self::Bytes, Value::Bytes(_))
                | (Self::Handle, Value::Handle(_))
                | (Self::Nil, Value::Nil)
                | (Self::Number, Value::Int(_) | Value::Float(_))
                | (Self::Array, Value::Array(_))
                | (Self::Record, Value::Record(_))
                | (Self::Float, Value::Float(_))
                | (Self::Bool, Value::Bool(_))
                | (Self::StrBuf, Value::StrBuf(_))
        )
    }
}

type NativeFn = fn(&mut RuntimeContext<'_>, &[Value]) -> Result<Value, RuntimeError>;

pub(crate) struct NativeFunction {
    pub name: &'static str,
    pub parameters: &'static [NativeType],
    pub result: NativeType,
    invoke: NativeFn,
}

pub(crate) type NativeCallback =
    dyn Fn(&mut RuntimeContext<'_>, &[Value]) -> Result<Value, RuntimeError>;

pub(crate) struct RegisteredNative {
    pub source_name: String,
    pub identity: String,
    pub version: u16,
    pub parameters: Vec<NativeType>,
    pub result: NativeType,
    pub mutating_parameters: Vec<usize>,
    pub invoke: Option<Rc<NativeCallback>>,
}

#[derive(Clone, Default)]
pub(crate) struct NativeRegistry {
    pub names: BTreeMap<String, Rc<RegisteredNative>>,
    pub identities: BTreeMap<String, Rc<RegisteredNative>>,
}

thread_local! {
    static ACTIVE_NATIVES: RefCell<Option<Rc<NativeRegistry>>> = const { RefCell::new(None) };
}

pub(crate) fn with_registry<T>(registry: Rc<NativeRegistry>, action: impl FnOnce() -> T) -> T {
    struct Reset(Option<Rc<NativeRegistry>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE_NATIVES.with(|active| {
                active.replace(self.0.take());
            });
        }
    }
    let previous = ACTIVE_NATIVES.with(|active| active.replace(Some(registry)));
    let _reset = Reset(previous);
    action()
}

#[derive(Clone)]
enum NativeCode {
    Builtin(&'static NativeFunction),
    Registered(Rc<RegisteredNative>),
}

/// A checked import that owns registered executable state for its whole lifetime.
/// Cloning an import retains its callback and any pinned plugin library.
#[derive(Clone)]
pub struct NativeImport(NativeCode);

impl std::fmt::Debug for NativeImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NativeImport").field(&self.name()).finish()
    }
}

impl NativeImport {
    pub fn named(name: &str) -> Result<Self, String> {
        if let Some(function) = find(name) {
            return Ok(Self(NativeCode::Builtin(function)));
        }
        ACTIVE_NATIVES
            .with(|active| {
                active
                    .borrow()
                    .as_ref()
                    .and_then(|registry| {
                        registry
                            .names
                            .get(name)
                            .or_else(|| registry.identities.get(name))
                    })
                    .cloned()
            })
            .map(|native| Self(NativeCode::Registered(native)))
            .ok_or_else(|| format!("unresolved runtime import `{}`", name.escape_debug()))
    }

    pub fn resolve(
        name: &str,
        version: u16,
        parameters: &[NativeType],
        result: NativeType,
    ) -> Result<Self, String> {
        let import = Self::named(name)?;
        if name != import.name()
            || version != import.version()
            || parameters != import.parameters()
            || result != import.result()
        {
            return Err(format!(
                "runtime import signature/ABI mismatch for `{}`",
                name.escape_debug()
            ));
        }
        Ok(import)
    }

    pub fn name(&self) -> &str {
        match &self.0 {
            NativeCode::Builtin(function) => function.name,
            NativeCode::Registered(function) => &function.identity,
        }
    }
    pub fn version(&self) -> u16 {
        match &self.0 {
            NativeCode::Builtin(_) => 1,
            NativeCode::Registered(function) => function.version,
        }
    }
    pub fn parameters(&self) -> &[NativeType] {
        match &self.0 {
            NativeCode::Builtin(function) => function.parameters,
            NativeCode::Registered(function) => &function.parameters,
        }
    }
    pub fn result(&self) -> NativeType {
        match &self.0 {
            NativeCode::Builtin(function) => function.result,
            NativeCode::Registered(function) => function.result,
        }
    }
    pub(crate) fn is_linked(&self) -> bool {
        match &self.0 {
            NativeCode::Builtin(_) => true,
            NativeCode::Registered(function) => function.invoke.is_some(),
        }
    }
    fn operation(&self) -> &'static str {
        match &self.0 {
            NativeCode::Builtin(function) => function.name,
            NativeCode::Registered(_) => "native extension",
        }
    }

    pub(crate) fn result_contract(&self) -> NativeResultContract {
        if matches!(self.0, NativeCode::Registered(_)) {
            return NativeResultContract::Declared;
        }
        match self.name() {
            "process::env" | "path::parent" | "path::file_name" | "path::extension" => {
                NativeResultContract::OptionalString
            }
            "json::parse" => NativeResultContract::Json,
            "$vm::iterator_new" => NativeResultContract::Iterator,
            "$vm::merge_cleanup_error" => NativeResultContract::Error,
            "$vm::structural_clone" => NativeResultContract::StructuralClone,
            _ => NativeResultContract::Declared,
        }
    }

    pub(crate) fn mutating_parameters(&self) -> &[usize] {
        if let NativeCode::Registered(function) = &self.0 {
            return &function.mutating_parameters;
        }
        match self.name() {
            "bytes::set" | "fmt::write" | "sort::native_quick" | "sort::native_stable" => &[0],
            "io::read_into" => &[1],
            _ => &[],
        }
    }

    pub fn call(
        &self,
        context: &mut RuntimeContext<'_>,
        arguments: &[Value],
    ) -> Result<Value, RuntimeError> {
        if arguments.len() != self.parameters().len() {
            return Err(RuntimeError::InvalidNativeCall);
        }
        for (ty, value) in self.parameters().iter().zip(arguments) {
            if *ty != NativeType::Any && matches!(value, Value::Error(_)) {
                return Ok(value.clone());
            }
            if !ty.accepts(value) {
                return Err(RuntimeError::TypeMismatch {
                    op: self.operation(),
                });
            }
        }
        let value = match &self.0 {
            NativeCode::Builtin(function) => (function.invoke)(context, arguments)?,
            NativeCode::Registered(function) => {
                let invoke =
                    function
                        .invoke
                        .as_ref()
                        .ok_or_else(|| RuntimeError::NativeFailure {
                            message: format!(
                                "native function `{}` is not linked",
                                function.source_name
                            ),
                        })?;
                invoke(context, arguments)?
            }
        };
        if !matches!(value, Value::Error(_)) && !self.result().accepts(&value) {
            return Err(match &self.0 {
                NativeCode::Builtin(_) => RuntimeError::InvalidNativeCall,
                NativeCode::Registered(function) => RuntimeError::NativeFailure {
                    message: format!(
                        "native function `{}` returned a value outside its contract",
                        function.source_name
                    ),
                },
            });
        }
        Ok(value)
    }
}

pub(crate) fn find(name: &str) -> Option<&'static NativeFunction> {
    if name == BYTES_LITERAL.name {
        return Some(&BYTES_LITERAL);
    }
    if name == CLEANUP_MERGE.name {
        return Some(&CLEANUP_MERGE);
    }
    if name == STRUCTURAL_CLONE.name {
        return Some(&STRUCTURAL_CLONE);
    }
    if name == ITERATOR_NEW.name {
        return Some(&ITERATOR_NEW);
    }
    if name == ITERATOR_NEXT.name {
        return Some(&ITERATOR_NEXT);
    }
    if name == ITERATOR_CURRENT.name {
        return Some(&ITERATOR_CURRENT);
    }
    FUNCTIONS.iter().find(|function| function.name == name)
}

pub(crate) fn names() -> impl Iterator<Item = String> {
    let mut names: Vec<_> = FUNCTIONS
        .iter()
        .map(|function| function.name.to_owned())
        .collect();
    ACTIVE_NATIVES.with(|active| {
        if let Some(registry) = active.borrow().as_ref() {
            names.extend(registry.names.keys().cloned());
        }
    });
    names.into_iter()
}

pub(crate) fn builtin_module(name: &str) -> bool {
    FUNCTIONS.iter().any(|function| {
        function
            .name
            .split_once("::")
            .is_some_and(|(module, _)| module == name)
    }) || crate::core_modules::SOURCES
        .iter()
        .any(|(module, _)| *module == name)
        || matches!(name, "err" | "build")
}

pub(crate) fn fingerprint_natives(hasher: &mut blake3::Hasher) {
    ACTIVE_NATIVES.with(|active| {
        let registry = active.borrow();
        let names = registry.as_ref().map(|registry| &registry.identities);
        hasher.update(b"cog-native-registry-v1\0");
        hasher.update(&(names.map_or(0, |names| names.len()) as u64).to_le_bytes());
        if let Some(names) = names {
            for name in names.keys() {
                hasher.update(&(name.len() as u64).to_le_bytes());
                hasher.update(name.as_bytes());
            }
        }
    });
}

use NativeType::{Any, Array, Bool, Bytes, Float, Handle, Int, Nil, Number, Record, Str, StrBuf};

static HASH_STATE: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();

fn hash_value(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let mut hasher = HASH_STATE
        .get_or_init(std::collections::hash_map::RandomState::new)
        .build_hasher();
    match &args[0] {
        Value::Int(value) => value.hash(&mut hasher),
        Value::Bool(value) => value.hash(&mut hasher),
        Value::Str(value) => value.hash(&mut hasher),
        _ => return Err(RuntimeError::TypeMismatch { op: "hash::of" }),
    }
    Ok(Value::Int(hasher.finish() as i64))
}

fn native_sort(
    _: &mut RuntimeContext<'_>,
    args: &[Value],
    stable: bool,
) -> Result<Value, RuntimeError> {
    let Value::Array(array) = &args[0] else {
        return Err(RuntimeError::TypeMismatch { op: "sort" });
    };
    Ok(array
        .try_sort_primitive(stable)?
        .unwrap_or(Value::Bool(false)))
}

fn native_quick(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    native_sort(context, args, false)
}

fn native_stable(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    native_sort(context, args, true)
}
// Compiler-private, pure error metadata operation. Not enumerated as a source
// module/API. Uses the existing checked native ABI, with no new bytecode opcode.
static CLEANUP_MERGE: NativeFunction = NativeFunction {
    name: "$vm::merge_cleanup_error",
    parameters: &[Any, Any],
    result: Any,
    invoke: merge_cleanup_error,
};

static STRUCTURAL_CLONE: NativeFunction = NativeFunction {
    name: "$vm::structural_clone",
    parameters: &[Any],
    result: Any,
    invoke: structural_clone,
};

static BYTES_LITERAL: NativeFunction = NativeFunction {
    name: "$vm::bytes_literal",
    parameters: &[Str],
    result: Bytes,
    invoke: bytes_literal,
};

static ITERATOR_NEW: NativeFunction = NativeFunction {
    name: "$vm::iterator_new",
    parameters: &[Any],
    result: Any,
    invoke: iterator_new,
};

static ITERATOR_NEXT: NativeFunction = NativeFunction {
    name: "$vm::iterator_next",
    parameters: &[Any],
    result: Bool,
    invoke: iterator_next,
};

static ITERATOR_CURRENT: NativeFunction = NativeFunction {
    name: "$vm::iterator_current",
    parameters: &[Any],
    result: Any,
    invoke: iterator_current,
};

pub(crate) fn bytes_literal_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&BYTES_LITERAL))
}

pub(crate) fn iterator_new_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&ITERATOR_NEW))
}

pub(crate) fn iterator_next_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&ITERATOR_NEXT))
}

pub(crate) fn iterator_current_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&ITERATOR_CURRENT))
}

fn iterator_new(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    if let Value::Error(_) = args[0] {
        return Ok(args[0].clone());
    }
    let iterator = IteratorValue::new(args[0].clone())?;
    Ok(Value::Iterator(HeapRef::try_new(RefCell::new(iterator))?))
}

fn iterator_next(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    match &args[0] {
        Value::Error(_) => Ok(args[0].clone()),
        Value::Iterator(iterator) => iterator
            .try_borrow_mut()
            .map_err(|_| invalid("iterator borrow"))?
            .next()
            .map(Value::Bool),
        _ => Err(RuntimeError::TypeMismatch {
            op: "iterator next",
        }),
    }
}

fn iterator_current(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    match &args[0] {
        Value::Error(_) => Ok(args[0].clone()),
        Value::Iterator(iterator) => iterator
            .try_borrow()
            .map_err(|_| invalid("iterator borrow"))?
            .current(),
        _ => Err(RuntimeError::TypeMismatch {
            op: "iterator current",
        }),
    }
}

fn bytes_literal(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let hex = string(&args[0]).as_bytes();
    if !hex.len().is_multiple_of(2) {
        return Err(invalid("bytes literal"));
    }
    let size = hex.len() / 2;
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| size > limit)
    {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    for pair in hex.as_chunks::<2>().0 {
        let (Some(hi), Some(lo)) = (digit(pair[0]), digit(pair[1])) else {
            return Err(invalid("bytes literal"));
        };
        bytes.push(hi * 16 + lo);
    }
    Ok(Value::Bytes(HeapRef::try_new(RefCell::new(bytes))?))
}

pub(crate) fn cleanup_merge_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&CLEANUP_MERGE))
}

pub(crate) fn structural_clone_import() -> NativeImport {
    NativeImport(NativeCode::Builtin(&STRUCTURAL_CLONE))
}

fn structural_clone(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let mut active = HashSet::new();
    let mut copies = HashMap::new();
    clone_value(&args[0], &mut active, &mut copies, 0)
}

fn clone_value(
    value: &Value,
    active: &mut HashSet<usize>,
    copies: &mut HashMap<usize, Value>,
    depth: usize,
) -> Result<Value, RuntimeError> {
    match value {
        Value::Array(array) => {
            let identity = array.identity();
            if let Some(copy) = copies.get(&identity) {
                return Ok(copy.clone());
            }
            active
                .try_reserve(1)
                .map_err(|_| RuntimeError::ResourceLimit { resource: "clone" })?;
            if depth >= 64 || !active.insert(identity) {
                return Err(RuntimeError::ResourceLimit { resource: "clone" });
            }
            let result = (|| {
                let source = array
                    .try_borrow()
                    .map_err(|_| RuntimeError::InvalidArgument { op: "clone" })?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(source.len())
                    .map_err(|_| RuntimeError::ResourceLimit { resource: "clone" })?;
                for item in source.iter() {
                    values.push(clone_value(item, active, copies, depth + 1)?);
                }
                drop(source);
                let copy = array.try_with_values_like(values)?;
                Ok(Value::Array(HeapRef::try_new(copy)?))
            })();
            active.remove(&identity);
            if let Ok(copy) = &result {
                copies
                    .try_reserve(1)
                    .map_err(|_| RuntimeError::ResourceLimit { resource: "clone" })?;
                copies.insert(identity, copy.clone());
            }
            result
        }
        Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Nil
        | Value::Str(_)
        | Value::Error(_) => Ok(value.clone()),
        _ => Err(RuntimeError::TypeMismatch { op: "clone" }),
    }
}

fn merge_cleanup_error(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let Value::Error(secondary) = &args[1] else {
        return Err(RuntimeError::InvalidNativeCall);
    };
    Ok(match &args[0] {
        Value::Error(primary) => Value::Error(Box::new(
            (**primary).clone().with_suppressed((**secondary).clone()),
        )),
        _ => args[1].clone(),
    })
}

static FUNCTIONS: &[NativeFunction] = &[
    NativeFunction {
        name: "sort::native_quick",
        parameters: &[Array],
        result: Bool,
        invoke: native_quick,
    },
    NativeFunction {
        name: "sort::native_stable",
        parameters: &[Array],
        result: Bool,
        invoke: native_stable,
    },
    NativeFunction {
        name: "hash::of",
        parameters: &[Any],
        result: Int,
        invoke: hash_value,
    },
    NativeFunction {
        name: "str::to_int",
        parameters: &[Any],
        result: Int,
        invoke: text::to_int,
    },
    NativeFunction {
        name: "str::to_float",
        parameters: &[Any],
        result: Float,
        invoke: text::to_float,
    },
    NativeFunction {
        name: "str::to_bool",
        parameters: &[Any],
        result: Bool,
        invoke: text::to_bool,
    },
    NativeFunction {
        name: "str::to_str",
        parameters: &[Any],
        result: Str,
        invoke: text::to_str,
    },
    NativeFunction {
        name: "str::builder",
        parameters: &[],
        result: StrBuf,
        invoke: text::builder,
    },
    NativeFunction {
        name: "str::build",
        parameters: &[StrBuf],
        result: Str,
        invoke: text::build,
    },
    NativeFunction {
        name: "fmt::write",
        parameters: &[StrBuf, Str, Any],
        result: StrBuf,
        invoke: text::write,
    },
    NativeFunction {
        name: "time::sleep",
        parameters: &[Number],
        result: Nil,
        invoke: host::sleep,
    },
    NativeFunction {
        name: "time::now",
        parameters: &[],
        result: Int,
        invoke: host::now,
    },
    NativeFunction {
        name: "time::now_nanos",
        parameters: &[],
        result: Int,
        invoke: host::now_nanos,
    },
    NativeFunction {
        name: "time::now_millis",
        parameters: &[],
        result: Int,
        invoke: host::now_millis,
    },
    NativeFunction {
        name: "time::monotonic_nanos",
        parameters: &[],
        result: Int,
        invoke: host::monotonic_nanos,
    },
    NativeFunction {
        name: "entropy::bytes",
        parameters: &[Int],
        result: Bytes,
        invoke: host::entropy,
    },
    NativeFunction {
        name: "process::run",
        parameters: &[Str, Array],
        result: Record,
        invoke: host::run,
    },
    NativeFunction {
        name: "process::shell",
        parameters: &[Str],
        result: Record,
        invoke: host::shell,
    },
    NativeFunction {
        name: "process::args",
        parameters: &[],
        result: Array,
        invoke: host::arguments,
    },
    NativeFunction {
        name: "process::env",
        parameters: &[Str],
        result: Any,
        invoke: host::environment,
    },
    NativeFunction {
        name: "path::join",
        parameters: &[Str, Str],
        result: Str,
        invoke: path::join,
    },
    NativeFunction {
        name: "path::is_absolute",
        parameters: &[Str],
        result: Bool,
        invoke: path::is_absolute,
    },
    NativeFunction {
        name: "path::parent",
        parameters: &[Str],
        result: Any,
        invoke: path::parent,
    },
    NativeFunction {
        name: "path::file_name",
        parameters: &[Str],
        result: Any,
        invoke: path::file_name,
    },
    NativeFunction {
        name: "path::extension",
        parameters: &[Str],
        result: Any,
        invoke: path::extension,
    },
    NativeFunction {
        name: "path::with_extension",
        parameters: &[Str, Str],
        result: Str,
        invoke: path::with_extension,
    },
    NativeFunction {
        name: "fs::metadata",
        parameters: &[Str],
        result: Record,
        invoke: fs::metadata,
    },
    NativeFunction {
        name: "fs::read_dir",
        parameters: &[Str],
        result: Array,
        invoke: fs::read_dir,
    },
    NativeFunction {
        name: "fs::create_dir",
        parameters: &[Str],
        result: Nil,
        invoke: fs::create_dir,
    },
    NativeFunction {
        name: "fs::create_dir_all",
        parameters: &[Str],
        result: Nil,
        invoke: fs::create_dir_all,
    },
    NativeFunction {
        name: "fs::remove_dir",
        parameters: &[Str],
        result: Nil,
        invoke: fs::remove_dir,
    },
    NativeFunction {
        name: "fs::remove_file",
        parameters: &[Str],
        result: Nil,
        invoke: fs::remove_file,
    },
    NativeFunction {
        name: "json::parse",
        parameters: &[Str],
        result: Any,
        invoke: json::parse,
    },
    NativeFunction {
        name: "json::stringify",
        parameters: &[Any],
        result: Str,
        invoke: json::stringify,
    },
    NativeFunction {
        name: "bytes::new",
        parameters: &[Int],
        result: Bytes,
        invoke: bytes_new,
    },
    NativeFunction {
        name: "bytes::copy",
        parameters: &[Bytes],
        result: Bytes,
        invoke: bytes_copy,
    },
    NativeFunction {
        name: "bytes::slice",
        parameters: &[Bytes, Int, Int],
        result: Bytes,
        invoke: bytes_slice,
    },
    NativeFunction {
        name: "bytes::concat",
        parameters: &[Bytes, Bytes],
        result: Bytes,
        invoke: bytes_concat,
    },
    NativeFunction {
        name: "bytes::from_str",
        parameters: &[Str],
        result: Bytes,
        invoke: bytes_from_str,
    },
    NativeFunction {
        name: "bytes::to_str",
        parameters: &[Bytes, Int, Int],
        result: Str,
        invoke: bytes_to_str,
    },
    NativeFunction {
        name: "bytes::render",
        parameters: &[Any],
        result: Bytes,
        invoke: bytes_render,
    },
    NativeFunction {
        name: "bytes::len",
        parameters: &[Bytes],
        result: Int,
        invoke: bytes_len,
    },
    NativeFunction {
        name: "bytes::get",
        parameters: &[Bytes, Int],
        result: Int,
        invoke: bytes_get,
    },
    NativeFunction {
        name: "bytes::set",
        parameters: &[Bytes, Int, Int],
        result: Nil,
        invoke: bytes_set,
    },
    NativeFunction {
        name: "io::stdout",
        parameters: &[],
        result: Handle,
        invoke: stdout,
    },
    NativeFunction {
        name: "io::read_line",
        parameters: &[],
        result: Str,
        invoke: read_line,
    },
    NativeFunction {
        name: "io::stdin",
        parameters: &[],
        result: Handle,
        invoke: stdin,
    },
    NativeFunction {
        name: "io::open",
        parameters: &[Str, Str, Int],
        result: Handle,
        invoke: open,
    },
    NativeFunction {
        name: "io::read_into",
        parameters: &[Handle, Bytes, Int, Int],
        result: Int,
        invoke: read_into,
    },
    NativeFunction {
        name: "io::write",
        parameters: &[Handle, Bytes, Int, Int],
        result: Int,
        invoke: write,
    },
    NativeFunction {
        name: "io::write_all",
        parameters: &[Handle, Bytes, Int, Int],
        result: Nil,
        invoke: write_all,
    },
    NativeFunction {
        name: "io::seek",
        parameters: &[Handle, Int, Str],
        result: Int,
        invoke: seek,
    },
    NativeFunction {
        name: "io::flush",
        parameters: &[Handle],
        result: Nil,
        invoke: flush,
    },
    NativeFunction {
        name: "io::close",
        parameters: &[Handle],
        result: Nil,
        invoke: close,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmCapabilities {
    pub allow_print: bool,
    pub allow_sleep: bool,
    pub allow_exec: bool,
    pub allow_asm: bool,
    pub allow_read: bool,
    pub allow_time: bool,
    pub allow_random: bool,
    pub allow_process_info: bool,
    pub allow_fs_read: bool,
    pub allow_fs_write: bool,
}

impl Default for VmCapabilities {
    fn default() -> Self {
        Self::relaxed()
    }
}
impl VmCapabilities {
    pub const fn relaxed() -> Self {
        Self {
            allow_print: true,
            allow_sleep: true,
            allow_exec: true,
            allow_asm: true,
            allow_read: true,
            allow_time: true,
            allow_random: true,
            allow_process_info: true,
            allow_fs_read: true,
            allow_fs_write: true,
        }
    }
    pub const fn restricted() -> Self {
        Self {
            allow_print: false,
            allow_sleep: false,
            allow_exec: false,
            allow_asm: false,
            allow_read: false,
            allow_time: false,
            allow_random: false,
            allow_process_info: false,
            allow_fs_read: false,
            allow_fs_write: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeLimits {
    /// Optional per-buffer and aggregate conversion budget. `None` leaves this
    /// resource unbounded by the VM (subject to host representation and OS limits).
    pub max_buffer_bytes: Option<usize>,
    /// Optional number of managed I/O resource slots, including standard streams.
    pub max_resources: Option<usize>,
    /// Optional execution limits. `None` preserves ordinary general-purpose runs.
    pub max_steps: Option<u64>,
    pub max_operand_values: Option<usize>,
    pub max_call_frames: Option<usize>,
    /// Combined local/global slots, across all active call frames.
    pub max_slots: Option<usize>,
    /// Optional limit on live managed Cog objects. None leaves growth to the OS.
    pub max_heap_objects: Option<usize>,
}
impl RuntimeLimits {
    /// Return the active buffer cap, or `usize::MAX` when no VM cap is set.
    /// Checked growth and fallible allocation still apply where implemented;
    /// this does not promise protection from process-wide memory exhaustion.
    pub(crate) fn buffer_limit(&self) -> usize {
        self.max_buffer_bytes.unwrap_or(usize::MAX)
    }
}

/// Opaque identity, scoped to one context. Slot reuse never revives old aliases.
#[derive(Debug, Clone)]
pub struct ResourceHandle {
    owner: Weak<()>,
    slot: usize,
    generation: u64,
}
impl PartialEq for ResourceHandle {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot
            && self.generation == other.generation
            && Weak::ptr_eq(&self.owner, &other.owner)
    }
}
impl Eq for ResourceHandle {}

enum Stream {
    Stdout,
    Stdin,
    File(FileStream),
}
enum FileStream {
    Direct(std::fs::File),
    Buffered(BufferedFile),
}

struct BufferedFile {
    file: std::fs::File,
    buffer: Vec<u8>,
    capacity: usize,
}

impl BufferedFile {
    fn has_pending_output(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        let pending = self.buffer.len();
        let mut written = 0;
        while written < pending {
            match self.file.write(&self.buffer[written..pending]) {
                Ok(0) => {
                    self.buffer.drain(..written);
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write buffered file contents",
                    ));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.buffer.drain(..written);
                    return Err(error);
                }
            }
        }
        self.buffer.clear();
        Ok(())
    }
}

impl Write for BufferedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.capacity.saturating_sub(self.buffer.len()) {
            self.flush_buffer()?;
        }
        if self.buffer.is_empty() && bytes.len() >= self.capacity {
            return self.file.write(bytes);
        }
        let count = bytes
            .len()
            .min(self.capacity.saturating_sub(self.buffer.len()));
        self.buffer.extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        self.file.flush()
    }
}

impl Seek for BufferedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.flush_buffer()?;
        self.file.seek(position)
    }
}
struct Resource {
    stream: Stream,
    readable: bool,
    writable: bool,
    dirty: bool,
}
struct Slot {
    generation: u64,
    resource: Option<Resource>,
}

/// One execution's host state. Explicit finish reports flush failures and closes
/// all resources; Drop only releases them, without retrying failed buffered I/O.
pub struct RuntimeContext<'a> {
    pub capabilities: VmCapabilities,
    pub limits: RuntimeLimits,
    output: &'a mut dyn Write,
    input: Option<Box<dyn Read + 'a>>,
    buffered_input: Option<BufReader<Box<dyn Read + 'a>>>,
    identity: Rc<()>,
    process_arguments: Option<Vec<OsString>>,
    last_source_location: Option<crate::diagnostic::SourceLocation>,
    slots: Vec<Slot>,
    free: Vec<usize>,
}

impl<'a> RuntimeContext<'a> {
    pub(crate) fn write_native_output(&mut self, text: &str) -> Result<(), RuntimeError> {
        if !self.capabilities.allow_print {
            return Err(RuntimeError::PermissionDenied {
                op: "native output",
            });
        }
        if self
            .limits
            .max_buffer_bytes
            .is_some_and(|limit| text.len() > limit)
        {
            return Err(RuntimeError::ResourceLimit { resource: "bytes" });
        }
        self.output
            .write_all(text.as_bytes())
            .and_then(|()| self.output.flush())
            .map_err(|error| RuntimeError::IoWriteFailed {
                message: error.to_string(),
            })
    }
    pub fn new(output: &'a mut dyn Write, capabilities: VmCapabilities) -> Self {
        Self::with_input(output, Box::new(io::stdin()), capabilities)
    }
    pub fn with_input(
        output: &'a mut dyn Write,
        input: Box<dyn Read + 'a>,
        capabilities: VmCapabilities,
    ) -> Self {
        Self {
            capabilities,
            limits: RuntimeLimits::default(),
            output,
            input: Some(input),
            buffered_input: None,
            identity: Rc::new(()),
            process_arguments: None,
            last_source_location: None,
            free: Vec::new(),
            slots: vec![
                Slot {
                    generation: 0,
                    resource: Some(Resource {
                        stream: Stream::Stdout,
                        readable: false,
                        writable: true,
                        dirty: false,
                    }),
                },
                Slot {
                    generation: 0,
                    resource: Some(Resource {
                        stream: Stream::Stdin,
                        readable: true,
                        writable: false,
                        dirty: false,
                    }),
                },
            ],
        }
    }

    /// Supply arguments for `process::args`. When omitted, the native module
    /// reads the host process arguments excluding argv[0].
    pub fn set_process_arguments(&mut self, arguments: Vec<OsString>) {
        self.process_arguments = Some(arguments);
    }

    /// Source coordinate for the last terminal VM error, when the artifact has
    /// source-map metadata. Catchable errors handled by Cog are not recorded.
    pub fn last_source_location(&self) -> Option<&crate::diagnostic::SourceLocation> {
        self.last_source_location.as_ref()
    }

    pub(crate) fn set_source_location(
        &mut self,
        location: Option<crate::diagnostic::SourceLocation>,
    ) {
        self.last_source_location = location;
    }

    pub(crate) fn process_arguments(&self) -> Option<&[OsString]> {
        self.process_arguments.as_deref()
    }

    fn handle(&self, slot: usize) -> ResourceHandle {
        ResourceHandle {
            owner: Rc::downgrade(&self.identity),
            slot,
            generation: self.slots[slot].generation,
        }
    }
    fn slot(&self, handle: &ResourceHandle) -> Result<usize, RuntimeError> {
        if !Weak::ptr_eq(&handle.owner, &Rc::downgrade(&self.identity)) {
            return Err(RuntimeError::InvalidResource);
        }
        match self.slots.get(handle.slot) {
            Some(slot) if slot.generation == handle.generation && slot.resource.is_some() => {
                Ok(handle.slot)
            }
            _ => Err(RuntimeError::InvalidResource),
        }
    }
    fn reserve_resource_slot(&mut self) -> Result<(), RuntimeError> {
        if !self.free.is_empty() {
            return Ok(());
        }
        if self
            .limits
            .max_resources
            .is_some_and(|limit| self.slots.len() >= limit)
        {
            return Err(RuntimeError::ResourceLimit {
                resource: "handles",
            });
        }

        self.slots
            .try_reserve(1)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
        Ok(())
    }
    fn insert(&mut self, resource: Resource) -> Result<Value, RuntimeError> {
        self.reserve_resource_slot()?;
        let slot = if let Some(slot) = self.free.pop() {
            self.slots[slot].resource = Some(resource);
            slot
        } else {
            self.slots.push(Slot {
                generation: 0,
                resource: Some(resource),
            });
            self.slots.len() - 1
        };
        Ok(Value::Handle(self.handle(slot)))
    }
    fn permission(&self, slot: usize, writing: bool, op: &'static str) -> Result<(), RuntimeError> {
        let resource = self.slots[slot].resource.as_ref().unwrap();
        let allowed = match resource.stream {
            Stream::Stdout => self.capabilities.allow_print,
            Stream::Stdin => self.capabilities.allow_read,
            Stream::File(_) if writing => self.capabilities.allow_fs_write,
            Stream::File(_) => self.capabilities.allow_fs_read,
        };
        if !allowed {
            return Err(RuntimeError::PermissionDenied { op });
        }
        if (writing && !resource.writable) || (!writing && !resource.readable) {
            return Err(RuntimeError::IoFailure {
                op,
                message: "stream does not support this operation".into(),
            });
        }
        Ok(())
    }
    fn flush_slot(&mut self, slot: usize) -> Result<(), RuntimeError> {
        self.permission(slot, true, "io::flush")?;
        let resource = self.slots[slot].resource.as_mut().unwrap();
        let result = match &mut resource.stream {
            Stream::Stdout => self.output.flush(),
            Stream::File(FileStream::Direct(file)) => file.flush(),
            Stream::File(FileStream::Buffered(file)) => file.flush(),
            Stream::Stdin => unreachable!(),
        };
        resource.dirty = false;
        result.map_err(write_error)?;
        Ok(())
    }
    fn pending_output(&self, slot: usize) -> bool {
        let resource = self.slots[slot].resource.as_ref().unwrap();
        resource.dirty
            || matches!(&resource.stream, Stream::File(FileStream::Buffered(file)) if file.has_pending_output())
    }
    fn close_slot(&mut self, slot: usize, explicit: bool) -> Result<(), RuntimeError> {
        let resource = self.slots[slot].resource.as_ref().unwrap();
        let result = if resource.writable && (explicit || resource.dirty) {
            self.flush_slot(slot)
        } else {
            Ok(())
        };
        let resource = self.slots[slot].resource.take().unwrap();
        // BufferedFile does not flush from Drop. After a partial flush failure,
        // any unwritten suffix is discarded rather than retried implicitly.
        discard(resource);
        if let Some(generation) = self.slots[slot].generation.checked_add(1) {
            self.slots[slot].generation = generation;
            if slot >= 2 && self.free.try_reserve(1).is_ok() {
                // If this optional cache cannot grow, the closed slot remains
                // unavailable for reuse; close itself still succeeds.
                self.free.push(slot);
            }
        }
        result
    }
    pub fn finish(&mut self) -> Result<(), RuntimeError> {
        let mut failure: Option<RuntimeError> = None;
        for slot in 0..self.slots.len() {
            if self.slots[slot].resource.is_some()
                && let Err(error) = self.close_slot(slot, false)
            {
                failure = Some(match failure {
                    Some(primary) => primary.with_suppressed(error),
                    None => error,
                });
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn discard(resource: Resource) {
    if let Stream::File(FileStream::Buffered(file)) = resource.stream {
        drop(file);
    }
}
impl Drop for RuntimeContext<'_> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(resource) = slot.resource.take() {
                discard(resource);
            }
        }
    }
}

fn integer(value: &Value) -> i64 {
    if let Value::Int(n) = value {
        *n
    } else {
        unreachable!()
    }
}
fn string(value: &Value) -> &str {
    if let Value::Str(s) = value {
        s
    } else {
        unreachable!()
    }
}
fn buffer(value: &Value) -> &RefCell<Vec<u8>> {
    if let Value::Bytes(b) = value {
        b
    } else {
        unreachable!()
    }
}
fn bytes_borrow<'a>(
    value: &'a Value,
    op: &'static str,
) -> Result<std::cell::Ref<'a, Vec<u8>>, RuntimeError> {
    buffer(value)
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op })
}
fn bytes_borrow_mut<'a>(
    value: &'a Value,
    op: &'static str,
) -> Result<std::cell::RefMut<'a, Vec<u8>>, RuntimeError> {
    buffer(value)
        .try_borrow_mut()
        .map_err(|_| RuntimeError::InvalidArgument { op })
}
fn handle(value: &Value) -> &ResourceHandle {
    if let Value::Handle(h) = value {
        h
    } else {
        unreachable!()
    }
}
fn invalid(op: &'static str) -> RuntimeError {
    RuntimeError::InvalidArgument { op }
}
fn write_error(error: io::Error) -> RuntimeError {
    RuntimeError::IoWriteFailed {
        message: error.to_string(),
    }
}
fn io_error(op: &'static str, error: io::Error) -> RuntimeError {
    RuntimeError::IoFailure {
        op,
        message: error.to_string(),
    }
}
fn range(start: &Value, count: &Value, len: usize) -> Result<std::ops::Range<usize>, RuntimeError> {
    let offset = usize::try_from(integer(start)).map_err(|_| invalid("bytes range"))?;
    let count = usize::try_from(integer(count)).map_err(|_| invalid("bytes range"))?;
    let end = offset
        .checked_add(count)
        .filter(|end| *end <= len)
        .ok_or_else(|| invalid("bytes range"))?;
    Ok(offset..end)
}
fn checked_size(context: &RuntimeContext<'_>, n: i64) -> Result<usize, RuntimeError> {
    usize::try_from(n)
        .ok()
        .filter(|n| *n <= context.limits.buffer_limit())
        .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })
}
fn allocated(context: &RuntimeContext<'_>, data: &[u8]) -> Result<Value, RuntimeError> {
    checked_size(context, data.len() as i64)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(data.len())
        .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
    bytes.extend_from_slice(data);
    Ok(Value::Bytes(HeapRef::try_new(RefCell::new(bytes))?))
}
fn bytes_new(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let size = checked_size(context, integer(&args[0]))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
    bytes.resize(size, 0);
    Ok(Value::Bytes(HeapRef::try_new(RefCell::new(bytes))?))
}
fn bytes_copy(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let bytes = bytes_borrow(&args[0], "bytes::copy")?;
    allocated(context, &bytes)
}
fn bytes_slice(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let bytes = bytes_borrow(&args[0], "bytes::slice")?;
    let span = range(&args[1], &args[2], bytes.len())?;
    allocated(context, &bytes[span])
}
fn bytes_concat(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let left = bytes_borrow(&args[0], "bytes::concat")?;
    let right = bytes_borrow(&args[1], "bytes::concat")?;
    let size = left
        .len()
        .checked_add(right.len())
        .and_then(|size| i64::try_from(size).ok())
        .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })?;
    let size = checked_size(context, size)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
    bytes.extend_from_slice(&left);
    bytes.extend_from_slice(&right);
    Ok(Value::Bytes(HeapRef::try_new(RefCell::new(bytes))?))
}
fn bytes_from_str(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    allocated(context, string(&args[0]).as_bytes())
}
fn bytes_to_str(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let bytes = bytes_borrow(&args[0], "bytes::to_str")?;
    let span = range(&args[1], &args[2], bytes.len())?;
    checked_size(context, integer(&args[2]))?;
    let text = std::str::from_utf8(&bytes[span]).map_err(|_| RuntimeError::ParseFailure {
        from: "bytes",
        to: "str",
    })?;
    Ok(Value::Str(Rc::from(text)))
}
fn bytes_render(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    // Formatting remains native; print/newline/flush policy is ordinary Cog code.
    if let Value::Str(text) = &args[0] {
        return allocated(context, text.as_bytes());
    }
    let rendered = text::render(&args[0], context.limits.buffer_limit(), "bytes")?;
    Ok(Value::Bytes(HeapRef::try_new(RefCell::new(
        rendered.into_bytes(),
    ))?))
}
fn bytes_len(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::Int(
        bytes_borrow(&args[0], "bytes::len")?.len() as i64
    ))
}
fn bytes_get(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let bytes = bytes_borrow(&args[0], "bytes::get")?;
    let span = range(&args[1], &Value::Int(1), bytes.len())?;
    Ok(Value::Int(bytes[span.start] as i64))
}
fn bytes_set(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let mut bytes = bytes_borrow_mut(&args[0], "bytes::set")?;
    let span = range(&args[1], &Value::Int(1), bytes.len())?;
    let value = u8::try_from(integer(&args[2])).map_err(|_| invalid("bytes::set"))?;
    bytes[span.start] = value;
    Ok(Value::Nil)
}
fn stdout(context: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::Handle(context.handle(0)))
}
fn stdin(context: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::Handle(context.handle(1)))
}
fn read_line(context: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    let slot = context.slot(&context.handle(1))?;
    context.permission(slot, false, "io::read_line")?;
    let input = context
        .buffered_input
        .get_or_insert_with(|| BufReader::new(context.input.take().unwrap()));
    // UTF-8 sequences can straddle reads; keep unread bytes in the reader.
    let mut bytes = Vec::new();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error("io::read_line", error)),
        };
        if available.is_empty() {
            if bytes.is_empty() {
                return Err(RuntimeError::IoReadFailed);
            }
            break;
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if count > context.limits.buffer_limit().saturating_sub(bytes.len()) {
            return Err(RuntimeError::ResourceLimit { resource: "bytes" });
        }
        let ended = available[count - 1] == b'\n';
        bytes
            .try_reserve(count)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
        bytes.extend_from_slice(&available[..count]);
        input.consume(count);
        if ended {
            break;
        }
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let line = String::from_utf8(bytes).map_err(|_| RuntimeError::ParseFailure {
        from: "bytes",
        to: "str",
    })?;
    Ok(Value::Str(line.into()))
}
fn open(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let (read, write, create, truncate, append) = match string(&args[1]) {
        "r" => (true, false, false, false, false),
        "r+" => (true, true, false, false, false),
        "w" => (false, true, true, true, false),
        "w+" => (true, true, true, true, false),
        "a" => (false, true, true, false, true),
        "a+" => (true, true, true, false, true),
        _ => return Err(invalid("io::open")),
    };
    if (read && !context.capabilities.allow_fs_read)
        || (write && !context.capabilities.allow_fs_write)
    {
        return Err(RuntimeError::PermissionDenied { op: "io::open" });
    }
    let capacity = checked_size(context, integer(&args[2]))?;
    // Reserve the slot and its future free-list entry before opening a path.
    // A fallible table growth must not happen after a truncating open.
    context.reserve_resource_slot()?;
    let buffer = if write && capacity > 0 {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
        Some(buffer)
    } else {
        None
    };
    let file = std::fs::OpenOptions::new()
        .read(read)
        .write(write)
        .create(create)
        .truncate(truncate)
        .append(append)
        .open(string(&args[0]))
        .map_err(|error| io_error("io::open", error))?;
    let stream = match buffer {
        Some(buffer) => FileStream::Buffered(BufferedFile {
            file,
            buffer,
            capacity,
        }),
        None => FileStream::Direct(file),
    };
    context.insert(Resource {
        stream: Stream::File(stream),
        readable: read,
        writable: write,
        dirty: false,
    })
}
fn read_into(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let slot = context.slot(handle(&args[0]))?;
    context.permission(slot, false, "io::read_into")?;
    let mut bytes = bytes_borrow_mut(&args[1], "io::read_into")?;
    let span = range(&args[2], &args[3], bytes.len())?;
    if span.is_empty() {
        return Ok(Value::Int(0));
    }
    if context.pending_output(slot) {
        context.flush_slot(slot)?;
    }
    let resource = context.slots[slot].resource.as_mut().unwrap();
    let result = loop {
        let result = match &mut resource.stream {
            Stream::Stdin => match &mut context.buffered_input {
                Some(input) => input.read(&mut bytes[span.clone()]),
                None => context
                    .input
                    .as_mut()
                    .unwrap()
                    .read(&mut bytes[span.clone()]),
            },
            Stream::File(FileStream::Direct(file)) => file.read(&mut bytes[span.clone()]),
            Stream::File(FileStream::Buffered(file)) => file.file.read(&mut bytes[span.clone()]),
            Stream::Stdout => unreachable!(),
        };
        if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::Interrupted) {
            break result;
        }
    };
    result
        .map(|n| Value::Int(n as i64))
        .map_err(|error| io_error("io::read_into", error))
}
fn write_bytes(
    context: &mut RuntimeContext<'_>,
    slot: usize,
    bytes: &[u8],
    all: bool,
) -> Result<usize, RuntimeError> {
    context.permission(slot, true, "io::write")?;
    let resource = context.slots[slot].resource.as_mut().unwrap();
    let output: &mut dyn Write = match &mut resource.stream {
        Stream::Stdout => context.output,
        Stream::File(FileStream::Direct(file)) => file,
        Stream::File(FileStream::Buffered(file)) => file,
        Stream::Stdin => unreachable!(),
    };
    let was_dirty = resource.dirty;
    let result = if all {
        output.write_all(bytes).map(|()| bytes.len())
    } else {
        loop {
            match output.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result,
            }
        }
    };
    // The caller has received this failure. Automatic cleanup must not retry
    // the write or report it again; explicit flush/close can retry deliberately.
    resource.dirty = match result {
        Ok(count) => was_dirty || count > 0,
        Err(_) => false,
    };
    result.map_err(write_error)
}
fn write_common(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
    all: bool,
) -> Result<Value, RuntimeError> {
    let slot = context.slot(handle(&args[0]))?;
    let bytes = bytes_borrow(&args[1], "io::write")?;
    let span = range(&args[2], &args[3], bytes.len())?;
    let count = write_bytes(context, slot, &bytes[span], all)?;
    Ok(if all {
        Value::Nil
    } else {
        Value::Int(count as i64)
    })
}
fn write(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    write_common(context, args, false)
}
fn write_all(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    write_common(context, args, true)
}
fn flush(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let slot = context.slot(handle(&args[0]))?;
    context.flush_slot(slot)?;
    Ok(Value::Nil)
}
fn close(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let slot = context.slot(handle(&args[0]))?;
    context.close_slot(slot, true)?;
    Ok(Value::Nil)
}
fn seek(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    let slot = context.slot(handle(&args[0]))?;
    let resource = context.slots[slot].resource.as_ref().unwrap();
    context.permission(slot, !resource.readable, "io::seek")?;
    let offset = integer(&args[1]);
    let position = match string(&args[2]) {
        "start" => SeekFrom::Start(u64::try_from(offset).map_err(|_| invalid("io::seek"))?),
        "current" => SeekFrom::Current(offset),
        "end" => SeekFrom::End(offset),
        _ => return Err(invalid("io::seek")),
    };
    if context.pending_output(slot) {
        context.flush_slot(slot)?;
    }
    let resource = context.slots[slot].resource.as_mut().unwrap();
    let position = match &mut resource.stream {
        Stream::File(FileStream::Direct(file)) => file.seek(position),
        Stream::File(FileStream::Buffered(file)) => file.seek(position),
        _ => {
            return Err(io_error(
                "io::seek",
                io::Error::new(io::ErrorKind::Unsupported, "stream is not seekable"),
            ));
        }
    }
    .map_err(|error| io_error("io::seek", error))?;
    i64::try_from(position)
        .map(Value::Int)
        .map_err(|_| RuntimeError::Overflow)
}

#[cfg(test)]
mod limits_tests {
    use super::*;

    fn resource() -> Resource {
        Resource {
            stream: Stream::Stdout,
            readable: false,
            writable: true,
            dirty: false,
        }
    }

    #[test]
    fn buffer_cap_is_opt_in_and_still_enforced_when_set() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        let previous_default = 64 * 1024 * 1024;
        let above_previous_default = previous_default + 1;

        assert_eq!(context.limits.max_buffer_bytes, None);
        assert_eq!(
            checked_size(&context, above_previous_default as i64).unwrap(),
            above_previous_default
        );

        context.limits.max_buffer_bytes = Some(previous_default);
        assert!(matches!(
            checked_size(&context, above_previous_default as i64),
            Err(RuntimeError::ResourceLimit { resource: "bytes" })
        ));
    }

    #[test]
    fn bytes_to_str_checks_the_selected_slice_against_the_buffer_cap() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        // Existing host-owned bytes may be larger than a subsequent run's cap.
        let bytes =
            Value::Bytes(HeapRef::try_new(RefCell::new(vec![b'C', b'o', b'g', 0xff])).unwrap());
        let import = NativeImport::named("bytes::to_str").unwrap();
        context.limits.max_buffer_bytes = Some(2);
        assert!(matches!(
            import.call(&mut context, &[bytes.clone(), Value::Int(0), Value::Int(3)]),
            Err(RuntimeError::ResourceLimit { resource: "bytes" })
        ));
        let prefix = import
            .call(&mut context, &[bytes.clone(), Value::Int(0), Value::Int(2)])
            .unwrap();
        assert!(matches!(prefix, Value::Str(text) if text.as_ref() == "Co"));
        assert!(matches!(
            import.call(&mut context, &[bytes.clone(), Value::Int(3), Value::Int(1)]),
            Err(RuntimeError::ParseFailure {
                from: "bytes",
                to: "str"
            })
        ));
        context.limits.max_buffer_bytes = Some(0);
        let empty = import
            .call(&mut context, &[bytes.clone(), Value::Int(4), Value::Int(0)])
            .unwrap();
        assert!(matches!(empty, Value::Str(text) if text.is_empty()));
        context.limits.max_buffer_bytes = None;
        let complete = import
            .call(&mut context, &[bytes, Value::Int(0), Value::Int(3)])
            .unwrap();
        assert!(matches!(complete, Value::Str(text) if text.as_ref() == "Cog"));
    }

    #[test]
    fn resource_slot_cap_is_opt_in_and_still_enforced_when_set() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        assert_eq!(context.limits.max_resources, None);

        for _ in 0..1024 {
            context.insert(resource()).unwrap();
        }
        assert!(context.slots.len() > 1024);

        context.limits.max_resources = Some(context.slots.len());
        assert!(matches!(
            context.insert(resource()),
            Err(RuntimeError::ResourceLimit {
                resource: "handles"
            })
        ));
    }

    #[test]
    fn resource_slot_growth_is_reserved_and_closed_slots_are_reused() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        let initial_len = context.slots.len();

        context.reserve_resource_slot().unwrap();
        assert!(context.slots.capacity() > initial_len);
        let reserved_capacity = context.slots.capacity();

        let Value::Handle(first) = context.insert(resource()).unwrap() else {
            panic!("resource insertion must return a handle");
        };
        let first_slot = context.slot(&first).unwrap();
        assert_eq!(context.slots.capacity(), reserved_capacity);

        context.close_slot(first_slot, false).unwrap();
        let Value::Handle(second) = context.insert(resource()).unwrap() else {
            panic!("resource insertion must return a handle");
        };
        assert_eq!(second.slot, first.slot);
        assert_ne!(second.generation, first.generation);
        assert_eq!(context.slots.capacity(), reserved_capacity);
    }
}
