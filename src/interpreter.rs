use crate::error::RuntimeError;
use crate::heap;
use crate::heap::HeapRef;
use crate::instruction::Instruction;
use crate::template::ArrayTemplate;
use crate::value::{ArrayValue, ClosureCaptures, RecordValue, RuntimeType, Value, ValueFields};
use std::cell::RefCell;
use std::rc::Rc;

use crate::backend::CompiledProgram;
use crate::runtime::RuntimeContext;
use crate::runtime::VmCapabilities;

#[derive(Clone, Default)]
struct CaptureValues(Option<HeapRef<ClosureCaptures>>);

impl CaptureValues {
    fn get(&self, index: usize) -> Option<Value> {
        self.0.as_ref()?.get(index)
    }

    fn into_root(self) -> Option<Value> {
        self.0.map(|captures| Value::Closure(0, captures))
    }
}

type CallFrame = (usize, usize, CaptureValues, usize);

fn reserve_cleanup_roots(stack: &mut Vec<Value>, counts: [usize; 4]) -> Result<(), RuntimeError> {
    let failure = || RuntimeError::ResourceLimit {
        resource: "VM cleanup roots",
    };
    let additional = counts
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(failure)?;
    stack.try_reserve_exact(additional).map_err(|_| failure())
}

fn reserve_frame(frames: &mut Vec<CallFrame>, limit: Option<usize>) -> Result<(), RuntimeError> {
    if limit.is_some_and(|limit| frames.len() >= limit) {
        return Err(RuntimeError::ResourceLimit {
            resource: "call stack",
        });
    }
    frames
        .try_reserve(1)
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "call stack",
        })
}

fn try_record_key_with_capacity(key: &str, capacity: usize) -> Result<String, RuntimeError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(capacity.max(key.len()))
        .map_err(|_| RuntimeError::ResourceLimit { resource: "record" })?;
    owned.push_str(key);
    Ok(owned)
}

fn try_record_key(key: &str) -> Result<String, RuntimeError> {
    try_record_key_with_capacity(key, key.len())
}

fn missing_field(name: &str) -> Result<Value, RuntimeError> {
    Err(RuntimeError::MissingField {
        field: try_record_key(name)?,
    })
}

#[inline]
fn check_jump(program: &[Instruction], target: usize) -> Result<usize, RuntimeError> {
    if target < program.len() {
        Ok(target)
    } else {
        Err(RuntimeError::InvalidJump(target))
    }
}

fn check_call(
    artifact: &CompiledProgram,
    target: usize,
    parameters: usize,
    captures: usize,
) -> Result<usize, RuntimeError> {
    let signature = artifact
        .callables
        .get(&target)
        .ok_or(RuntimeError::InvalidCall(target))?;
    if target == 0 || signature.parameters != parameters || signature.captures != captures {
        return Err(RuntimeError::InvalidCall(target));
    }
    check_jump(&artifact.instructions, target)
}

fn matches_runtime_type(expected: &RuntimeType, value: &Value) -> bool {
    matches!(value, Value::Error(_)) || expected.accepts(&RuntimeType::of(value))
}

fn call_arguments_match(artifact: &CompiledProgram, target: usize, arguments: &[Value]) -> bool {
    let Some(signature) = artifact.callables.get(&target) else {
        return false;
    };
    let Some(parameter_types) = &signature.parameter_types else {
        return true;
    };
    parameter_types.len() == arguments.len()
        && parameter_types
            .iter()
            .zip(arguments)
            .all(|(expected, value)| {
                expected
                    .as_ref()
                    .is_none_or(|expected| matches_runtime_type(expected, value))
            })
}

fn check_branch(
    artifact: &CompiledProgram,
    pc: usize,
    target: usize,
) -> Result<usize, RuntimeError> {
    check_jump(&artifact.instructions, target)?;
    let region = |address| {
        artifact
            .callables
            .range(..=address)
            .next_back()
            .map(|(entry, _)| *entry)
    };
    if region(pc) != region(target) {
        return Err(RuntimeError::InvalidJump(target));
    }
    Ok(target)
}

#[inline]
fn numeric_binop(
    a: Value,
    b: Value,
    op_name: &'static str,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
) -> Result<Value, RuntimeError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            int_op(x, y).map(Value::Int).ok_or(RuntimeError::Overflow)
        }
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(float_op(x, y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(float_op(x, y as f64))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(float_op(x as f64, y))),
        _ => Err(RuntimeError::TypeMismatch { op: op_name }),
    }
}

#[inline]
fn compare_binop(
    a: Value,
    b: Value,
    op_name: &'static str,
    predicate: impl FnOnce(Option<std::cmp::Ordering>) -> bool,
) -> Result<Value, RuntimeError> {
    let order = match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(&y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(&y),
        (Value::Float(x), Value::Int(y)) => {
            crate::value::compare_int_float(y, x).map(std::cmp::Ordering::reverse)
        }
        (Value::Int(x), Value::Float(y)) => crate::value::compare_int_float(x, y),
        _ => return Err(RuntimeError::TypeMismatch { op: op_name }),
    };
    Ok(Value::Bool(predicate(order)))
}

#[inline]
fn pop2(stack: &mut Vec<Value>) -> Result<(Value, Value), RuntimeError> {
    let b = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
    let a = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
    Ok((a, b))
}

fn take_stack_tail(stack: &mut Vec<Value>, count: usize) -> Result<Vec<Value>, RuntimeError> {
    let begin = stack
        .len()
        .checked_sub(count)
        .ok_or(RuntimeError::StackUnderflow)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM aggregate",
        })?;
    values.extend(stack.drain(begin..));
    Ok(values)
}

#[inline]
fn pop3(stack: &mut Vec<Value>) -> Result<(Value, Value, Value), RuntimeError> {
    let c = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
    let b = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
    let a = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
    Ok((a, b, c))
}

fn local_address(base: usize, index: usize) -> Result<usize, RuntimeError> {
    // Match the portable slot contract even when an embedder bypasses decode.
    if index >= crate::bytecode::LIMIT {
        return Err(RuntimeError::InvalidLocal(index));
    }
    base.checked_add(index)
        .ok_or(RuntimeError::InvalidLocal(index))
}

#[inline]
fn borrow_conflict(op: &'static str) -> RuntimeError {
    RuntimeError::InvalidArgument { op }
}

fn store_slot(
    slots: &mut Vec<Option<Value>>,
    address: usize,
    value: Value,
    limit: Option<usize>,
) -> Result<(), RuntimeError> {
    if limit.is_some_and(|limit| address >= limit) {
        return Err(RuntimeError::ResourceLimit {
            resource: "VM slots",
        });
    }
    if address >= slots.len() {
        let length = address
            .checked_add(1)
            .ok_or(RuntimeError::InvalidLocal(address))?;
        slots
            .try_reserve(length - slots.len())
            .map_err(|_| RuntimeError::ResourceLimit {
                resource: "VM slots",
            })?;
        slots.resize(length, None);
    }
    slots[address] = Some(value);
    Ok(())
}

#[inline]
fn instantiate_array(template: &ArrayTemplate) -> Result<Value, RuntimeError> {
    Ok(match template {
        ArrayTemplate::Scalar(v) => v.clone(),
        ArrayTemplate::Array(items) => {
            let mut vec = Vec::new();
            for item in items {
                vec.try_reserve(item.repeat)
                    .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
                for _ in 0..item.repeat {
                    vec.push(instantiate_array(&item.template)?);
                }
            }
            match ArrayValue::new(vec) {
                Ok(array) => Value::Array(HeapRef::try_new(array)?),
                Err(_) => Value::Error(Box::new(RuntimeError::TypeMismatch { op: "array" })),
            }
        }
    })
}

