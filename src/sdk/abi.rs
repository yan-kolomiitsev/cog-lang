//! Version 1 C plugin interface. See `include/cog_plugin.h` and `docs/SDK.md`.
//! Only C layouts, opaque call handles and borrowed views cross this boundary.
use super::{Error, Type};
use std::ffi::c_void;

pub type Handle = u64;
pub type Status = u32;
pub const OK: Status = 0;
pub const FAILURE: Status = 1;
pub const INVALID: Status = 2;
pub const TYPE_MISMATCH: Status = 3;
pub const LIMIT: Status = 4;
pub const PANIC: Status = 5;
pub const VERSION: u32 = 1;
pub const ERROR_VALUE: u32 = 12;
pub const OPAQUE_VALUE: u32 = 255;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    pub data: *const u8,
    pub len: usize,
}
impl Bytes {
    pub const fn new(bytes: &[u8]) -> Self {
        Self {
            data: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}
// Descriptors only borrow immutable plugin-owned storage; reading any pointer
// requires the unsafe ABI contract. Host code never writes through this view.
unsafe impl Sync for Bytes {}

/// Output from inspect, or input to make. Strings are UTF-8. For arrays make
/// reads `items`; inspect returns the length only (use array_get for elements).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ValueView {
    pub kind: u32,
    pub integer: i64,
    pub number: f64,
    pub bytes: Bytes,
    pub items: *const Handle,
    pub len: usize,
}
impl Default for ValueView {
    fn default() -> Self {
        Self {
            kind: Type::Nil as u32,
            integer: 0,
            number: 0.0,
            bytes: Bytes::new(&[]),
            items: std::ptr::null(),
            len: 0,
        }
    }
}

#[repr(C)]
pub struct HostV1 {
    pub size: u32,
    pub version: u32,
    pub inspect: unsafe extern "C" fn(*mut c_void, Handle, *mut ValueView) -> Status,
    pub make: unsafe extern "C" fn(*mut c_void, *const ValueView, *mut Handle) -> Status,
    pub array_get: unsafe extern "C" fn(*mut c_void, Handle, usize, *mut Handle) -> Status,
    pub array_set: unsafe extern "C" fn(*mut c_void, Handle, usize, Handle) -> Status,
    pub write_output: unsafe extern "C" fn(*mut c_void, Bytes) -> Status,
    pub set_error: unsafe extern "C" fn(*mut c_void, Bytes) -> Status,
}

pub type Callback = unsafe extern "C" fn(
    *mut c_void,
    *const HostV1,
    *mut c_void,
    *const Handle,
    usize,
    *mut Handle,
) -> Status;

#[repr(C)]
pub struct FunctionV1 {
    pub size: u32,
    pub name: Bytes,
    pub parameters: Bytes,
    pub result: u8,
    pub effects: u16,
    pub mutations: u64,
    pub invoke: Option<Callback>,
}
impl FunctionV1 {
    pub const fn new(
        name: &'static str,
        parameters: &'static [Type],
        result: Type,
        effects: super::Effects,
        mutations: u64,
        invoke: Callback,
    ) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            name: Bytes::new(name.as_bytes()),
            parameters: Bytes {
                data: parameters.as_ptr().cast(),
                len: parameters.len(),
            },
            result: result as u8,
            effects: effects.bits(),
            mutations,
            invoke: Some(invoke),
        }
    }
}

#[repr(C)]
pub struct PluginV1 {
    pub size: u32,
    pub version: u32,
    pub function_count: usize,
    pub functions: *const FunctionV1,
    pub create: Option<unsafe extern "C" fn(*mut *mut c_void) -> Status>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}
// Plugin descriptors remain immutable and valid until the library unloads.
unsafe impl Sync for PluginV1 {}
impl PluginV1 {
    pub const fn new(functions: &'static [FunctionV1]) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            version: VERSION,
            function_count: functions.len(),
            functions: functions.as_ptr(),
            create: None,
            destroy: None,
        }
    }
}

