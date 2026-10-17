//! Lexical path operations. These functions do not access the filesystem.
use super::{RuntimeContext, invalid, string};
use crate::error::RuntimeError;
use crate::value::Value;
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn path_string(context: &RuntimeContext<'_>, value: String) -> Result<Value, RuntimeError> {
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| value.len() > limit)
    {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    Ok(Value::Str(Rc::from(value)))
}

fn optional_path_string(
    context: &RuntimeContext<'_>,
    value: Option<&str>,
) -> Result<Value, RuntimeError> {
    match value {
        Some(value)
            if context
                .limits
                .max_buffer_bytes
                .is_some_and(|limit| value.len() > limit) =>
        {
            Err(RuntimeError::ResourceLimit { resource: "bytes" })
        }
        Some(value) => Ok(Value::Str(Rc::from(value))),
        None => Ok(Value::Nil),
    }
}

fn into_cog_string(
    context: &RuntimeContext<'_>,
    op: &'static str,
    path: PathBuf,
) -> Result<Value, RuntimeError> {
    let value = path
        .into_os_string()
        .into_string()
        .map_err(|_| invalid(op))?;
    path_string(context, value)
}

pub(super) fn join(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    into_cog_string(
        context,
        "path::join",
        Path::new(string(&args[0])).join(string(&args[1])),
    )
}

pub(super) fn is_absolute(
    _: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    Ok(Value::Bool(Path::new(string(&args[0])).is_absolute()))
}

pub(super) fn parent(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    optional_path_string(
        context,
        Path::new(string(&args[0])).parent().and_then(Path::to_str),
    )
}

pub(super) fn file_name(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    optional_path_string(
        context,
        Path::new(string(&args[0]))
            .file_name()
            .and_then(std::ffi::OsStr::to_str),
    )
}

pub(super) fn extension(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    optional_path_string(
        context,
        Path::new(string(&args[0]))
            .extension()
            .and_then(std::ffi::OsStr::to_str),
    )
}

pub(super) fn with_extension(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let extension = string(&args[1]);
    if extension.chars().any(std::path::is_separator) {
        return Err(invalid("path::with_extension"));
    }
    let path = Path::new(string(&args[0])).with_extension(extension);
    into_cog_string(context, "path::with_extension", path)
}
