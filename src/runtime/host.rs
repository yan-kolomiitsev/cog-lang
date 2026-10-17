//! Host effects share the linked native ABI; none are VM instructions.
use super::{RuntimeContext, bytes_new, invalid, io_error, string};
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{ArrayValue, RuntimeType, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static MONOTONIC_EPOCH: OnceLock<Instant> = OnceLock::new();

pub(super) fn sleep(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if !context.capabilities.allow_sleep {
        return Err(RuntimeError::PermissionDenied { op: "time::sleep" });
    }
    let duration = match args[0] {
        Value::Int(seconds) if seconds >= 0 => Duration::from_secs(seconds as u64),
        Value::Int(seconds) => return Err(RuntimeError::NegativeSleep(seconds as f64)),
        Value::Float(seconds) if seconds < 0.0 => return Err(RuntimeError::NegativeSleep(seconds)),
        Value::Float(seconds) => {
            Duration::try_from_secs_f64(seconds).map_err(|_| invalid("time::sleep"))?
        }
        _ => unreachable!(),
    };
    // Reject durations that cannot be represented by the host clock, before
    // handing them to an OS wait implementation. Sub-second precision is kept.
    std::time::Instant::now()
        .checked_add(duration)
        .ok_or_else(|| invalid("time::sleep"))?;
    std::thread::sleep(duration);
    Ok(Value::Nil)
}

fn epoch(context: &RuntimeContext<'_>, op: &'static str) -> Result<Duration, RuntimeError> {
    if !context.capabilities.allow_time {
        return Err(RuntimeError::PermissionDenied { op });
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid(op))
}
pub(super) fn now(context: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    i64::try_from(epoch(context, "time::now")?.as_secs())
        .map(Value::Int)
        .map_err(|_| RuntimeError::Overflow)
}
pub(super) fn now_nanos(
    context: &mut RuntimeContext<'_>,
    _: &[Value],
) -> Result<Value, RuntimeError> {
    i64::try_from(epoch(context, "time::now_nanos")?.as_nanos())
        .map(Value::Int)
        .map_err(|_| RuntimeError::Overflow)
}
pub(super) fn now_millis(
    context: &mut RuntimeContext<'_>,
    _: &[Value],
) -> Result<Value, RuntimeError> {
    i64::try_from(epoch(context, "time::now_millis")?.as_millis())
        .map(Value::Int)
        .map_err(|_| RuntimeError::Overflow)
}
pub(super) fn monotonic_nanos(
    context: &mut RuntimeContext<'_>,
    _: &[Value],
) -> Result<Value, RuntimeError> {
    if !context.capabilities.allow_time {
        return Err(RuntimeError::PermissionDenied {
            op: "time::monotonic_nanos",
        });
    }
    let elapsed = MONOTONIC_EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos();
    i64::try_from(elapsed)
        .map(Value::Int)
        .map_err(|_| RuntimeError::Overflow)
}
pub(super) fn entropy(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if !context.capabilities.allow_random {
        return Err(RuntimeError::PermissionDenied {
            op: "entropy::bytes",
        });
    }
    let value = bytes_new(context, args)?;
    if let Value::Bytes(bytes) = &value {
        let mut bytes = bytes
            .try_borrow_mut()
            .map_err(|_| RuntimeError::InvalidArgument {
                op: "entropy::bytes",
            })?;
        getrandom::fill(&mut bytes).map_err(|_| RuntimeError::EntropyUnavailable)?;
    }
    Ok(value)
}

pub(super) fn run(context: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    if !context.capabilities.allow_exec {
        return Err(RuntimeError::PermissionDenied { op: "process::run" });
    }
    let Value::Array(arguments) = &args[1] else {
        unreachable!()
    };
    let arguments = arguments
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op: "process::run" })?;
    // This function has a dynamic array parameter, so validate its complete view
    // before spawning. The array object still keeps its own element invariant.
    for value in arguments.iter() {
        match value {
            Value::Str(_) => {}
            Value::Error(_) => return Ok(value.clone()),
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    op: "process::run arguments",
                });
            }
        }
    }
    let program = string(&args[0]);
    let mut command = Command::new(program);
    command.args(arguments.iter().map(string));
    capture(context, command, program, "process::run")
}

pub(super) fn shell(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if !context.capabilities.allow_exec {
        return Err(RuntimeError::PermissionDenied {
            op: "process::shell",
        });
    }
    #[cfg(unix)]
    {
        let script = string(&args[0]);
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        capture(context, command, script, "process::shell")
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err(io_error(
            "process::shell",
            io::Error::new(io::ErrorKind::Unsupported, "POSIX shell is unavailable"),
        ))
    }
}