/// Return the number of managed array objects and expanded template nodes that
/// one `PushArray` will instantiate. Bytecode validation already applies these
/// limits to decoded artifacts; this runtime check also protects callers that
/// construct `CompiledProgram` values directly through the Rust API.
fn array_template_cost(template: &ArrayTemplate) -> Result<(usize, usize), RuntimeError> {
    const MAX_TEMPLATE_NODES: usize = crate::bytecode::LIMIT;
    const MAX_TEMPLATE_DEPTH: usize = 64;

    let mut pending = Vec::new();
    pending
        .try_reserve(1)
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "array template",
        })?;
    pending.push((template, 1usize, 0usize));
    let (mut scheduled, mut expanded, mut objects) = (1usize, 0usize, 0usize);

    while let Some((template, multiplier, depth)) = pending.pop() {
        if depth >= MAX_TEMPLATE_DEPTH {
            return Err(RuntimeError::ResourceLimit {
                resource: "array template",
            });
        }
        expanded = expanded
            .checked_add(multiplier)
            .filter(|count| *count <= MAX_TEMPLATE_NODES)
            .ok_or(RuntimeError::ResourceLimit {
                resource: "array template",
            })?;

        let ArrayTemplate::Array(items) = template else {
            continue;
        };
        scheduled = scheduled
            .checked_add(items.len())
            .filter(|count| *count <= MAX_TEMPLATE_NODES)
            .ok_or(RuntimeError::ResourceLimit {
                resource: "array template",
            })?;
        objects = objects
            .checked_add(multiplier)
            .filter(|count| *count <= MAX_TEMPLATE_NODES)
            .ok_or(RuntimeError::ResourceLimit {
                resource: "array template",
            })?;

        for item in items {
            if item.repeat > MAX_TEMPLATE_NODES {
                return Err(RuntimeError::ResourceLimit {
                    resource: "array template",
                });
            }
            if item.repeat == 0 {
                continue;
            }
            let child_multiplier = multiplier
                .checked_mul(item.repeat)
                .filter(|count| *count <= MAX_TEMPLATE_NODES)
                .ok_or(RuntimeError::ResourceLimit {
                    resource: "array template",
                })?;
            pending
                .try_reserve(1)
                .map_err(|_| RuntimeError::ResourceLimit {
                    resource: "array template",
                })?;
            pending.push((&item.template, child_multiplier, depth + 1));
        }
    }

    Ok((objects, expanded))
}

fn check_array_template_resources(
    template: &ArrayTemplate,
    context: &RuntimeContext<'_>,
) -> Result<(), RuntimeError> {
    let (objects, _) = array_template_cost(template)?;
    let Some(limit) = context.limits.max_heap_objects else {
        return Ok(());
    };
    if objects > limit {
        return Err(RuntimeError::ResourceLimit {
            resource: "VM heap objects",
        });
    }

    if heap::live_objects().saturating_add(objects) > limit {
        heap::collect(&[])?;
    }
    if heap::live_objects().saturating_add(objects) > limit {
        return Err(RuntimeError::ResourceLimit {
            resource: "VM heap objects",
        });
    }
    Ok(())
}

#[inline]
fn binop(
    stack: &mut Vec<Value>,
    f: impl FnOnce(Value, Value) -> Result<Value, RuntimeError>,
) -> Result<(), RuntimeError> {
    let (a, b) = pop2(stack)?;

    if let Value::Error(_) = a {
        stack.push(a);
        return Ok(());
    }
    if let Value::Error(_) = b {
        stack.push(b);
        return Ok(());
    }

    match f(a, b) {
        Ok(result) => stack.push(result),
        Err(e) if e.is_recoverable() => stack.push(Value::Error(Box::new(e))),
        Err(e) => return Err(e),
    }
    Ok(())
}

pub fn run(program: &CompiledProgram) -> Result<(), RuntimeError> {
    run_with(program, &mut std::io::stdout())
}

pub fn run_with<W: std::io::Write>(
    program: &CompiledProgram,
    out: &mut W,
) -> Result<(), RuntimeError> {
    run_with_capabilities(program, out, VmCapabilities::default())
}

pub fn run_with_capabilities<W: std::io::Write>(
    program: &CompiledProgram,
    out: &mut W,
    caps: VmCapabilities,
) -> Result<(), RuntimeError> {
    let mut context = RuntimeContext::new(out, caps);
    run_with_context(program, &mut context)
}

/// Execute with an explicit set of process arguments available to
/// `process::args`, excluding the host executable name.
pub fn run_with_process_args<W: std::io::Write>(
    program: &CompiledProgram,
    out: &mut W,
    caps: VmCapabilities,
    arguments: Vec<std::ffi::OsString>,
) -> Result<(), RuntimeError> {
    let mut context = RuntimeContext::new(out, caps);
    context.set_process_arguments(arguments);
    run_with_context(program, &mut context)
}

/// Execute already-linked imports and finish all context resources on every exit.
/// A primary VM failure takes precedence over a secondary cleanup failure.
pub fn run_with_context(
    artifact: &CompiledProgram,
    context: &mut RuntimeContext<'_>,
) -> Result<(), RuntimeError> {
    let result = execute(artifact, context);
    let cleanup = context.finish();
    match (result, cleanup) {
        (Err(primary), Err(secondary)) if primary.is_recoverable() => {
            Err(primary.with_suppressed(secondary))
        }
        (result, cleanup) => result.and(cleanup),
    }
}

