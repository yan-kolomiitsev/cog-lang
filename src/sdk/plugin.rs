use super::abi::{self, Bytes, Handle, HostV1, Status, ValueView};
use super::{
    Effects, Engine, Error, ErrorKind, NativeContext, Signature, Type, Value, entry_allowed,
};
use crate::value::Value as VmValue;
use std::cell::Cell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAX_HANDLES: usize = 4096;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
fn failure(message: impl Into<String>) -> Error {
    Error::at(ErrorKind::Native, message)
}

impl Type {
    fn from_tag(tag: u8) -> Result<Self, Error> {
        Ok(match tag {
            0 => Self::Any,
            1 => Self::Int,
            2 => Self::Str,
            3 => Self::Bytes,
            4 => Self::Handle,
            5 => Self::Nil,
            6 => Self::Number,
            7 => Self::Array,
            8 => Self::Record,
            9 => Self::Float,
            10 => Self::Bool,
            11 => Self::StrBuf,
            _ => return Err(failure("unknown native type tag")),
        })
    }
    fn from_name(name: &str) -> Result<Self, Error> {
        Ok(match name {
            "any" => Self::Any,
            "int" => Self::Int,
            "str" => Self::Str,
            "bytes" => Self::Bytes,
            "handle" => Self::Handle,
            "nil" => Self::Nil,
            "number" => Self::Number,
            "array" => Self::Array,
            "record" => Self::Record,
            "float" => Self::Float,
            "bool" => Self::Bool,
            "strbuf" => Self::StrBuf,
            _ => return Err(failure("unknown native type name")),
        })
    }
}