/// A borrowed call into the host. Handles and inspected views expire when the
/// callback returns. This wrapper never permits VM reentry or retaining a view.
pub struct PluginCall<'a> {
    host: &'a HostV1,
    context: *mut c_void,
    arguments: &'a [Handle],
}
impl PluginCall<'_> {
    pub fn argument(&self, index: usize) -> Result<Handle, Error> {
        self.arguments
            .get(index)
            .copied()
            .ok_or_else(|| Error::new("missing native argument"))
    }
    fn status(status: Status) -> Result<(), Error> {
        if status == OK {
            Ok(())
        } else {
            Err(Error::new(format!("host operation failed ({status})")))
        }
    }
    pub fn inspect(&self, value: Handle) -> Result<ValueView, Error> {
        let mut result = ValueView::default();
        Self::status(unsafe { (self.host.inspect)(self.context, value, &mut result) })?;
        Ok(result)
    }
    pub fn int(&self, value: Handle) -> Result<i64, Error> {
        let view = self.inspect(value)?;
        if view.kind == Type::Int as u32 {
            Ok(view.integer)
        } else {
            Err(Error::new("expected int"))
        }
    }
    pub fn string(&self, value: Handle) -> Result<&str, Error> {
        let view = self.inspect(value)?;
        if view.kind != Type::Str as u32 {
            return Err(Error::new("expected str"));
        }
        // Host roots inspected strings throughout this call; this reference
        // cannot outlive PluginCall. The ABI contract guarantees valid UTF-8.
        Ok(unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                view.bytes.data,
                view.bytes.len,
            ))
        })
    }
    fn make(&self, value: ValueView) -> Result<Handle, Error> {
        let mut result = 0;
        Self::status(unsafe { (self.host.make)(self.context, &value, &mut result) })?;
        Ok(result)
    }
    pub fn make_int(&self, value: i64) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Int as u32,
            integer: value,
            ..ValueView::default()
        })
    }
    pub fn make_nil(&self) -> Result<Handle, Error> {
        self.make(ValueView::default())
    }
    pub fn make_string(&self, value: &str) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Str as u32,
            bytes: Bytes::new(value.as_bytes()),
            ..ValueView::default()
        })
    }
    pub fn make_array(&self, values: &[Handle]) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Array as u32,
            items: values.as_ptr(),
            len: values.len(),
            ..ValueView::default()
        })
    }
    pub fn array_get(&self, array: Handle, index: usize) -> Result<Handle, Error> {
        let mut result = 0;
        Self::status(unsafe { (self.host.array_get)(self.context, array, index, &mut result) })?;
        Ok(result)
    }
    pub fn array_set(&self, array: Handle, index: usize, value: Handle) -> Result<(), Error> {
        Self::status(unsafe { (self.host.array_set)(self.context, array, index, value) })
    }
    pub fn write_output(&self, value: &str) -> Result<(), Error> {
        Self::status(unsafe {
            (self.host.write_output)(self.context, Bytes::new(value.as_bytes()))
        })
    }
}

/// Invoke a safe Rust plugin callback and catch unwinding panics before the C
/// boundary. The host's normal panic hook may still run.
///
/// # Safety
/// The host table, context, argument array and output pointer must obey ABI v1
/// and remain valid for this synchronous call. This does not catch aborts/OOM.
pub unsafe fn dispatch(
    host: *const HostV1,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
    callback: impl FnOnce(&PluginCall<'_>) -> Result<Handle, Error>,
) -> Status {
    if host.is_null()
        || context.is_null()
        || output.is_null()
        || count > 64
        || (count != 0 && arguments.is_null())
    {
        return INVALID;
    }
    // Read only the common header until the complete table has been checked.
    let size = unsafe { std::ptr::read(host.cast::<u32>()) };
    if size != size_of::<HostV1>() as u32 {
        return INVALID;
    }
    let host = unsafe { &*host };
    if host.version != VERSION {
        return INVALID;
    }
    let arguments = if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(arguments, count) }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(&PluginCall {
            host,
            context,
            arguments,
        })
    }));
    match result {
        Ok(Ok(value)) => {
            unsafe {
                output.write(value);
            }
            OK
        }
        Ok(Err(error)) => {
            unsafe {
                (host.set_error)(context, Bytes::new(error.message().as_bytes()));
            }
            FAILURE
        }
        Err(_) => {
            unsafe {
                (host.set_error)(context, Bytes::new(b"Rust plugin callback panicked"));
            }
            PANIC
        }
    }
}

/// Create plugin-owned Rust state without transferring its allocator to the host.
/// Unwinding panics become PANIC; allocation aborts cannot be contained.
///
/// # Safety
/// Output must be writable. Pair success with destroy_state of the same T in the
/// same library; no host code may dereference or free this pointer.
pub unsafe fn create_state<T: Default>(output: *mut *mut c_void) -> Status {
    if output.is_null() {
        return INVALID;
    }
    unsafe {
        output.write(std::ptr::null_mut());
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Box::new(T::default()))) {
        Ok(state) => {
            unsafe {
                output.write(Box::into_raw(state).cast());
            }
            OK
        }
        Err(_) => PANIC,
    }
}

/// Destroy plugin state while containing unwinding Drop panics.
///
/// # Safety
/// State must be null or a live pointer returned by create_state::<T> in this
/// library. It must be destroyed once, after all calls have finished.
pub unsafe fn destroy_state<T>(state: *mut c_void) {
    if !state.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(state.cast::<T>()));
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_state_wrappers_contain_initialization_and_destruction_panics() {
        struct InitPanic;
        impl Default for InitPanic {
            fn default() -> Self {
                panic!("initialization test");
            }
        }
        #[derive(Default)]
        struct DropPanic;
        impl Drop for DropPanic {
            fn drop(&mut self) {
                panic!("destruction test");
            }
        }
        let mut state = std::ptr::null_mut();
        assert_eq!(unsafe { create_state::<InitPanic>(&mut state) }, PANIC);
        assert!(state.is_null());
        assert_eq!(unsafe { create_state::<DropPanic>(&mut state) }, OK);
        unsafe {
            destroy_state::<DropPanic>(state);
        }
        assert_eq!(
            unsafe { create_state::<DropPanic>(std::ptr::null_mut()) },
            INVALID
        );
    }
}