pub(super) fn arguments(
    context: &mut RuntimeContext<'_>,
    _: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "process::args";
    if !context.capabilities.allow_process_info {
        return Err(RuntimeError::PermissionDenied { op: OP });
    }
    let mut values = Vec::new();
    let mut used = 0usize;
    let mut add = |argument: &std::ffi::OsStr| -> Result<(), RuntimeError> {
        let text = argument.to_str().ok_or(RuntimeError::ParseFailure {
            from: "OS argument",
            to: "str",
        })?;
        used = used
            .checked_add(text.len())
            .and_then(|bytes| {
                bytes.checked_add(std::mem::size_of::<Value>() + 2 * std::mem::size_of::<usize>())
            })
            .filter(|bytes| *bytes <= context.limits.buffer_limit())
            .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })?;
        values
            .try_reserve(1)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;
        values.push(Value::Str(std::rc::Rc::from(text)));
        Ok(())
    };
    if let Some(arguments) = context.process_arguments() {
        for argument in arguments {
            add(argument)?;
        }
    } else {
        for argument in std::env::args_os().skip(1) {
            add(&argument)?;
        }
    }
    let array =
        ArrayValue::typed(values, RuntimeType::Str).map_err(|_| RuntimeError::InvalidNativeCall)?;
    Ok(Value::Array(HeapRef::try_new(array)?))
}

pub(super) fn environment(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    const OP: &str = "process::env";
    if !context.capabilities.allow_process_info {
        return Err(RuntimeError::PermissionDenied { op: OP });
    }
    let key = super::string(&args[0]);
    if key.is_empty() || key.contains(['=', '\0']) {
        return Err(super::invalid(OP));
    }
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| key.len() > limit)
    {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    let Some(value) = std::env::var_os(key) else {
        return Ok(Value::Nil);
    };
    let value = value
        .into_string()
        .map_err(|_| RuntimeError::ParseFailure {
            from: "OS environment",
            to: "str",
        })?;
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| value.len() > limit)
    {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    Ok(Value::Str(value.into()))
}

// The runtime owns the direct child until it has been reaped, including when
// spawning a reader, capturing output or waiting fails. No child handle escapes.
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

enum CaptureError {
    Limit,
    Io(io::Error),
}
fn read_capped(mut input: impl Read, limit: usize) -> Result<Vec<u8>, CaptureError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = match input.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CaptureError::Io(error)),
        };
        if count > limit.saturating_sub(bytes.len()) {
            return Err(CaptureError::Limit);
        }
        bytes.try_reserve(count).map_err(|_| CaptureError::Limit)?;
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn capture(
    context: &RuntimeContext<'_>,
    mut command: Command,
    name: &str,
    op: &'static str,
) -> Result<Value, RuntimeError> {
    let mut child = OwnedChild(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| RuntimeError::ProcessFailed {
                command: name.into(),
                code: None,
            })?,
    );
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let limit = context.limits.buffer_limit();
    let outputs = std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let first_sender = sender.clone();
        let first = std::thread::Builder::new().spawn_scoped(scope, move || {
            let _ = first_sender.send((0, read_capped(stdout, limit)));
        });
        if let Err(error) = first {
            let _ = child.0.kill();
            return Err(io_error(op, error));
        }
        let second = std::thread::Builder::new().spawn_scoped(scope, move || {
            let _ = sender.send((1, read_capped(stderr, limit)));
        });
        if let Err(error) = second {
            let _ = child.0.kill();
            return Err(io_error(op, error));
        }
        let mut outputs = [Vec::new(), Vec::new()];
        let mut failure = None;
        for _ in 0..2 {
            match receiver.recv() {
                Ok((index, Ok(bytes))) => outputs[index] = bytes,
                result => {
                    let _ = child.0.kill();
                    let _ = child.0.wait();
                    failure.get_or_insert(match result {
                        Ok((_, Err(CaptureError::Limit))) => RuntimeError::ResourceLimit {
                            resource: "process output",
                        },
                        Ok((_, Err(CaptureError::Io(error)))) => io_error(op, error),
                        _ => io_error(op, io::Error::other("process capture reader disconnected")),
                    });
                }
            }
        }
        failure.map_or(Ok(outputs), Err)
    })?;
    let status = child.0.wait().map_err(|error| io_error(op, error))?;
    let [stdout, stderr] = outputs;
    let fields = HashMap::from([
        (
            "exit_code".into(),
            status
                .code()
                .map_or(Value::Nil, |code| Value::Int(code as i64)),
        ),
        ("success".into(), Value::Bool(status.success())),
        (
            "stdout".into(),
            Value::Bytes(HeapRef::try_new(RefCell::new(stdout))?),
        ),
        (
            "stderr".into(),
            Value::Bytes(HeapRef::try_new(RefCell::new(stderr))?),
        ),
    ]);
    Ok(Value::Record(HeapRef::try_new(
        crate::value::RecordValue::new(fields),
    )?))
}
