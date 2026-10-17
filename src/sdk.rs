//! Owned embedding API and synchronous native registration for SDK ABI v1.
//! Engines, programs and values are thread confined. Native code is trusted.
use crate::runtime::{NativeRegistry, RegisteredNative, RuntimeContext, VmCapabilities};
use crate::value::Value as VmValue;
use std::cell::Cell;
use std::fmt;
use std::io::{Read, Write};
use std::rc::Rc;

pub mod abi;
mod plugin;
pub use plugin::PluginManifest;

pub const ABI_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Registration,
    Compilation,
    Bytecode,
    Runtime,
    Native,
    Reentrant,
}

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self::at(ErrorKind::Native, message)
    }
    fn at(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

/// Stable coarse type tags shared with the plugin ABI. Error is accepted at
/// typed boundaries and is propagated before invoking a typed native callback.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Any = 0,
    Int = 1,
    Str = 2,
    Bytes = 3,
    Handle = 4,
    Nil = 5,
    Number = 6,
    Array = 7,
    Record = 8,
    Float = 9,
    Bool = 10,
    StrBuf = 11,
}
impl Type {
    fn runtime(self) -> crate::runtime::NativeType {
        use crate::runtime::NativeType as N;
        match self {
            Self::Any => N::Any,
            Self::Int => N::Int,
            Self::Str => N::Str,
            Self::Bytes => N::Bytes,
            Self::Handle => N::Handle,
            Self::Nil => N::Nil,
            Self::Number => N::Number,
            Self::Array => N::Array,
            Self::Record => N::Record,
            Self::Float => N::Float,
            Self::Bool => N::Bool,
            Self::StrBuf => N::StrBuf,
        }
    }
}

/// Declared host effects. These support optional host policy; they do not
/// constrain arbitrary OS operations performed by trusted Rust/native code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects(u16);
impl Effects {
    pub const PURE: Self = Self(0);
    pub const OUTPUT: Self = Self(1 << 0);
    pub const INPUT: Self = Self(1 << 1);
    pub const FS_READ: Self = Self(1 << 2);
    pub const FS_WRITE: Self = Self(1 << 3);
    pub const PROCESS: Self = Self(1 << 4);
    pub const PROCESS_INFO: Self = Self(1 << 5);
    pub const CLOCK: Self = Self(1 << 6);
    pub const SLEEP: Self = Self(1 << 7);
    pub const ENTROPY: Self = Self(1 << 8);
    pub const ALL: Self = Self((1 << 9) - 1);
    pub const fn bits(self) -> u16 {
        self.0
    }
    pub fn from_bits(bits: u16) -> Result<Self, Error> {
        if bits & !Self::ALL.0 != 0 {
            return Err(Error::new("unknown native effect flags"));
        }
        Ok(Self(bits))
    }
    fn contains(self, effect: Self) -> bool {
        self.0 & effect.0 == effect.0
    }
}
impl std::ops::BitOr for Effects {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Permissions {
    allowed: Effects,
}
impl Permissions {
    pub const fn all() -> Self {
        Self {
            allowed: Effects::ALL,
        }
    }
    pub const fn none() -> Self {
        Self {
            allowed: Effects::PURE,
        }
    }
    pub fn allow(mut self, effects: Effects) -> Self {
        self.allowed = self.allowed | effects;
        self
    }
    fn runtime(self) -> VmCapabilities {
        let has = |effect| self.allowed.contains(effect);
        VmCapabilities {
            allow_print: has(Effects::OUTPUT),
            allow_read: has(Effects::INPUT),
            allow_fs_read: has(Effects::FS_READ),
            allow_fs_write: has(Effects::FS_WRITE),
            allow_exec: has(Effects::PROCESS),
            allow_process_info: has(Effects::PROCESS_INFO),
            allow_time: has(Effects::CLOCK),
            allow_sleep: has(Effects::SLEEP),
            allow_random: has(Effects::ENTROPY),
            allow_asm: self.allowed == Effects::ALL,
        }
    }
    fn from_runtime(capabilities: VmCapabilities) -> Self {
        let mut allowed = Effects::PURE;
        for (enabled, effect) in [
            (capabilities.allow_print, Effects::OUTPUT),
            (capabilities.allow_read, Effects::INPUT),
            (capabilities.allow_fs_read, Effects::FS_READ),
            (capabilities.allow_fs_write, Effects::FS_WRITE),
            (capabilities.allow_exec, Effects::PROCESS),
            (capabilities.allow_process_info, Effects::PROCESS_INFO),
            (capabilities.allow_time, Effects::CLOCK),
            (capabilities.allow_sleep, Effects::SLEEP),
            (capabilities.allow_random, Effects::ENTROPY),
        ] {
            if enabled {
                allowed = allowed | effect;
            }
        }
        Self { allowed }
    }
}
impl Default for Permissions {
    fn default() -> Self {
        Self::all()
    }
}

#[derive(Clone, Debug)]
pub struct Signature {
    pub parameters: Vec<Type>,
    pub result: Type,
    pub effects: Effects,
    pub mutating_parameters: Vec<usize>,
}
impl Signature {
    pub fn new(parameters: Vec<Type>, result: Type) -> Self {
        Self {
            parameters,
            result,
            effects: Effects::PURE,
            mutating_parameters: Vec::new(),
        }
    }
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.effects = effects;
        self
    }
    pub fn with_mutating_parameters(mut self, parameters: Vec<usize>) -> Self {
        self.mutating_parameters = parameters;
        self
    }
    fn identity(&self, name: &str) -> Result<String, Error> {
        if name.len() > 256
            || name.split("::").count() != 2
            || !name.split("::").all(crate::config::identifier)
            || self.parameters.len() > 64
            || self
                .mutating_parameters
                .iter()
                .any(|index| *index >= self.parameters.len())
            || self
                .mutating_parameters
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(Error::at(
                ErrorKind::Registration,
                "invalid native name, arity or mutation metadata",
            ));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cog-native-metadata-v1\0");
        hasher.update(&ABI_VERSION.to_le_bytes());
        hasher.update(&(name.len() as u32).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&(self.parameters.len() as u16).to_le_bytes());
        for parameter in &self.parameters {
            hasher.update(&[*parameter as u8]);
        }
        hasher.update(&[self.result as u8]);
        hasher.update(&self.effects.bits().to_le_bytes());
        let mutations = self
            .mutating_parameters
            .iter()
            .fold(0u64, |bits, index| bits | (1u64 << index));
        hasher.update(&mutations.to_le_bytes());
        Ok(format!("$native::{}::{name}", hasher.finalize().to_hex()))
    }
}

