//! Checked conversions and bounded formatting, independent of VM instructions.
use super::RuntimeContext;
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::Value;
use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

pub(super) fn to_int(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Int(_) => args[0].clone(),
        Value::Float(x) if x.is_finite() && *x >= i64::MIN as f64 && *x < -(i64::MIN as f64) => {
            Value::Int(*x as i64)
        }
        Value::Float(_) => return Err(RuntimeError::Overflow),
        Value::Str(text) => Value::Int(text.parse().map_err(|_| RuntimeError::ParseFailure {
            from: "str",
            to: "int",
        })?),
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_int" }),
    })
}

pub(super) fn to_float(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Float(_) => args[0].clone(),
        Value::Int(x) => Value::Float(*x as f64),
        Value::Str(text) => Value::Float(text.parse().map_err(|_| RuntimeError::ParseFailure {
            from: "str",
            to: "float",
        })?),
        _ => {
            return Err(RuntimeError::TypeMismatch {
                op: "str::to_float",
            });
        }
    })
}

pub(super) fn to_bool(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Bool(_) => args[0].clone(),
        Value::Str(text) => match text.as_ref() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => {
                return Err(RuntimeError::ParseFailure {
                    from: "str",
                    to: "bool",
                });
            }
        },
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_bool" }),
    })
}

fn limit(resource: &'static str) -> RuntimeError {
    RuntimeError::ResourceLimit { resource }
}

pub(super) fn to_str(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if let Value::Error(_) = &args[0] {
        return Ok(args[0].clone());
    }
    if let Value::Str(text) = &args[0] {
        if context
            .limits
            .max_buffer_bytes
            .is_some_and(|limit| text.len() > limit)
        {
            return Err(limit("str"));
        }
        return Ok(args[0].clone());
    }
    Ok(Value::Str(
        render(&args[0], context.limits.buffer_limit(), "str")?.into(),
    ))
}

pub(super) fn builder(_: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::StrBuf(HeapRef::try_new(
        RefCell::new(String::new()),
    )?))
}

pub(super) fn build(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::StrBuf(buffer) = &args[0] else {
        unreachable!("checked native signature")
    };
    let text = buffer
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op: "str::build" })?;
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| text.len() > limit)
    {
        return Err(limit("str"));
    }
    // Snapshot: later writes through any builder alias cannot change this str.
    Ok(Value::Str(Rc::from(text.as_str())))
}

pub(super) fn write(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::StrBuf(buffer) = &args[0] else {
        unreachable!("checked native signature")
    };
    let Value::Str(template) = &args[1] else {
        unreachable!("checked native signature")
    };
    if let Value::Error(_) = &args[2] {
        return Ok(args[2].clone());
    }
    let length = buffer
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op: "fmt::write" })?
        .len();
    let remaining = context
        .limits
        .buffer_limit()
        .checked_sub(length)
        .ok_or_else(|| limit("str"))?;
    let (prefix, suffix, rendered) = if let Some(position) = template.find("{}") {
        let literal_length = template.len() - 2;
        let budget = remaining
            .checked_sub(literal_length)
            .ok_or_else(|| limit("str"))?;
        // Render before mutably borrowing the target: it may be the value itself
        // or appear inside a nested array/record. Failure leaves every alias intact.
        (
            &template[..position],
            &template[position + 2..],
            render(&args[2], budget, "str")?,
        )
    } else {
        if template.len() > remaining {
            return Err(limit("str"));
        }
        (template.as_ref(), "", String::new())
    };
    let mut text = buffer
        .try_borrow_mut()
        .map_err(|_| RuntimeError::InvalidArgument { op: "fmt::write" })?;
    text.try_reserve(prefix.len() + rendered.len() + suffix.len())
        .map_err(|_| limit("str"))?;
    text.push_str(prefix);
    text.push_str(&rendered);
    text.push_str(suffix);
    Ok(args[0].clone())
}

/// One bounded renderer serves string conversion, builders and byte rendering.
/// Traversal depth is bounded independently of output size, including cycles.
pub(crate) fn render(
    value: &Value,
    limit: usize,
    resource: &'static str,
) -> Result<String, RuntimeError> {
    let mut output = RenderBuffer {
        text: String::new(),
        limit,
        failure: None,
    };
    if output.value(value, 0).is_err() {
        return Err(output.failure.unwrap_or_else(|| self::limit(resource)));
    }
    Ok(output.text)
}

struct RenderBuffer {
    text: String,
    limit: usize,
    failure: Option<RuntimeError>,
}

impl Write for RenderBuffer {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if text.len() > self.limit.saturating_sub(self.text.len()) {
            return Err(std::fmt::Error);
        }
        self.text
            .try_reserve(text.len())
            .map_err(|_| std::fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

impl RenderBuffer {
    fn borrow_error(&mut self) -> std::fmt::Error {
        self.failure = Some(RuntimeError::InvalidArgument { op: "render" });
        std::fmt::Error
    }

    fn fields(
        &mut self,
        fields: &std::collections::HashMap<String, Value>,
        depth: usize,
    ) -> std::fmt::Result {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(fields.len())
            .map_err(|_| std::fmt::Error)?;
        entries.extend(fields);
        entries.sort_unstable_by_key(|(name, _)| *name);
        for (index, (name, value)) in entries.into_iter().enumerate() {
            if index > 0 {
                self.write_str(", ")?;
            }
            write!(self, "{name}: ")?;
            self.value(value, depth + 1)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> std::fmt::Result {
        if depth >= 64 {
            self.failure = Some(limit("render depth"));
            return Err(std::fmt::Error);
        }
        match value {
            Value::Array(values) => {
                let values = values.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.write_str(", ")?;
                    }
                    self.value(value, depth + 1)?;
                }
                self.write_str("]")
            }
            Value::Record(fields) => {
                let fields = fields.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str("{")?;
                self.fields(&fields, depth)?;
                self.write_str("}")
            }
            Value::Variant(tag, fields) => {
                write!(self, "{tag}(")?;
                let fields = fields.try_borrow().map_err(|_| self.borrow_error())?;
                self.fields(&fields, depth)?;
                self.write_str(")")
            }
            Value::Cell(cell) => {
                let value = cell.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str("cell(")?;
                self.value(&value, depth + 1)?;
                self.write_str(")")
            }
            Value::StrBuf(buffer) => {
                let text = buffer.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str(&text)
            }
            Value::Bytes(buffer) => {
                let bytes = buffer.try_borrow().map_err(|_| self.borrow_error())?;
                write!(self, "<bytes:{}>", bytes.len())
            }
            Value::Error(error) => {
                self.write_str("Error: ")?;
                match error.primary() {
                    RuntimeError::LanguageError { tag, fields } => {
                        if let Some(tail) = tag.strip_prefix(crate::error::ERROR_TYPE) {
                            write!(self, "Error{tail}(")?;
                        } else {
                            write!(self, "{tag}(")?;
                        }
                        let fields = fields.try_borrow().map_err(|_| self.borrow_error())?;
                        self.fields(&fields, depth)?;
                        self.write_str(")")
                    }
                    RuntimeError::Crashed { context, value } => {
                        self.write_str("crash: ")?;
                        if let Some(context) = context {
                            write!(self, "{context}: ")?;
                        }
                        self.value(value, depth + 1)
                    }
                    _ => write!(self, "{error}"),
                }
            }
            // These variants cannot recursively visit a user-controlled graph.
            _ => write!(self, "{value}"),
        }
    }
}