fn execute(
    artifact: &CompiledProgram,
    context: &mut RuntimeContext<'_>,
) -> Result<(), RuntimeError> {
    context.set_source_location(None);
    let program = &artifact.instructions;
    let mut stack: Vec<Value> = Vec::new();
    let mut locals: Vec<Option<Value>> = Vec::new();
    let mut globals: Vec<Option<Value>> = Vec::new();
    let mut pc: usize = 0;
    let mut bp: usize = 0;
    let mut call_stack: Vec<CallFrame> = Vec::new();
    let mut operand_base = 0usize;
    let mut captures = CaptureValues::default();
    let mut remaining_steps = context.limits.max_steps;
    let mut error_value_pc = None;
    let mut halt_error_pc = None;
    let result = (|| -> Result<(), RuntimeError> {
        while pc < program.len() {
            let heap_limit = context.limits.max_heap_objects;
            if heap::collection_due()
                || heap_limit.is_some_and(|limit| heap::live_objects() > limit)
            {
                heap::collect(&[])?;
            }
            if heap_limit.is_some_and(|limit| heap::live_objects() > limit) {
                return Err(RuntimeError::ResourceLimit {
                    resource: "VM heap objects",
                });
            }
            if let Some(steps) = &mut remaining_steps {
                *steps = steps.checked_sub(1).ok_or(RuntimeError::ResourceLimit {
                    resource: "instruction steps",
                })?;
            }
            let (required, produced) = program[pc].stack_effect(&artifact.imports)?;
            let available = stack
                .len()
                .checked_sub(operand_base)
                .ok_or(RuntimeError::StackUnderflow)?;
            if available < required {
                return Err(RuntimeError::StackUnderflow);
            }
            if context
                .limits
                .max_operand_values
                .is_some_and(|limit| stack.len() - required + produced > limit)
            {
                return Err(RuntimeError::ResourceLimit {
                    resource: "operand stack",
                });
            }
            stack
                .try_reserve(produced.saturating_sub(required))
                .map_err(|_| RuntimeError::ResourceLimit {
                    resource: "operand stack",
                })?;
            match &program[pc] {
                Instruction::Push(value) => {
                    stack.push(value.clone());
                }
                Instruction::PushArray(template) => {
                    check_array_template_resources(template, context)?;
                    stack.push(instantiate_array(template)?);
                }
                Instruction::Halt => {
                    if let Some(Value::Error(error)) = stack[operand_base..].last() {
                        halt_error_pc = Some(error_value_pc.unwrap_or(pc));
                        return Err((**error).clone());
                    }
                    return Ok(());
                }
                Instruction::Add => {
                    binop(&mut stack, |a, b| {
                        if let (Value::Str(x), Value::Str(y)) = (&a, &b) {
                            let length = x
                                .len()
                                .checked_add(y.len())
                                .filter(|length| *length <= context.limits.buffer_limit())
                                .ok_or(RuntimeError::ResourceLimit { resource: "str" })?;
                            let mut s = String::new();
                            s.try_reserve_exact(length)
                                .map_err(|_| RuntimeError::ResourceLimit { resource: "str" })?;
                            s.push_str(x);
                            s.push_str(y);
                            return Ok(Value::Str(Rc::from(s)));
                        }
                        if let Value::Array(arr_a) = &a {
                            let left = arr_a
                                .try_borrow()
                                .map_err(|_| borrow_conflict("array_concat"))?;
                            let mut new_vec = Vec::new();
                            new_vec
                                .try_reserve_exact(left.len())
                                .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
                            new_vec.extend(left.iter().cloned());
                            drop(left);
                            let mut expected = arr_a.element_type();
                            if let Value::Array(arr_b) = &b {
                                let right_type = arr_b.element_type();
                                if expected
                                    .as_ref()
                                    .zip(right_type.as_ref())
                                    .is_some_and(|(left, right)| left != right)
                                {
                                    return Err(RuntimeError::TypeMismatch { op: "array_concat" });
                                }
                                let right = arr_b
                                    .try_borrow()
                                    .map_err(|_| borrow_conflict("array_concat"))?;
                                new_vec.try_reserve(right.len()).map_err(|_| {
                                    RuntimeError::ResourceLimit { resource: "array" }
                                })?;
                                new_vec.extend(right.iter().cloned());
                                expected = expected.or(right_type);
                            } else {
                                new_vec.try_reserve(1).map_err(|_| {
                                    RuntimeError::ResourceLimit { resource: "array" }
                                })?;
                                new_vec.push(b.clone());
                            }
                            let result = if let Some(expected) = expected {
                                ArrayValue::typed(new_vec, expected)
                            } else {
                                ArrayValue::new(new_vec)
                            };
                            return match result {
                                Ok(array) => Ok(Value::Array(HeapRef::try_new(array)?)),
                                Err(_) => Err(RuntimeError::TypeMismatch { op: "array_concat" }),
                            };
                        }
                        if let Value::Array(arr_b) = &b {
                            let right = arr_b
                                .try_borrow()
                                .map_err(|_| borrow_conflict("array_concat"))?;
                            let capacity = right
                                .len()
                                .checked_add(1)
                                .ok_or(RuntimeError::ResourceLimit { resource: "array" })?;
                            let mut new_vec: Vec<Value> = Vec::new();
                            new_vec
                                .try_reserve_exact(capacity)
                                .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
                            new_vec.push(a.clone());
                            new_vec.extend(right.iter().cloned());
                            drop(right);
                            let result = if let Some(expected) = arr_b.element_type() {
                                ArrayValue::typed(new_vec, expected)
                            } else {
                                ArrayValue::new(new_vec)
                            };
                            return match result {
                                Ok(array) => Ok(Value::Array(HeapRef::try_new(array)?)),
                                Err(_) => Err(RuntimeError::TypeMismatch { op: "array_concat" }),
                            };
                        }
                        numeric_binop(a, b, "add", i64::checked_add, |x, y| x + y)
                    })?;
                }
                Instruction::ArrayPush => {
                    // `arr << x`: left operand must be the array, mutated in place so
                    // every alias of it (another variable, a captured closure, a field
                    // on some other value) observes the push — unlike the old
                    // "reuse Add" approach, which built a brand-new array and left the
                    // original untouched, silently doing nothing for a bare `arr << x;`.
                    binop(&mut stack, |a, b| match a {
                        Value::Array(arr) => {
                            arr.try_push(b, false)?;
                            Ok(Value::Array(arr))
                        }
                        _ => Err(RuntimeError::TypeMismatch { op: "arrpush" }),
                    })?;
                }
                Instruction::ArrayPrepend => {
                    // `x >> arr`: right operand must be the array (see hlparser's
                    // `parse_shift` / the `prepend_operator_parses` test — Prepend's
                    // AST is `Binary(Prepend, elem, arr)`), mutated in place for the
                    // same aliasing reason as ArrayPush above.
                    binop(&mut stack, |a, b| match b {
                        Value::Array(arr) => {
                            arr.try_push(a, true)?;
                            Ok(Value::Array(arr))
                        }
                        _ => Err(RuntimeError::TypeMismatch { op: "arrprepend" }),
                    })?;
                }
                Instruction::Sub => {
                    binop(&mut stack, |a, b| {
                        numeric_binop(a, b, "sub", i64::checked_sub, |x, y| x - y)
                    })?;
                }
                Instruction::Mul => {
                    binop(&mut stack, |a, b| {
                        numeric_binop(a, b, "mul", i64::checked_mul, |x, y| x * y)
                    })?;
                }
                Instruction::Div => {
                    binop(&mut stack, |a, b| match (a, b) {
                        (Value::Int(_), Value::Int(0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Int(x), Value::Int(y)) => x
                            .checked_div(y)
                            .map(Value::Int)
                            .ok_or(RuntimeError::Overflow),
                        (Value::Float(_), Value::Float(0.0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Float(_), Value::Int(0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Int(_), Value::Float(0.0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x / y)),
                        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x / (y as f64))),
                        (Value::Int(x), Value::Float(y)) => Ok(Value::Float((x as f64) / y)),
                        _ => Err(RuntimeError::TypeMismatch { op: "div" }),
                    })?;
                }
                Instruction::Mod => {
                    binop(&mut stack, |a, b| match (a, b) {
                        (Value::Int(_), Value::Int(0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Int(x), Value::Int(y)) => x
                            .checked_rem(y)
                            .map(Value::Int)
                            .ok_or(RuntimeError::Overflow),
                        (Value::Float(_), Value::Float(0.0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Float(_), Value::Int(0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Int(_), Value::Float(0.0)) => Err(RuntimeError::DivisionByZero),
                        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x % y)),
                        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x % (y as f64))),
                        (Value::Int(x), Value::Float(y)) => Ok(Value::Float((x as f64) % y)),
                        _ => Err(RuntimeError::TypeMismatch { op: "mod" }),
                    })?;
                }
                Instruction::Eq => {
                    binop(&mut stack, |a, b| match (&a, &b) {
                        (Value::Nil, Value::Nil) => Ok(Value::Bool(true)),
                        // Nullable values can be compared to nil independently
                        // of their non-nil normal type. Nullability does not
                        // imply an omitted call argument or a default value.
                        (Value::Nil, _) | (_, Value::Nil) => Ok(Value::Bool(false)),
                        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(x == y)),
                        (Value::Str(x), Value::Str(y)) | (Value::TypeTag(x), Value::TypeTag(y)) => {
                            Ok(Value::Bool(x == y))
                        }
                        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                            compare_binop(a, b, "eq", |order| {
                                order == Some(std::cmp::Ordering::Equal)
                            })
                        }
                        // Different normal types compare unequal. Equality of
                        // arrays, records and other unsupported same-kind values
                        // produces a recoverable TypeMismatch.
                        _ if std::mem::discriminant(&a) != std::mem::discriminant(&b) => {
                            Ok(Value::Bool(false))
                        }
                        _ => compare_binop(a, b, "eq", |order| {
                            order == Some(std::cmp::Ordering::Equal)
                        }),
                    })?;
                }
                Instruction::Lt => {
                    binop(&mut stack, |a, b| {
                        compare_binop(a, b, "lt", |order| {
                            order.is_some_and(std::cmp::Ordering::is_lt)
                        })
                    })?;
                }
                Instruction::Gt => {
                    binop(&mut stack, |a, b| {
                        compare_binop(a, b, "gt", |order| {
                            order.is_some_and(std::cmp::Ordering::is_gt)
                        })
                    })?;
                }
                Instruction::Ne => {
                    binop(&mut stack, |a, b| match (&a, &b) {
                        (Value::Nil, Value::Nil) => Ok(Value::Bool(false)),
                        (Value::Nil, _) | (_, Value::Nil) => Ok(Value::Bool(true)),
                        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(x != y)),
                        (Value::Str(x), Value::Str(y)) | (Value::TypeTag(x), Value::TypeTag(y)) => {
                            Ok(Value::Bool(x != y))
                        }
                        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                            compare_binop(a, b, "ne", |order| {
                                order != Some(std::cmp::Ordering::Equal)
                            })
                        }
                        // Mirror of `Eq`'s mismatched-type case above — see its
                        // comment.
                        _ if std::mem::discriminant(&a) != std::mem::discriminant(&b) => {
                            Ok(Value::Bool(true))
                        }
                        _ => compare_binop(a, b, "ne", |order| {
                            order != Some(std::cmp::Ordering::Equal)
                        }),
                    })?;
                }
                Instruction::Le => {
                    binop(&mut stack, |a, b| {
                        compare_binop(a, b, "le", |order| {
                            order.is_some_and(std::cmp::Ordering::is_le)
                        })
                    })?;
                }
                Instruction::Ge => {
                    binop(&mut stack, |a, b| {
                        compare_binop(a, b, "ge", |order| {
                            order.is_some_and(std::cmp::Ordering::is_ge)
                        })
                    })?;
                }
                Instruction::CallNative(index) => {
                    let import = artifact
                        .imports
                        .get(*index)
                        .ok_or(RuntimeError::InvalidNativeCall)?;
                    let count = import.parameters().len();
                    let base = stack
                        .len()
                        .checked_sub(count)
                        .filter(|base| *base >= operand_base)
                        .ok_or(RuntimeError::StackUnderflow)?;
                    let result = import.call(context, &stack[base..]);
                    stack.truncate(base);
                    match result {
                        Ok(value) => stack.push(value),
                        Err(error) if error.is_recoverable() => {
                            stack.push(Value::Error(Box::new(error)))
                        }
                        Err(error) => return Err(error),
                    }
                }
                Instruction::Store(index) => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    store_slot(
                        &mut locals,
                        local_address(bp, *index)?,
                        value,
                        context
                            .limits
                            .max_slots
                            .map(|limit| limit.saturating_sub(globals.len())),
                    )?;
                }
                Instruction::Load(index) => {
                    let abs = local_address(bp, *index)?;
                    let value = locals
                        .get(abs)
                        .cloned()
                        .flatten()
                        .ok_or(RuntimeError::InvalidLocal(*index))?;
                    stack.push(value);
                }
                Instruction::StoreGlobal(index) => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    store_slot(
                        &mut globals,
                        local_address(0, *index)?,
                        value,
                        context
                            .limits
                            .max_slots
                            .map(|limit| limit.saturating_sub(locals.len())),
                    )?;
                }
                Instruction::LoadGlobal(index) => {
                    local_address(0, *index)?;
                    let value = globals
                        .get(*index)
                        .cloned()
                        .flatten()
                        .ok_or(RuntimeError::InvalidLocal(*index))?;
                    stack.push(value);
                }
                Instruction::Jump(index) => {
                    pc = check_branch(artifact, pc, *index)?;
                    continue;
                }
                Instruction::JumpIfFalse(index) => {
                    let condition = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    match condition {
                        Value::Bool(false) => {
                            pc = check_branch(artifact, pc, *index)?;
                            continue;
                        }
                        Value::Bool(true) => {}
                        _ => {
                            return Err(RuntimeError::TypeMismatch {
                                op: "jump_if_false",
                            });
                        }
                    }
                }
                Instruction::JumpIfTrue(index) => {
                    let condition = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    match condition {
                        Value::Bool(true) => {
                            pc = check_branch(artifact, pc, *index)?;
                            continue;
                        }
                        Value::Bool(false) => {}
                        _ => {
                            return Err(RuntimeError::TypeMismatch { op: "jump_if_true" });
                        }
                    }
                }
                Instruction::IsType(ty) => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let is_match = ty.matches(&value);
                    stack.push(Value::Bool(is_match));
                }
                Instruction::ErrorKind => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let name = match &value {
                        Value::Error(err) => err.kind(),
                        _ => "ok",
                    };
                    stack.push(Value::Str(Rc::from(name)));
                }
                Instruction::Len => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let result = match value {
                        Value::Error(_) => value,
                        Value::Str(ref s) => Value::Int(s.chars().count() as i64),
                        Value::Array(ref arr) => match arr.try_borrow() {
                            Ok(items) => Value::Int(items.len() as i64),
                            Err(_) => Value::Error(Box::new(borrow_conflict("len"))),
                        },
                        _ => Value::Error(Box::new(RuntimeError::TypeMismatch { op: "len" })),
                    };
                    stack.push(result);
                }
                Instruction::Exclusive => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let result = match value {
                        Value::Error(_) => value,
                        Value::Array(arr) => Value::Bool(arr.strong_count() == 1),
                        Value::Record(fields) => Value::Bool(fields.strong_count() == 1),
                        Value::Cell(cell) => Value::Bool(cell.strong_count() == 1),
                        Value::StrBuf(buf) => Value::Bool(buf.strong_count() == 1),
                        Value::Str(s) => Value::Bool(Rc::strong_count(&s) == 1),
                        Value::Closure(_, captures) => Value::Bool(captures.strong_count() == 1),
                        _ => Value::Bool(true),
                    };
                    stack.push(result);
                }
                Instruction::Call(index, arity) => {
                    check_call(artifact, *index, *arity, 0)?;
                    check_branch(artifact, pc, pc + 1)?;
                    let next_base = stack
                        .len()
                        .checked_sub(*arity)
                        .ok_or(RuntimeError::StackUnderflow)?;
                    if !call_arguments_match(artifact, *index, &stack[next_base..]) {
                        stack.truncate(next_base);
                        stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                            op: "call",
                        })));
                    } else {
                        reserve_frame(&mut call_stack, context.limits.max_call_frames)?;
                        call_stack.push((pc + 1, bp, captures.clone(), operand_base));
                        operand_base = next_base;
                        captures = CaptureValues::default();
                        bp = locals.len();
                        pc = check_jump(program, *index)?;
                        continue;
                    }
                }
                Instruction::Return => {
                    let (return_pc, saved_bp, saved_captures, saved_operand_base) =
                        call_stack.pop().ok_or(RuntimeError::CallStackUnderflow)?;
                    if stack.len() <= operand_base {
                        return Err(RuntimeError::StackUnderflow);
                    }
                    let result = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let result = artifact
                        .callables
                        .range(..=pc)
                        .next_back()
                        .and_then(|(_, signature)| signature.result_type.as_ref())
                        .filter(|expected| !matches_runtime_type(expected, &result))
                        .map_or(result, |_| {
                            Value::Error(Box::new(RuntimeError::TypeMismatch { op: "return" }))
                        });
                    if matches!(result, Value::Error(_)) {
                        error_value_pc = Some(pc);
                    }
                    stack.truncate(operand_base);
                    stack.push(result);
                    operand_base = saved_operand_base;
                    locals.truncate(bp);
                    bp = saved_bp;
                    captures = saved_captures;
                    pc = check_jump(program, return_pc)?;
                    continue;
                }
                Instruction::And => {
                    binop(&mut stack, |a, b| match (a, b) {
                        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(x && y)),
                        _ => Err(RuntimeError::TypeMismatch { op: "and" }),
                    })?;
                }
                Instruction::Or => {
                    binop(&mut stack, |a, b| match (a, b) {
                        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(x || y)),
                        _ => Err(RuntimeError::TypeMismatch { op: "or" }),
                    })?;
                }
                Instruction::Xor => {
                    binop(&mut stack, |a, b| match (a, b) {
                        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(x != y)),
                        _ => Err(RuntimeError::TypeMismatch { op: "xor" }),
                    })?;
                }
                Instruction::Dup => {
                    let top = stack.last().ok_or(RuntimeError::StackUnderflow)?.clone();
                    stack.push(top);
                }
                Instruction::Pop => {
                    stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                }
                Instruction::Swap => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(b);
                    stack.push(a);
                }
                Instruction::StackTruncate(depth) => {
                    let target = operand_base
                        .checked_add(*depth)
                        .filter(|target| *target <= stack.len())
                        .ok_or(RuntimeError::StackUnderflow)?;
                    stack.truncate(target);
                }
                Instruction::CallValue(arity) => {
                    let target = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let next_base = stack
                        .len()
                        .checked_sub(*arity)
                        .ok_or(RuntimeError::StackUnderflow)?;
                    match target {
                        Value::Function(addr) => {
                            check_call(artifact, addr, *arity, 0)?;
                            check_branch(artifact, pc, pc + 1)?;
                            if !call_arguments_match(artifact, addr, &stack[next_base..]) {
                                stack.truncate(next_base);
                                stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "call",
                                })));
                            } else {
                                reserve_frame(&mut call_stack, context.limits.max_call_frames)?;
                                call_stack.push((pc + 1, bp, captures.clone(), operand_base));
                                operand_base = next_base;
                                captures = CaptureValues::default();
                                bp = locals.len();
                                pc = check_jump(program, addr)?;
                                continue;
                            }
                        }
                        Value::Closure(addr, closure_captures) => {
                            check_call(artifact, addr, *arity, closure_captures.len())?;
                            check_branch(artifact, pc, pc + 1)?;
                            if !call_arguments_match(artifact, addr, &stack[next_base..]) {
                                stack.truncate(next_base);
                                stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "call",
                                })));
                            } else {
                                reserve_frame(&mut call_stack, context.limits.max_call_frames)?;
                                call_stack.push((pc + 1, bp, captures.clone(), operand_base));
                                operand_base = next_base;
                                captures = CaptureValues(Some(closure_captures));
                                bp = locals.len();
                                pc = check_jump(program, addr)?;
                                continue;
                            }
                        }
                        Value::Error(_) => {
                            stack.truncate(next_base);
                            stack.push(target);
                        }
                        _ => {
                            stack.truncate(next_base);
                            stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                op: "call_value",
                            })));
                        }
                    }
                }
                Instruction::Index => {
                    let (object, key) = pop2(&mut stack)?;
                    // Type metadata is unspellable in source. This generic query
                    // lets lowered patterns inspect nominal identity without a
                    // match/enum instruction or a forgeable record field.
                    let result = match (&object, &key) {
                        (Value::Variant(tag, _), Value::TypeTag(_)) => {
                            Ok(Value::TypeTag(tag.clone()))
                        }
                        (Value::Error(err), Value::TypeTag(_)) => Ok(Value::TypeTag(err.tag())),
                        (_, Value::TypeTag(_)) => Ok(Value::Nil),
                        (Value::Error(err), Value::Str(name)) => {
                            err.field(name).map_or_else(|| missing_field(name), Ok)
                        }
                        (Value::Error(_), _) => Ok(object),
                        (_, Value::Error(_)) => Ok(key),
                        (Value::Array(arr), Value::Int(i)) => arr
                            .try_borrow()
                            .map_err(|_| borrow_conflict("index"))
                            .and_then(|items| {
                                items
                                    .get(usize::try_from(*i).unwrap_or(usize::MAX))
                                    .cloned()
                                    .ok_or(RuntimeError::IndexOutOfBounds {
                                        index: *i,
                                        len: items.len(),
                                    })
                            }),
                        (Value::Record(fields), Value::Str(name)) => fields
                            .try_borrow()
                            .map_err(|_| borrow_conflict("index"))
                            .and_then(|fields| {
                                fields
                                    .get(name.as_ref())
                                    .cloned()
                                    .map_or_else(|| missing_field(name), Ok)
                            }),
                        (Value::Variant(_, fields), Value::Str(name)) => fields
                            .get(name.as_ref())
                            .map_or_else(|| missing_field(name), |value| Ok(value.clone())),
                        _ => Err(RuntimeError::TypeMismatch { op: "index" }),
                    };
                    stack.push(match result {
                        Ok(value) => value,
                        Err(error) => Value::Error(Box::new(error)),
                    });
                }
                Instruction::IndexSet => {
                    let (array, idx, value) = pop3(&mut stack)?;
                    if let Value::Error(_) = array {
                        stack.push(array);
                    } else if let Value::Error(_) = idx {
                        stack.push(idx);
                    } else if let Value::Error(_) = value {
                        stack.push(value);
                    } else {
                        match (&array, &idx) {
                            (Value::Array(arr), Value::Int(i)) => match arr.try_borrow() {
                                Err(_) => {
                                    stack.push(Value::Error(Box::new(borrow_conflict("indexset"))))
                                }
                                Ok(items) => {
                                    let len = items.len();
                                    drop(items);
                                    if *i < 0 || (*i as usize) >= len {
                                        stack.push(Value::Error(Box::new(
                                            RuntimeError::IndexOutOfBounds { index: *i, len },
                                        )));
                                    } else if arr.set(*i as usize, value).is_ok() {
                                        stack.push(array.clone());
                                    } else {
                                        stack.push(Value::Error(Box::new(
                                            RuntimeError::TypeMismatch { op: "indexset" },
                                        )));
                                    }
                                }
                            },
                            (Value::Record(record), Value::Str(key)) => match record.try_borrow() {
                                Err(_) => {
                                    stack.push(Value::Error(Box::new(borrow_conflict("indexset"))))
                                }
                                Ok(fields) => {
                                    let current = fields.get(key.as_ref()).cloned();
                                    drop(fields);
                                    let valid = match (&value, current.as_ref()) {
                                        (
                                            Value::Array(replacement),
                                            Some(Value::Array(template)),
                                        ) if record.is_nominal() => {
                                            template.element_type().is_some_and(|expected| {
                                                replacement.constrain(expected).is_ok()
                                            })
                                        }
                                        (
                                            Value::Array(replacement),
                                            Some(Value::Array(template)),
                                        ) if replacement.needs_type_context() => {
                                            replacement.constrain_from(template).is_ok()
                                        }
                                        (Value::Array(replacement), _) => {
                                            !replacement.needs_type_context()
                                        }
                                        _ => true,
                                    };
                                    if valid {
                                        match record.try_borrow_mut() {
                                            Ok(mut fields) => {
                                                if let Some(existing) = fields.get_mut(key.as_ref())
                                                {
                                                    *existing = value;
                                                    stack.push(array.clone());
                                                } else {
                                                    let insert =
                                                        try_record_key(key).and_then(|key| {
                                                            fields.try_reserve(1).map_err(
                                                                |_| RuntimeError::ResourceLimit {
                                                                    resource: "record",
                                                                },
                                                            )?;
                                                            fields.insert(key, value);
                                                            Ok(())
                                                        });
                                                    match insert {
                                                        Ok(()) => stack.push(array.clone()),
                                                        Err(error) => stack
                                                            .push(Value::Error(Box::new(error))),
                                                    }
                                                }
                                            }
                                            Err(_) => stack.push(Value::Error(Box::new(
                                                borrow_conflict("indexset"),
                                            ))),
                                        }
                                    } else {
                                        stack.push(Value::Error(Box::new(
                                            RuntimeError::TypeMismatch { op: "indexset" },
                                        )));
                                    }
                                }
                            },
                            _ => {
                                stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "indexset",
                                })));
                            }
                        }
                    }
                }
                Instruction::MakeCell => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    stack.push(Value::Cell(HeapRef::try_new(RefCell::new(value))?));
                }
                Instruction::CellGet => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let result = match value {
                        Value::Error(_) => value,
                        Value::Cell(ref c) => match c.try_borrow() {
                            Ok(value) => value.clone(),
                            Err(_) => Value::Error(Box::new(borrow_conflict("cellget"))),
                        },
                        _ => Value::Error(Box::new(RuntimeError::TypeMismatch { op: "cellget" })),
                    };
                    stack.push(result);
                }
                Instruction::MakeArray(count) => {
                    let items = take_stack_tail(&mut stack, *count)?;
                    stack.push(match ArrayValue::new(items) {
                        Ok(array) => Value::Array(HeapRef::try_new(array)?),
                        Err(_) => {
                            Value::Error(Box::new(RuntimeError::TypeMismatch { op: "array" }))
                        }
                    });
                }
                Instruction::MakeTuple(count) => {
                    let items = take_stack_tail(&mut stack, *count)?;
                    stack.push(Value::Array(HeapRef::try_new(ArrayValue::try_tuple(
                        items,
                    )?)?));
                }
                Instruction::MakeTypedArray(count, expected) => {
                    let items = take_stack_tail(&mut stack, *count)?;
                    stack.push(match ArrayValue::typed(items, expected.clone()) {
                        Ok(array) => Value::Array(HeapRef::try_new(array)?),
                        Err(_) => {
                            Value::Error(Box::new(RuntimeError::TypeMismatch { op: "array" }))
                        }
                    });
                }
                Instruction::ConstrainArrayType(expected) => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let valid = match &value {
                        Value::Array(array) => array.constrain(expected.clone()).is_ok(),
                        Value::Error(_) => true,
                        _ => false,
                    };
                    stack.push(if valid {
                        value
                    } else {
                        Value::Error(Box::new(RuntimeError::TypeMismatch { op: "array_type" }))
                    });
                }
                Instruction::ConstrainArrayFrom => {
                    let (value, template) = pop2(&mut stack)?;
                    let result = match value {
                        Value::Array(array) if !array.needs_type_context() => Value::Array(array),
                        Value::Array(array) => match template {
                            Value::Array(template) => match array.constrain_from(&template) {
                                Ok(()) => Value::Array(array),
                                Err(_) => Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "array_type",
                                })),
                            },
                            _ => Value::Error(Box::new(RuntimeError::TypeMismatch {
                                op: "array_type",
                            })),
                        },
                        value => value,
                    };
                    stack.push(result);
                }
                Instruction::ArrayEmptyLike => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    let result = match value {
                        Value::Array(array) => match array.element_type() {
                            Some(element_type) => match ArrayValue::typed(Vec::new(), element_type)
                            {
                                Ok(empty) => Value::Array(HeapRef::try_new(empty)?),
                                Err(_) => Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "array_empty_like",
                                })),
                            },
                            None => Value::Error(Box::new(RuntimeError::TypeMismatch {
                                op: "array_empty_like",
                            })),
                        },
                        Value::Error(error) => Value::Error(error),
                        _ => Value::Error(Box::new(RuntimeError::TypeMismatch {
                            op: "array_empty_like",
                        })),
                    };
                    stack.push(result);
                }
                Instruction::MakeRecord(count) => {
                    let slots = count.checked_mul(2).ok_or(RuntimeError::StackUnderflow)?;
                    let entries = take_stack_tail(&mut stack, slots)?;
                    let mut entries = entries.into_iter();
                    let mut fields = std::collections::HashMap::new();
                    let mut tag = None;
                    let mut error = None;
                    for _ in 0..*count {
                        let key = entries.next().ok_or(RuntimeError::StackUnderflow)?;
                        let value = entries.next().ok_or(RuntimeError::StackUnderflow)?;
                        match key {
                            Value::TypeTag(name) if tag.is_none() => tag = Some(name),
                            Value::Str(key) => {
                                fields
                                    .try_reserve(1)
                                    .map_err(|_| RuntimeError::ResourceLimit {
                                        resource: "record",
                                    })?;
                                fields.insert(try_record_key(&key)?, value);
                            }
                            Value::Error(error_value) if error.is_none() => {
                                error = Some(Value::Error(error_value));
                            }
                            _ if error.is_none() => {
                                error = Some(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                    op: "record",
                                })))
                            }
                            _ => {}
                        }
                    }
                    let value = if let Some(error) = error {
                        error
                    } else {
                        match tag {
                            Some(tag)
                                if tag
                                    .strip_prefix(crate::error::ERROR_TYPE)
                                    .is_some_and(|tail| tail.starts_with("::")) =>
                            {
                                Value::Error(Box::new(RuntimeError::LanguageError {
                                    tag,
                                    fields: HeapRef::try_new(ValueFields::new(fields))?,
                                }))
                            }
                            Some(tag) if tag.starts_with("@struct::") => {
                                Value::Record(HeapRef::try_new(RecordValue::nominal(
                                    fields,
                                    try_record_key(&tag[9..])?,
                                ))?)
                            }
                            Some(tag) => {
                                Value::Variant(tag, HeapRef::try_new(ValueFields::new(fields))?)
                            }
                            None => Value::Record(HeapRef::try_new(RecordValue::new(fields))?),
                        }
                    };
                    stack.push(value);
                }
                Instruction::CellSet => {
                    let (cell, new_value) = pop2(&mut stack)?;
                    if let Value::Error(_) = cell {
                        stack.push(cell);
                    } else if let Value::Error(_) = new_value {
                        stack.push(new_value);
                    } else {
                        match &cell {
                            Value::Cell(c) => match c.try_borrow_mut() {
                                Ok(mut value) => {
                                    *value = new_value;
                                    stack.push(cell.clone());
                                }
                                Err(_) => {
                                    stack.push(Value::Error(Box::new(borrow_conflict("cellset"))))
                                }
                            },
                            _ => stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                op: "cellset",
                            }))),
                        }
                    }
                }
                Instruction::MakeClosure(addr, count) => {
                    check_jump(program, *addr)?;
                    if artifact
                        .callables
                        .get(addr)
                        .is_none_or(|signature| signature.captures != *count)
                    {
                        return Err(RuntimeError::InvalidCall(*addr));
                    }
                    let captured = take_stack_tail(&mut stack, *count)?;
                    stack.push(Value::Closure(
                        *addr,
                        HeapRef::try_new(ClosureCaptures::new(captured))?,
                    ));
                }
                Instruction::LoadCapture(index) => {
                    let value = captures
                        .get(*index)
                        .ok_or(RuntimeError::InvalidCapture(*index))?;
                    stack.push(value);
                }
                Instruction::Crash(context) => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    return Err(RuntimeError::Crashed {
                        context: context.clone(),
                        value,
                    });
                }
                Instruction::Not => {
                    let value = stack.pop().ok_or(RuntimeError::StackUnderflow)?;
                    if let Value::Error(_) = value {
                        stack.push(value);
                    } else {
                        match value {
                            Value::Bool(x) => stack.push(Value::Bool(!x)),
                            _ => stack.push(Value::Error(Box::new(RuntimeError::TypeMismatch {
                                op: "not",
                            }))),
                        }
                    }
                }
            }
            if produced != 0 {
                let output_start = stack.len().saturating_sub(produced);
                if stack[output_start..]
                    .iter()
                    .any(|value| matches!(value, Value::Error(_)))
                {
                    error_value_pc = Some(pc);
                }
            }
            if artifact.callables.contains_key(&(pc + 1)) {
                return Err(RuntimeError::InvalidJump(pc + 1));
            }
            pc += 1;
        }
        Err(RuntimeError::MissingHalt)
    })();

    let source_pc = result.as_ref().err().map(|_| halt_error_pc.unwrap_or(pc));
    let source_location = source_pc.and_then(|instruction| {
        artifact
            .source_location(instruction)
            .or_else(|| artifact.source_location_before(instruction))
            .cloned()
    });
    context.set_source_location(source_location);

    // Keep the VM's roots alive until the collector has detached all edges in
    // objects that become unreachable when this execution frame exits.
    if let Err(cleanup) = reserve_cleanup_roots(
        &mut stack,
        [
            locals.iter().flatten().count(),
            globals.iter().flatten().count(),
            usize::from(captures.0.is_some()),
            call_stack
                .iter()
                .filter(|(_, _, captures, _)| captures.0.is_some())
                .count(),
        ],
    ) {
        // Match the collector's reservation-failure protocol: retain all roots
        // rather than recursively dropping a graph under allocation pressure.
        std::mem::forget(stack);
        std::mem::forget(locals);
        std::mem::forget(globals);
        std::mem::forget(captures);
        std::mem::forget(call_stack);
        return result.and(Err(cleanup));
    }
    let mut discarded = stack;
    discarded.extend(locals.into_iter().flatten());
    discarded.extend(globals.into_iter().flatten());
    if let Some(captures) = captures.into_root() {
        discarded.push(captures);
    }
    for (_, _, frame_captures, _) in call_stack {
        if let Some(captures) = frame_captures.into_root() {
            discarded.push(captures);
        }
    }
    let collection = heap::collect(&discarded).map(|_| ());
    if collection.is_ok() {
        drop(discarded);
    } else {
        // Preserve the graph if collection cannot reserve its work list. A
        // recursive fallback drop could overflow the host stack.
        std::mem::forget(discarded);
    }
    result.and(collection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_root_reservation_preserves_values_on_size_or_allocation_failure() {
        let mut values = vec![Value::Int(7)];
        for counts in [[usize::MAX, 1, 0, 0], [usize::MAX, 0, 0, 0]] {
            assert!(matches!(
                reserve_cleanup_roots(&mut values, counts),
                Err(RuntimeError::ResourceLimit {
                    resource: "VM cleanup roots"
                })
            ));
            assert_eq!(values.len(), 1);
            assert!(matches!(values[0], Value::Int(7)));
        }
    }

    #[derive(Default)]
    struct TrackingWriter {
        buf: Vec<u8>,
        flush_count: usize,
    }

    impl std::io::Write for TrackingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flush_count += 1;
            Ok(())
        }
    }

    fn compile(source: &str) -> CompiledProgram {
        crate::project::compile_source(
            "vm-test.cog",
            &format!("include std; use io; use err::Error; {source}"),
        )
        .unwrap()
    }

    #[test]
    fn record_key_copy_reports_allocation_failure() {
        assert!(matches!(
            try_record_key_with_capacity("field", usize::MAX),
            Err(RuntimeError::ResourceLimit { resource: "record" })
        ));
    }

    #[test]
    fn record_index_set_can_insert_a_new_field() {
        let record = HeapRef::new(RecordValue::new(std::collections::HashMap::new()));
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Record(record.clone())),
            Instruction::Push(Value::Str("field".into())),
            Instruction::Push(Value::Int(7)),
            Instruction::IndexSet,
            Instruction::Pop,
            Instruction::Halt,
        ]);

        run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::default()).unwrap();

        let fields = record.borrow();
        assert!(matches!(fields.get("field"), Some(Value::Int(7))));
    }

    #[test]
    fn print_is_blocked_when_permission_is_disabled() {
        let program = compile("fn main() { return io::print(7); }");
        let mut out = Vec::new();
        assert!(matches!(
            run_with_capabilities(&program, &mut out, VmCapabilities::restricted()),
            Err(RuntimeError::PermissionDenied { .. })
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn execution_collects_unreachable_self_referential_cells() {
        assert_eq!(crate::heap::live_objects(), 0);
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Nil),
            Instruction::MakeCell,
            Instruction::Dup,
            Instruction::Dup,
            Instruction::CellSet,
            Instruction::Pop,
            Instruction::Halt,
        ]);
        run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::restricted()).unwrap();
        assert_eq!(crate::heap::live_objects(), 0);
    }

    #[test]
    fn forgotten_heap_roots_remain_charged_to_the_runtime_object_limit() {
        let limit = crate::heap::live_objects();
        let leaked = Value::Record(HeapRef::new(RecordValue::new(
            std::collections::HashMap::new(),
        )));
        std::mem::forget(leaked);

        let program = CompiledProgram::new(vec![Instruction::Halt]);
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        context.limits.max_heap_objects = Some(limit);
        assert!(matches!(
            run_with_context(&program, &mut context),
            Err(RuntimeError::ResourceLimit {
                resource: "VM heap objects"
            })
        ));
        assert_eq!(crate::heap::live_objects(), limit + 1);
    }

    #[test]
    fn execution_drops_deep_managed_graphs_iteratively() {
        assert_eq!(crate::heap::live_objects(), 0);
        const DEPTH: usize = 5_000;
        let mut instructions = Vec::with_capacity(DEPTH * 3 + 3);
        instructions.push(Instruction::Push(Value::Int(0)));
        for _ in 0..DEPTH {
            instructions.push(Instruction::Push(Value::Str("next".into())));
            instructions.push(Instruction::MakeRecord(1));
            instructions.push(Instruction::MakeArray(1));
        }
        instructions.push(Instruction::Pop);
        instructions.push(Instruction::Halt);
        let program = CompiledProgram::new(instructions);
        run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::restricted()).unwrap();
        assert_eq!(crate::heap::live_objects(), 0);
    }

    #[test]
    fn push_array_checks_nested_heap_cost_before_allocating() {
        let template = ArrayTemplate::Array(vec![crate::template::ArrayItem {
            template: ArrayTemplate::Array(Vec::new()),
            repeat: 2,
        }]);
        assert_eq!(array_template_cost(&template).unwrap(), (3, 3));

        let program =
            CompiledProgram::new(vec![Instruction::PushArray(template), Instruction::Halt]);
        let mut out = Vec::new();
        let mut context = RuntimeContext::new(&mut out, VmCapabilities::restricted());
        context.limits.max_heap_objects = Some(2);
        assert!(matches!(
            run_with_context(&program, &mut context),
            Err(RuntimeError::ResourceLimit {
                resource: "VM heap objects"
            })
        ));
        assert_eq!(crate::heap::live_objects(), 0);
    }

    #[test]
    fn push_array_cost_rejects_excessive_in_memory_templates() {
        let template = ArrayTemplate::Array(vec![crate::template::ArrayItem {
            template: ArrayTemplate::Scalar(Value::Nil),
            repeat: crate::bytecode::LIMIT + 1,
        }]);
        assert!(matches!(
            array_template_cost(&template),
            Err(RuntimeError::ResourceLimit {
                resource: "array template"
            })
        ));
    }

    #[test]
    fn print_and_println_flush_output() {
        let program = compile("fn main() { io::print(\"hi\"); io::println(42); }");
        let mut out = TrackingWriter::default();
        run_with(&program, &mut out).unwrap();
        assert_eq!(out.buf, b"hi42\n");
        assert_eq!(out.flush_count, 2);
    }

    #[test]
    fn exclusive_reports_shared_values() {
        let program = compile("fn main() { const a = [1]; const b = a; io::print(exclusive(a)); }");
        let mut out = Vec::new();
        run_with(&program, &mut out).unwrap();
        assert_eq!(out, b"false");
    }

    #[test]
    fn sleep_is_blocked_when_permission_is_disabled() {
        let program = compile("use time; fn main() { io::print(time::sleep(1)); }");
        let caps = VmCapabilities {
            allow_sleep: false,
            ..VmCapabilities::default()
        };
        let mut out = Vec::new();
        run_with_capabilities(&program, &mut out, caps).unwrap();
        assert_eq!(out, b"Error: permission denied for `time::sleep`");
    }

    #[test]
    fn array_push_mutates_in_place_through_alias() {
        // `arr << 3;` as a bare statement (result popped/discarded, exactly like
        // codegen emits for `Stmt::Expr`) must still be visible through any other
        // handle to the same array — that's the whole point of it being
        // `Rc<RefCell<..>>` rather than a plain Vec. The old "reuse Add" codegen
        // built a brand-new array and left `arr` untouched, so a bare `arr << x;`
        // silently did nothing.
        let arr = HeapRef::new(ArrayValue::new(vec![Value::Int(1), Value::Int(2)]).unwrap());
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Array(arr.clone())),
            Instruction::Push(Value::Int(3)),
            Instruction::ArrayPush,
            Instruction::Pop, // as if `arr << 3;` were a statement, not assigned
            Instruction::Halt,
        ]);
        let mut out = Vec::new();
        run_with_capabilities(&program, &mut out, VmCapabilities::default()).unwrap();
        let borrowed = arr.borrow();
        assert_eq!(borrowed.len(), 3);
        assert!(matches!(borrowed[0], Value::Int(1)));
        assert!(matches!(borrowed[1], Value::Int(2)));
        assert!(matches!(borrowed[2], Value::Int(3)));
    }

    #[test]
    fn array_prepend_mutates_in_place_through_alias() {
        let arr = HeapRef::new(ArrayValue::new(vec![Value::Int(2), Value::Int(3)]).unwrap());
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Int(1)),
            Instruction::Push(Value::Array(arr.clone())),
            Instruction::ArrayPrepend,
            Instruction::Pop, // as if `1 >> arr;` were a statement
            Instruction::Halt,
        ]);
        let mut out = Vec::new();
        run_with_capabilities(&program, &mut out, VmCapabilities::default()).unwrap();
        let borrowed = arr.borrow();
        assert_eq!(borrowed.len(), 3);
        assert!(matches!(borrowed[0], Value::Int(1)));
        assert!(matches!(borrowed[1], Value::Int(2)));
        assert!(matches!(borrowed[2], Value::Int(3)));
    }

    #[test]
    fn array_push_on_non_array_is_type_mismatch() {
        let program = compile(
            "fn bad() { return 1; } fn main() { when bad() << 2 is Error { io::print(true); } }",
        );
        let mut out = Vec::new();
        run_with(&program, &mut out).unwrap();
        assert_eq!(out, b"true");
    }

    #[test]
    fn eq_between_nil_and_non_nil_is_false_not_type_mismatch() {
        let program = compile("fn main() { io::print(5 == nil); }");
        let mut out = Vec::new();
        run_with(&program, &mut out).unwrap();
        assert_eq!(out, b"false");
    }

    #[test]
    fn ne_between_nil_and_non_nil_is_true_not_type_mismatch() {
        let program = compile("fn main() { io::print(\"hi\" != nil); }");
        let mut out = Vec::new();
        run_with(&program, &mut out).unwrap();
        assert_eq!(out, b"true");
    }

    #[test]
    fn array_push_rejects_an_incompatible_value_without_mutating_aliases() {
        let arr = HeapRef::new(ArrayValue::new(vec![Value::Int(1)]).unwrap());
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Array(arr.clone())),
            Instruction::Push(Value::Str("wrong".into())),
            Instruction::ArrayPush,
            Instruction::Halt,
        ]);
        let error = run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::TypeMismatch { op: "arrpush" }
        ));
        assert_eq!(arr.borrow().len(), 1);
        assert!(matches!(arr.borrow()[0], Value::Int(1)));
    }

    #[test]
    fn array_index_set_rejects_an_incompatible_value_without_mutating_aliases() {
        let arr = HeapRef::new(ArrayValue::new(vec![Value::Int(1)]).unwrap());
        let program = CompiledProgram::new(vec![
            Instruction::Push(Value::Array(arr.clone())),
            Instruction::Push(Value::Int(0)),
            Instruction::Push(Value::Str("wrong".into())),
            Instruction::IndexSet,
            Instruction::Halt,
        ]);
        let error = run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::TypeMismatch { op: "indexset" }
        ));
        assert_eq!(arr.borrow().len(), 1);
        assert!(matches!(arr.borrow()[0], Value::Int(1)));
    }
}