/// An owned Cog value. Cloning retains a graph root and preserves shared aliases.
/// Resource handles retain their context/generation checks; no host pointer is exposed.
#[derive(Clone)]
pub struct Value(pub(crate) VmValue);
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Value {
    pub fn nil() -> Self {
        Self(VmValue::Nil)
    }
    pub fn int(value: i64) -> Self {
        Self(VmValue::Int(value))
    }
    pub fn float(value: f64) -> Self {
        Self(VmValue::Float(value))
    }
    pub fn boolean(value: bool) -> Self {
        Self(VmValue::Bool(value))
    }
    pub fn string(value: impl AsRef<str>) -> Self {
        Self(VmValue::Str(Rc::from(value.as_ref())))
    }
    pub fn as_int(&self) -> Result<i64, Error> {
        if let VmValue::Int(value) = self.0 {
            Ok(value)
        } else {
            Err(Error::new("expected int"))
        }
    }
    pub fn as_float(&self) -> Result<f64, Error> {
        if let VmValue::Float(value) = self.0 {
            Ok(value)
        } else {
            Err(Error::new("expected float"))
        }
    }
    pub fn as_bool(&self) -> Result<bool, Error> {
        if let VmValue::Bool(value) = self.0 {
            Ok(value)
        } else {
            Err(Error::new("expected bool"))
        }
    }
    pub fn as_str(&self) -> Result<&str, Error> {
        if let VmValue::Str(value) = &self.0 {
            Ok(value)
        } else {
            Err(Error::new("expected str"))
        }
    }
    pub fn is_error(&self) -> bool {
        matches!(self.0, VmValue::Error(_))
    }
    pub fn array(values: Vec<Self>) -> Result<Self, Error> {
        let mut items = Vec::new();
        items
            .try_reserve(values.len())
            .map_err(|_| Error::new("cannot allocate SDK array"))?;
        items.extend(values.into_iter().map(|value| value.0));
        let array = crate::value::ArrayValue::new(items)
            .map_err(|_| Error::new("incompatible array elements"))?;
        crate::heap::HeapRef::try_new(array)
            .map(|array| Self(VmValue::Array(array)))
            .map_err(|error| Error::new(error.to_string()))
    }
    pub fn array_get(&self, index: usize) -> Result<Self, Error> {
        let VmValue::Array(array) = &self.0 else {
            return Err(Error::new("expected array"));
        };
        array
            .try_borrow()
            .map_err(|_| Error::new("array borrow conflict"))?
            .get(index)
            .cloned()
            .map(Self)
            .ok_or_else(|| Error::new("array index out of bounds"))
    }
    pub fn array_set(&self, index: usize, value: Self) -> Result<(), Error> {
        let VmValue::Array(array) = &self.0 else {
            return Err(Error::new("expected array"));
        };
        array
            .set(index, value.0)
            .map_err(|_| Error::new("array update rejected"))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Optional buffer/rendering/serialization byte cap. Defaults to `None`.
    pub max_buffer_bytes: Option<usize>,
    /// Optional live host resource-slot cap, including standard streams.
    pub max_resources: Option<usize>,
    pub max_steps: Option<u64>,
    pub max_operand_values: Option<usize>,
    pub max_call_frames: Option<usize>,
    pub max_slots: Option<usize>,
    pub max_heap_objects: Option<usize>,
}
impl Default for Limits {
    fn default() -> Self {
        let limits = crate::runtime::RuntimeLimits::default();
        Self {
            max_buffer_bytes: limits.max_buffer_bytes,
            max_resources: limits.max_resources,
            max_steps: None,
            max_operand_values: None,
            max_call_frames: None,
            max_slots: None,
            max_heap_objects: None,
        }
    }
}
impl Limits {
    fn runtime(self) -> crate::runtime::RuntimeLimits {
        crate::runtime::RuntimeLimits {
            max_buffer_bytes: self.max_buffer_bytes,
            max_resources: self.max_resources,
            max_steps: self.max_steps,
            max_operand_values: self.max_operand_values,
            max_call_frames: self.max_call_frames,
            max_slots: self.max_slots,
            max_heap_objects: self.max_heap_objects,
        }
    }
}

/// State for one run. Context-owned file resources are finalized by the VM;
/// retained handles cannot revive resources after the context is dropped.
pub struct RunContext<'a> {
    inner: RuntimeContext<'a>,
}
impl<'a> RunContext<'a> {
    pub fn new(output: &'a mut dyn Write) -> Self {
        Self {
            inner: RuntimeContext::new(output, VmCapabilities::default()),
        }
    }
    pub fn with_input(output: &'a mut dyn Write, input: impl Read + 'a) -> Self {
        Self {
            inner: RuntimeContext::with_input(output, Box::new(input), VmCapabilities::default()),
        }
    }
    pub fn with_permissions(mut self, permissions: Permissions) -> Self {
        self.inner.capabilities = permissions.runtime();
        self
    }
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.inner.limits = limits.runtime();
        self
    }
    pub fn set_process_arguments(&mut self, arguments: Vec<std::ffi::OsString>) {
        self.inner.set_process_arguments(arguments);
    }
}

