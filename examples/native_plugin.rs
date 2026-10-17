//! Build as a separate cdylib with the command in docs/SDK.md.
use cog::sdk::abi::{self, FunctionV1, Handle, HostV1, PluginV1};
use cog::sdk::{Effects, Type};
use std::ffi::c_void;

unsafe extern "C" fn double(
    _state: *mut c_void,
    host: *const HostV1,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
) -> abi::Status {
    unsafe {
        abi::dispatch(host, context, arguments, count, output, |call| {
            let value = call
                .int(call.argument(0)?)?
                .checked_mul(2)
                .ok_or_else(|| cog::sdk::Error::new("integer overflow"))?;
            call.make_int(value)
        })
    }
}

unsafe extern "C" fn fail(
    _state: *mut c_void,
    host: *const HostV1,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
) -> abi::Status {
    unsafe {
        abi::dispatch(host, context, arguments, count, output, |_| {
            panic!("example plugin panic");
        })
    }
}

static FUNCTIONS: [FunctionV1; 2] = [
    FunctionV1::new(
        "host::double",
        &[Type::Int],
        Type::Int,
        Effects::PURE,
        0,
        double,
    ),
    FunctionV1::new("host::fail", &[], Type::Nil, Effects::PURE, 0, fail),
];
static PLUGIN: PluginV1 = PluginV1::new(&FUNCTIONS);

#[unsafe(no_mangle)]
pub extern "C" fn cog_plugin_v1() -> *const PluginV1 {
    &PLUGIN
}

// Also allow cargo's ordinary all-target checks to compile this source.
fn main() {}