/// Explicit CLI plugin metadata. Parsing/checking never loads native code.
/// Library paths are resolved relative to the manifest file by the CLI.
#[derive(Clone, Debug)]
pub struct PluginManifest {
    pub library: PathBuf,
    functions: Vec<(String, Signature)>,
}
impl PluginManifest {
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > crate::config::MAX_CONFIGURATION_BYTES {
            return Err(failure("plugin manifest exceeds 256 KiB"));
        }
        crate::limits::check_kdl_depth(text).map_err(failure)?;
        let document =
            kdl::KdlDocument::parse_v2(text).map_err(|error| failure(error.to_string()))?;
        let mut library = None;
        let mut functions = Vec::new();
        for node in document.nodes() {
            if node.ty().is_some() || node.entries().iter().any(|entry| entry.ty().is_some()) {
                return Err(failure("plugin manifest does not accept type annotations"));
            }
            match node.name().value() {
                "plugin" if library.is_none() && node.children().is_none() => {
                    if node.entries().len() != 2
                        || node.get("version").and_then(|v| v.as_integer()) != Some(1)
                    {
                        return Err(failure("plugin requires version=1 and library=\"path\""));
                    }
                    let path = node
                        .get("library")
                        .and_then(|v| v.as_string())
                        .filter(|p| !p.is_empty() && !p.contains('\0'))
                        .ok_or_else(|| failure("plugin requires a library path"))?;
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        return Err(failure(
                            "plugin library path must be relative to the manifest",
                        ));
                    }
                    library = Some(path);
                }
                "function" if functions.len() < 4096 => {
                    if node.entries().len() != 2 || node.entries()[0].name().is_some() {
                        return Err(failure(
                            "function requires a positional name and result=\"type\"",
                        ));
                    }
                    let name = node
                        .get(0)
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| failure("function requires a name"))?;
                    let result = node
                        .get("result")
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| failure("function requires a result type"))?;
                    let mut signature = Signature::new(Vec::new(), Type::from_name(result)?);
                    if let Some(children) = node.children() {
                        for child in children.nodes() {
                            if child.ty().is_some()
                                || child.children().is_some()
                                || child.entries().len() != 1
                                || child.entries()[0].name().is_some()
                                || child.entries()[0].ty().is_some()
                            {
                                return Err(failure("invalid function metadata"));
                            }
                            match child.name().value() {
                                "param" if signature.parameters.len() < 64 => {
                                    signature.parameters.push(Type::from_name(
                                        child
                                            .get(0)
                                            .and_then(|v| v.as_string())
                                            .ok_or_else(|| failure("param requires a type"))?,
                                    )?);
                                }
                                "effect" => {
                                    let effect = match child.get(0).and_then(|v| v.as_string()) {
                                        Some("output") => Effects::OUTPUT,
                                        Some("input") => Effects::INPUT,
                                        Some("fs_read") => Effects::FS_READ,
                                        Some("fs_write") => Effects::FS_WRITE,
                                        Some("process") => Effects::PROCESS,
                                        Some("process_info") => Effects::PROCESS_INFO,
                                        Some("clock") => Effects::CLOCK,
                                        Some("sleep") => Effects::SLEEP,
                                        Some("entropy") => Effects::ENTROPY,
                                        _ => return Err(failure("unknown native effect")),
                                    };
                                    if signature.effects.contains(effect) {
                                        return Err(failure("duplicate native effect"));
                                    }
                                    signature.effects = signature.effects | effect;
                                }
                                "mutates" => signature.mutating_parameters.push(
                                    usize::try_from(
                                        child
                                            .get(0)
                                            .and_then(|v| v.as_integer())
                                            .ok_or_else(|| failure("mutates requires an index"))?,
                                    )
                                    .map_err(|_| failure("invalid mutation index"))?,
                                ),
                                _ => return Err(failure("unknown or excessive function metadata")),
                            }
                        }
                    }
                    signature.identity(name)?;
                    if functions.iter().any(|(existing, _)| existing == name) {
                        return Err(failure("duplicate native function"));
                    }
                    functions.push((name.to_owned(), signature));
                }
                _ => return Err(failure("unknown or duplicate plugin manifest node")),
            }
        }
        Ok(Self {
            library: library.ok_or_else(|| failure("missing plugin node"))?,
            functions,
        })
    }
    pub fn read(path: &Path) -> Result<Self, Error> {
        let bytes = crate::limits::read_at_most(
            std::fs::File::open(path).map_err(|e| failure(e.to_string()))?,
            crate::config::MAX_CONFIGURATION_BYTES + 1,
        )
        .map_err(|e| failure(e.to_string()))?;
        Self::parse(std::str::from_utf8(&bytes).map_err(|e| failure(e.to_string()))?)
    }
    pub fn functions(&self) -> &[(String, Signature)] {
        &self.functions
    }
    pub fn register_metadata(&self, engine: &mut Engine) -> Result<(), Error> {
        let mut next = engine.clone();
        for (name, signature) in &self.functions {
            next.register_metadata(name, signature.clone())?;
        }
        *engine = next;
        Ok(())
    }
    /// Load the library only after checking its complete descriptor against this
    /// manifest. The library initializer runs when the OS loads it.
    ///
    /// # Safety
    /// This executes trusted native code with process authority. The library and
    /// all transitive dependencies must obey C ABI v1, including pointer validity,
    /// no unwind across C, synchronous calls and context/state ownership.
    pub unsafe fn load(&self, engine: &mut Engine, manifest_directory: &Path) -> Result<(), Error> {
        if self.library.is_absolute() {
            return Err(failure(
                "plugin library path must be relative to the manifest",
            ));
        }
        unsafe {
            load(
                engine,
                &manifest_directory.join(&self.library),
                Some(&self.functions),
            )
        }
    }
}

struct LibraryState {
    state: *mut c_void,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    _library: libloading::Library,
}
impl Drop for LibraryState {
    fn drop(&mut self) {
        // The library field remains alive until after this destructor returns.
        if let Some(destroy) = self.destroy {
            unsafe {
                destroy(self.state);
            }
        }
    }
}

impl Engine {
    /// Load a trusted C ABI v1 plugin and register its descriptor's functions.
    /// Source check/build tools should use PluginManifest metadata instead.
    ///
    /// # Safety
    /// Same requirements as PluginManifest::load. Size/version checks detect
    /// compatibility mistakes; they cannot validate arbitrary native pointers.
    pub unsafe fn load_plugin(&mut self, path: &Path) -> Result<(), Error> {
        unsafe { load(self, path, None) }
    }
}