/// Borrowed only for the synchronous callback. Keep owned Value clones to retain
/// values; this context and its I/O references cannot be retained by safe callbacks.
pub struct NativeContext<'call, 'io> {
    inner: &'call mut RuntimeContext<'io>,
}
impl NativeContext<'_, '_> {
    pub fn write_output(&mut self, text: &str) -> Result<(), Error> {
        self.inner
            .write_native_output(text)
            .map_err(|error| Error::new(error.to_string()))
    }
    pub fn read_line(&mut self) -> Result<Value, Error> {
        crate::runtime::NativeImport::named("io::read_line")
            .map_err(Error::new)?
            .call(self.inner, &[])
            .map(Value)
            .map_err(|error| Error::new(error.to_string()))
    }
}

thread_local! { static IN_CALLBACK: Cell<bool> = const { Cell::new(false) }; }
fn entry_allowed() -> Result<(), Error> {
    if IN_CALLBACK.with(Cell::get) {
        Err(Error::at(
            ErrorKind::Reentrant,
            "SDK v1 does not allow VM reentry",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct Engine {
    registry: Rc<NativeRegistry>,
}
impl Engine {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(
        &mut self,
        name: &str,
        signature: Signature,
        callback: impl Fn(&mut NativeContext<'_, '_>, &[Value]) -> Result<Value, Error> + 'static,
    ) -> Result<(), Error> {
        let label = name.to_owned();
        let effects = signature.effects;
        let invoke: Rc<crate::runtime::NativeCallback> = Rc::new(move |context, arguments| {
            use crate::error::RuntimeError;
            if !Permissions::from_runtime(context.capabilities)
                .allowed
                .contains(effects)
            {
                return Err(RuntimeError::PermissionDenied {
                    op: "native extension",
                });
            }
            if IN_CALLBACK.with(|active| active.replace(true)) {
                return Err(RuntimeError::NativeFailure {
                    message: "SDK v1 does not allow native reentry".into(),
                });
            }
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    IN_CALLBACK.with(|active| active.set(false));
                }
            }
            let _reset = Reset;
            let mut values = Vec::new();
            values
                .try_reserve(arguments.len())
                .map_err(|_| RuntimeError::ResourceLimit {
                    resource: "native arguments",
                })?;
            values.extend(arguments.iter().cloned().map(Value));
            let mut context = NativeContext { inner: context };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(&mut context, &values)
            })) {
                Ok(Ok(value)) => Ok(value.0),
                Ok(Err(error)) => Err(RuntimeError::NativeFailure {
                    message: format!("{label}: {error}"),
                }),
                Err(_) => Err(RuntimeError::NativeFailure {
                    message: format!("{label}: callback panicked"),
                }),
            }
        });
        self.insert(name, signature, Some(invoke))
    }
    /// Register compiler metadata only; no library is loaded and execution must
    /// later resolve the exact identity against a registry with executable code.
    pub fn register_metadata(&mut self, name: &str, signature: Signature) -> Result<(), Error> {
        self.insert(name, signature, None)
    }
    fn insert(
        &mut self,
        name: &str,
        signature: Signature,
        invoke: Option<Rc<crate::runtime::NativeCallback>>,
    ) -> Result<(), Error> {
        entry_allowed()?;
        let identity = signature.identity(name)?;
        if crate::runtime::builtin_module(name.split_once("::").unwrap().0)
            || self.registry.names.contains_key(name)
            || self.registry.names.len() >= 4096
        {
            return Err(Error::at(
                ErrorKind::Registration,
                "duplicate, reserved or excessive native registration",
            ));
        }
        let function = Rc::new(RegisteredNative {
            source_name: name.to_owned(),
            identity: identity.clone(),
            version: ABI_VERSION,
            parameters: signature
                .parameters
                .iter()
                .map(|parameter| parameter.runtime())
                .collect(),
            result: signature.result.runtime(),
            mutating_parameters: signature.mutating_parameters,
            invoke,
        });
        let registry = Rc::make_mut(&mut self.registry);
        registry.names.insert(name.to_owned(), function.clone());
        registry.identities.insert(identity, function);
        Ok(())
    }
    pub fn compile(&self, name: &str, source: &str) -> Result<Program, Error> {
        entry_allowed()?;
        crate::runtime::with_registry(self.registry.clone(), || {
            crate::project::compile_source(name, source)
        })
        .map(|inner| Program { inner })
        .map_err(|error| Error::at(ErrorKind::Compilation, error))
    }
    pub fn compile_project(
        &self,
        path: &std::path::Path,
        target: Option<&str>,
        locked: bool,
    ) -> Result<Program, Error> {
        entry_allowed()?;
        crate::runtime::with_registry(self.registry.clone(), || {
            let loaded = crate::project::load(path, target, locked)
                .map_err(|errors| crate::frontend::render(&errors))?;
            loaded.compile()
        })
        .map(|inner| Program { inner })
        .map_err(|error| Error::at(ErrorKind::Compilation, error))
    }
    pub fn decode(&self, bytes: &[u8]) -> Result<Program, Error> {
        entry_allowed()?;
        crate::runtime::with_registry(self.registry.clone(), || crate::bytecode::decode(bytes))
            .map(|inner| Program { inner })
            .map_err(|error| Error::at(ErrorKind::Bytecode, error))
    }
}

