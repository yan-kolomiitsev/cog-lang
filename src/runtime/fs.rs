//! Runtime filesystem operations use process paths and OS permissions.
use super::{RuntimeContext, io_error, string};
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{ArrayValue, RecordValue, RuntimeType, Value};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

fn require_read(context: &RuntimeContext<'_>, op: &'static str) -> Result<(), RuntimeError> {
    if context.capabilities.allow_fs_read {
        Ok(())
    } else {
        Err(RuntimeError::PermissionDenied { op })
    }
}

fn require_write(context: &RuntimeContext<'_>, op: &'static str) -> Result<(), RuntimeError> {
    if context.capabilities.allow_fs_write {
        Ok(())
    } else {
        Err(RuntimeError::PermissionDenied { op })
    }
}

pub(super) fn metadata(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "fs::metadata";
    require_read(context, OP)?;
    let metadata = std::fs::metadata(string(&args[0])).map_err(|error| io_error(OP, error))?;
    let length = i64::try_from(metadata.len()).map_err(|_| RuntimeError::Overflow)?;
    let fields = HashMap::from([
        ("is_file".to_owned(), Value::Bool(metadata.is_file())),
        ("is_dir".to_owned(), Value::Bool(metadata.is_dir())),
        (
            "is_readonly".to_owned(),
            Value::Bool(metadata.permissions().readonly()),
        ),
        ("len".to_owned(), Value::Int(length)),
    ]);
    Ok(Value::Record(HeapRef::try_new(RecordValue::new(fields))?))
}

pub(super) fn read_dir(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "fs::read_dir";
    require_read(context, OP)?;
    let entries =
        std::fs::read_dir(Path::new(string(&args[0]))).map_err(|error| io_error(OP, error))?;
    let mut values = Vec::new();
    let mut bytes = 0usize;
    for entry in entries {
        let name = entry
            .map_err(|error| io_error(OP, error))?
            .file_name()
            .into_string()
            .map_err(|_| RuntimeError::ParseFailure {
                from: "path",
                to: "str",
            })?;
        let entry_cost = name
            .len()
            .checked_add(std::mem::size_of::<Value>())
            .and_then(|size| size.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })?;
        bytes = bytes
            .checked_add(entry_cost)
            .filter(|total| *total <= context.limits.buffer_limit())
            .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })?;
        values
            .try_reserve(1)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
        values.push(Value::Str(Rc::from(name)));
    }
    values.sort_unstable_by(|left, right| match (left, right) {
        (Value::Str(left), Value::Str(right)) => left.as_ref().cmp(right.as_ref()),
        _ => std::cmp::Ordering::Equal,
    });
    let array =
        ArrayValue::typed(values, RuntimeType::Str).map_err(|_| RuntimeError::InvalidNativeCall)?;
    Ok(Value::Array(HeapRef::try_new(array)?))
}

fn create_directory(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
    recursive: bool,
) -> Result<Value, RuntimeError> {
    const OP: &str = "fs::create_dir";
    const RECURSIVE_OP: &str = "fs::create_dir_all";
    let op = if recursive { RECURSIVE_OP } else { OP };
    require_write(context, op)?;
    let result = if recursive {
        std::fs::create_dir_all(string(&args[0]))
    } else {
        std::fs::create_dir(string(&args[0]))
    };
    result.map_err(|error| io_error(op, error))?;
    Ok(Value::Nil)
}

pub(super) fn create_dir(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    create_directory(context, args, false)
}

pub(super) fn create_dir_all(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    create_directory(context, args, true)
}

pub(super) fn remove_dir(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "fs::remove_dir";
    require_write(context, OP)?;
    std::fs::remove_dir(string(&args[0])).map_err(|error| io_error(OP, error))?;
    Ok(Value::Nil)
}

pub(super) fn remove_file(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "fs::remove_file";
    require_write(context, OP)?;
    std::fs::remove_file(string(&args[0])).map_err(|error| io_error(OP, error))?;
    Ok(Value::Nil)
}