// The unsafe caller guarantees descriptor storage for the lifetime of library.
unsafe fn view<'a>(bytes: Bytes, limit: usize) -> Result<&'a [u8], Error> {
    if bytes.len > limit || (bytes.len != 0 && bytes.data.is_null()) {
        return Err(failure("invalid plugin byte view"));
    }
    if bytes.len == 0 {
        Ok(&[])
    } else {
        Ok(unsafe { std::slice::from_raw_parts(bytes.data, bytes.len) })
    }
}

unsafe fn load(
    engine: &mut Engine,
    path: &Path,
    expected: Option<&[(String, Signature)]>,
) -> Result<(), Error> {
    entry_allowed()?;
    // Bare names must select the explicit manifest/CWD path, without relying
    // on the platform loader's ambient library search directories.
    let path = path
        .canonicalize()
        .map_err(|error| failure(format!("cannot resolve plugin {}: {error}", path.display())))?;
    let library = unsafe { libloading::Library::new(&path) }.map_err(|e| failure(e.to_string()))?;
    let entry: libloading::Symbol<'_, unsafe extern "C" fn() -> *const abi::PluginV1> =
        unsafe { library.get(b"cog_plugin_v1\0") }.map_err(|e| failure(e.to_string()))?;
    let descriptor = unsafe { entry() };
    if descriptor.is_null()
        || unsafe { descriptor.cast::<u32>().read() } != size_of::<abi::PluginV1>() as u32
    {
        return Err(failure("invalid plugin descriptor size"));
    }
    let descriptor = unsafe { &*descriptor };
    if descriptor.version != abi::VERSION
        || descriptor.function_count > 4096
        || (descriptor.function_count != 0 && descriptor.functions.is_null())
        || descriptor.create.is_some() != descriptor.destroy.is_some()
    {
        return Err(failure("unsupported plugin ABI or lifecycle"));
    }
    let mut functions = Vec::new();
    let mut metadata_bytes = 0usize;
    for index in 0..descriptor.function_count {
        let pointer = unsafe { descriptor.functions.add(index) };
        if unsafe { pointer.cast::<u32>().read() } != size_of::<abi::FunctionV1>() as u32 {
            return Err(failure("invalid plugin function descriptor size"));
        }
        let function = unsafe { &*pointer };
        metadata_bytes = metadata_bytes
            .checked_add(function.name.len)
            .and_then(|v| v.checked_add(function.parameters.len))
            .filter(|v| *v <= MAX_METADATA_BYTES)
            .ok_or_else(|| failure("plugin metadata budget exceeded"))?;
        let name = std::str::from_utf8(unsafe { view(function.name, 256)? })
            .map_err(|e| failure(e.to_string()))?
            .to_owned();
        let parameters = unsafe { view(function.parameters, 64)? }
            .iter()
            .map(|tag| Type::from_tag(*tag))
            .collect::<Result<Vec<_>, _>>()?;
        let signature = Signature::new(parameters, Type::from_tag(function.result)?)
            .with_effects(Effects::from_bits(function.effects)?)
            .with_mutating_parameters(
                (0..64)
                    .filter(|index| function.mutations & (1u64 << index) != 0)
                    .collect(),
            );
        signature.identity(&name)?;
        let invoke = function
            .invoke
            .ok_or_else(|| failure("missing native callback"))?;
        if functions.iter().any(|(existing, _, _)| existing == &name) {
            return Err(failure("duplicate plugin function"));
        }
        functions.push((name, signature, invoke));
    }
    if let Some(expected) = expected
        && (expected.len() != functions.len()
            || expected.iter().any(|(name, signature)| {
                !functions.iter().any(|(actual, actual_signature, _)| {
                    actual == name
                        && actual_signature.identity(actual).ok() == signature.identity(name).ok()
                })
            }))
    {
        return Err(failure("plugin descriptor does not match the manifest"));
    }
    // Validate the entire registration before executing create or mutating Engine.
    let mut trial = engine.clone();
    for (name, signature, _) in &functions {
        trial.register_metadata(name, signature.clone())?;
    }
    let state = Rc::new(LibraryState {
        state: std::ptr::null_mut(),
        destroy: descriptor.destroy,
        _library: library,
    });
    let mut state = state;
    if let Some(create) = descriptor.create {
        let status = unsafe { create(&mut Rc::get_mut(&mut state).unwrap().state) };
        if status != abi::OK {
            return Err(failure(format!("plugin initialization failed ({status})")));
        }
    }
    let mut next = engine.clone();
    for (name, signature, invoke) in functions {
        let state = state.clone();
        next.register(&name, signature, move |context, arguments| {
            call(&state, invoke, context, arguments)
        })?;
    }
    *engine = next;
    Ok(())
}