pub struct Program {
    inner: crate::backend::CompiledProgram,
}
impl Program {
    pub fn disassemble(&self) -> String {
        self.inner.disassemble()
    }
    pub fn write(&self, path: &std::path::Path) -> Result<(), Error> {
        crate::build::write_bytecode(path, &self.inner)
            .map_err(|error| Error::at(ErrorKind::Bytecode, error))
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::bytecode::encode(&self.inner).map_err(|error| Error::at(ErrorKind::Bytecode, error))
    }
    pub fn run(&self, output: &mut dyn Write) -> Result<(), Error> {
        self.run_in(&mut RunContext::new(output))
    }
    pub fn run_in(&self, context: &mut RunContext<'_>) -> Result<(), Error> {
        entry_allowed()?;
        if let Some(import) = self.inner.imports.iter().find(|import| !import.is_linked()) {
            return Err(Error::at(
                ErrorKind::Runtime,
                format!("native import `{}` is not linked", import.name()),
            ));
        }
        crate::interpreter::run_with_context(&self.inner, &mut context.inner)
            .map_err(|error| Error::at(ErrorKind::Runtime, error.to_string()))
    }
}

pub struct Build {
    pub program: Program,
    pub reused: bool,
}
impl Engine {
    pub fn read(&self, input: impl Read) -> Result<Program, Error> {
        entry_allowed()?;
        crate::runtime::with_registry(self.registry.clone(), || crate::bytecode::read(input))
            .map(|inner| Program { inner })
            .map_err(|error| Error::at(ErrorKind::Bytecode, error))
    }
    /// Build/cache a source project with this engine's registered metadata.
    /// Writes the requested artifact and generated resolution lock on success.
    pub fn build_project(
        &self,
        path: &std::path::Path,
        destination: &std::path::Path,
        target: Option<&str>,
        locked: bool,
    ) -> Result<Build, Error> {
        entry_allowed()?;
        crate::runtime::with_registry(self.registry.clone(), || {
            let prepared = crate::project::prepare(path, target, locked)
                .map_err(|errors| crate::frontend::render(&errors))?;
            crate::build::project(prepared, destination)
        })
        .map(|built| Build {
            program: Program {
                inner: built.program,
            },
            reused: built.reused,
        })
        .map_err(|error| Error::at(ErrorKind::Compilation, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_registry_restores_previous_scope_after_unwind() {
        let mut parent = Engine::new();
        parent
            .register_metadata("outer::value", Signature::new(vec![], Type::Nil))
            .unwrap();
        let mut nested = Engine::new();
        nested
            .register_metadata("inner::value", Signature::new(vec![], Type::Nil))
            .unwrap();
        crate::runtime::with_registry(parent.registry.clone(), || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::runtime::with_registry(nested.registry.clone(), || {
                    assert!(crate::runtime::NativeImport::named("inner::value").is_ok());
                    assert!(crate::runtime::NativeImport::named("outer::value").is_err());
                    panic!("scope test");
                });
            }));
            assert!(result.is_err());
            assert!(crate::runtime::NativeImport::named("outer::value").is_ok());
            assert!(crate::runtime::NativeImport::named("inner::value").is_err());
        });
        assert!(crate::runtime::NativeImport::named("outer::value").is_err());
    }

    #[test]
    fn bytecode_linking_requires_canonical_identity_and_escapes_unknown_names() {
        let mut engine = Engine::new();
        engine
            .register_metadata("host::value", Signature::new(vec![], Type::Nil))
            .unwrap();
        crate::runtime::with_registry(engine.registry.clone(), || {
            let import = crate::runtime::NativeImport::named("host::value").unwrap();
            assert!(
                crate::runtime::NativeImport::resolve(
                    "host::value",
                    import.version(),
                    import.parameters(),
                    import.result()
                )
                .is_err()
            );
            assert!(
                crate::runtime::NativeImport::resolve(
                    import.name(),
                    import.version(),
                    import.parameters(),
                    import.result()
                )
                .is_ok()
            );
        });
        let error = crate::runtime::NativeImport::named("unknown::\u{1b}[2J").unwrap_err();
        assert!(!error.contains('\u{1b}'));
    }
}