thread_local! { static NEXT_CALL: Cell<u32> = const { Cell::new(1) }; }
fn next_call_id() -> Result<u32, Error> {
    NEXT_CALL.with(|next| {
        let id = next.get();
        if id == 0 {
            return Err(failure("native call generation exhausted"));
        }
        next.set(id.checked_add(1).unwrap_or(0));
        Ok(id)
    })
}
struct Frame<'a, 'b, 'c> {
    context: &'a mut NativeContext<'b, 'c>,
    id: u32,
    values: Vec<Value>,
    error: Option<String>,
    remaining_bytes: usize,
}
impl Frame<'_, '_, '_> {
    fn get(&self, handle: Handle) -> Result<&Value, Status> {
        if (handle >> 32) as u32 != self.id {
            return Err(abi::INVALID);
        }
        let index = (handle as u32).checked_sub(1).ok_or(abi::INVALID)? as usize;
        self.values.get(index).ok_or(abi::INVALID)
    }
    fn push(&mut self, value: Value) -> Result<Handle, Status> {
        if self.values.len() >= MAX_HANDLES {
            return Err(abi::LIMIT);
        }
        self.values.try_reserve(1).map_err(|_| abi::LIMIT)?;
        self.values.push(value);
        Ok(((self.id as u64) << 32) | self.values.len() as u64)
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Status> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes).ok_or(abi::LIMIT)?;
        Ok(())
    }
}

fn call(
    state: &LibraryState,
    invoke: abi::Callback,
    context: &mut NativeContext<'_, '_>,
    arguments: &[Value],
) -> Result<Value, Error> {
    let id = next_call_id()?;
    let remaining_bytes = context.inner.limits.buffer_limit();
    let mut frame = Frame {
        context,
        id,
        values: Vec::new(),
        error: None,
        remaining_bytes,
    };
    let mut handles = [0; 64];
    for (index, value) in arguments.iter().enumerate() {
        handles[index] = frame
            .push(value.clone())
            .map_err(|_| failure("native handle budget exceeded"))?;
    }
    let mut result = 0;
    let status = unsafe {
        invoke(
            state.state,
            &HOST,
            (&mut frame as *mut Frame<'_, '_, '_>).cast(),
            handles.as_ptr(),
            arguments.len(),
            &mut result,
        )
    };
    if status != abi::OK || frame.error.is_some() {
        return Err(failure(
            frame
                .error
                .unwrap_or_else(|| format!("native plugin failed ({status})")),
        ));
    }
    frame
        .get(result)
        .cloned()
        .map_err(|_| failure("native plugin returned an invalid or expired value handle"))
}

fn guarded(action: impl FnOnce() -> Result<(), Status>) -> Status {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Ok(Ok(())) => abi::OK,
        Ok(Err(status)) => status,
        Err(_) => abi::PANIC,
    }
}
// A trusted caller may only use this pointer synchronously on the calling thread.
unsafe fn frame<'a>(context: *mut c_void) -> &'a mut Frame<'a, 'a, 'a> {
    unsafe { &mut *context.cast::<Frame<'a, 'a, 'a>>() }
}
unsafe extern "C" fn inspect(
    context: *mut c_void,
    handle: Handle,
    output: *mut ValueView,
) -> Status {
    guarded(|| {
        if context.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let value = unsafe { frame(context) }.get(handle)?;
        let mut view = ValueView::default();
        view.kind = match &value.0 {
            VmValue::Int(v) => {
                view.integer = *v;
                Type::Int as u32
            }
            VmValue::Float(v) => {
                view.number = *v;
                Type::Float as u32
            }
            VmValue::Bool(v) => {
                view.integer = i64::from(*v);
                Type::Bool as u32
            }
            VmValue::Str(v) => {
                view.bytes = Bytes::new(v.as_bytes());
                Type::Str as u32
            }
            VmValue::Nil => Type::Nil as u32,
            VmValue::Array(v) => {
                view.len = v.try_borrow().map_err(|_| abi::INVALID)?.len();
                Type::Array as u32
            }
            VmValue::Error(_) => abi::ERROR_VALUE,
            _ => abi::OPAQUE_VALUE,
        };
        unsafe {
            output.write(view);
        }
        Ok(())
    })
}
unsafe extern "C" fn make(
    context: *mut c_void,
    input: *const ValueView,
    output: *mut Handle,
) -> Status {
    guarded(|| {
        if context.is_null() || input.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let input = unsafe { &*input };
        let value = match input.kind {
            1 => Value::int(input.integer),
            9 => Value::float(input.number),
            10 if input.integer == 0 || input.integer == 1 => Value::boolean(input.integer != 0),
            5 => Value::nil(),
            2 => {
                frame.charge(input.bytes.len)?;
                let bytes =
                    unsafe { view(input.bytes, isize::MAX as usize) }.map_err(|_| abi::INVALID)?;
                Value::string(std::str::from_utf8(bytes).map_err(|_| abi::INVALID)?)
            }
            7 => {
                if input.len > MAX_HANDLES || (input.len != 0 && input.items.is_null()) {
                    return Err(abi::LIMIT);
                }
                frame.charge(
                    input
                        .len
                        .checked_mul(size_of::<Value>())
                        .ok_or(abi::LIMIT)?,
                )?;
                let items = if input.len == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(input.items, input.len) }
                };
                let mut values = Vec::new();
                values.try_reserve(items.len()).map_err(|_| abi::LIMIT)?;
                for handle in items {
                    values.push(frame.get(*handle)?.clone());
                }
                Value::array(values).map_err(|_| abi::TYPE_MISMATCH)?
            }
            _ => return Err(abi::TYPE_MISMATCH),
        };
        let handle = frame.push(value)?;
        unsafe {
            output.write(handle);
        }
        Ok(())
    })
}
unsafe extern "C" fn array_get(
    context: *mut c_void,
    array: Handle,
    index: usize,
    output: *mut Handle,
) -> Status {
    guarded(|| {
        if context.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let value = frame
            .get(array)?
            .array_get(index)
            .map_err(|_| abi::INVALID)?;
        let handle = frame.push(value)?;
        unsafe {
            output.write(handle);
        }
        Ok(())
    })
}
unsafe extern "C" fn array_set(
    context: *mut c_void,
    array: Handle,
    index: usize,
    value: Handle,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        frame
            .get(array)?
            .array_set(index, frame.get(value)?.clone())
            .map_err(|_| abi::TYPE_MISMATCH)
    })
}
unsafe extern "C" fn write_output(context: *mut c_void, bytes: Bytes) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let bytes = unsafe { view(bytes, frame.context.inner.limits.buffer_limit()) }
            .map_err(|_| abi::LIMIT)?;
        frame
            .context
            .write_output(std::str::from_utf8(bytes).map_err(|_| abi::INVALID)?)
            .map_err(|_| abi::FAILURE)
    })
}
unsafe extern "C" fn set_error(context: *mut c_void, bytes: Bytes) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let bytes = unsafe { view(bytes, 64 * 1024) }.map_err(|_| abi::LIMIT)?;
        frame.error = Some(
            std::str::from_utf8(bytes)
                .map_err(|_| abi::INVALID)?
                .to_owned(),
        );
        Ok(())
    })
}
static HOST: HostV1 = HostV1 {
    size: size_of::<HostV1>() as u32,
    version: abi::VERSION,
    inspect,
    make,
    array_get,
    array_set,
    write_output,
    set_error,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_generations_remain_exhausted_without_wrapping() {
        let previous = NEXT_CALL.with(|next| next.replace(u32::MAX));
        assert_eq!(next_call_id().unwrap(), u32::MAX);
        assert!(next_call_id().is_err());
        assert!(next_call_id().is_err());
        NEXT_CALL.with(|next| next.set(previous));
    }
}
