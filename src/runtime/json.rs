//! Bounded JSON conversion between JSON values and Cog's dynamic runtime values.
use super::RuntimeContext;
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{ArrayValue, RecordValue, Value};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::ser::{Error as _, SerializeMap, SerializeSeq};
use serde::{Deserializer, Serialize, Serializer};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

const MAX_DEPTH: usize = 64;
const RESOURCE: &str = "$cog:resource";
const DEPTH: &str = "$cog:depth";
const OVERFLOW: &str = "$cog:overflow";
const DUPLICATE: &str = "$cog:duplicate";
const CYCLE: &str = "$cog:cycle";
const BORROW: &str = "$cog:borrow";
const TYPE: &str = "$cog:type";
const INVALID: &str = "$cog:invalid";

pub(super) fn parse(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::Str(text) = &args[0] else {
        return Err(RuntimeError::InvalidNativeCall);
    };
    if text.len() > context.limits.buffer_limit() {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }

    let mut budget = Budget {
        used: 0,
        limit: context.limits.buffer_limit(),
    };
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = ValueSeed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(parse_error)?;
    deserializer.end().map_err(parse_error)?;
    validate_integer_tokens(text)?;
    Ok(value)
}

// Serde's f64 visitor also receives integer tokens outside u64/i64. Inspect
// their original spelling after syntax validation so those tokens cannot be
// silently rounded. This scan allocates nothing and ignores quoted strings.
fn validate_integer_tokens(text: &str) -> Result<(), RuntimeError> {
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'"' {
            while let Some(byte) = bytes.next() {
                match byte {
                    b'\\' => {
                        bytes.next();
                    }
                    b'"' => break,
                    _ => {}
                }
            }
        } else if byte == b'-' || byte.is_ascii_digit() {
            let mut magnitude = u64::from(byte.saturating_sub(b'0'));
            let negative = byte == b'-';
            if negative {
                magnitude = 0;
            }
            let mut overflow = false;
            let mut fractional = false;
            while let Some(&byte) = bytes.peek() {
                if !(byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-')) {
                    break;
                }
                bytes.next();
                if byte.is_ascii_digit() {
                    match magnitude
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u64::from(byte - b'0')))
                    {
                        Some(n) => magnitude = n,
                        None => overflow = true,
                    }
                } else {
                    fractional = true;
                }
            }
            let limit = (i64::MAX as u64) + u64::from(negative);
            if !fractional && (overflow || magnitude > limit) {
                return Err(RuntimeError::Overflow);
            }
        }
    }
    Ok(())
}

pub(super) fn stringify(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if let Value::Error(_) = args[0] {
        return Ok(args[0].clone());
    }
    let mut writer = BoundedWriter::new(context.limits.buffer_limit());
    let state = Rc::new(SerializeState {
        depth: Cell::new(0),
        active: RefCell::new(HashSet::new()),
        allocation_limit: context.limits.buffer_limit(),
    });
    let result = {
        let mut serializer = serde_json::Serializer::new(&mut writer);
        JsonValue {
            value: &args[0],
            state,
        }
        .serialize(&mut serializer)
    };
    if result.is_err() {
        return Err(writer.error().unwrap_or_else(|| {
            result.err().map_or(
                RuntimeError::InvalidArgument {
                    op: "json::stringify",
                },
                |error| serialize_error(&error.to_string()),
            )
        }));
    }
    let text =
        String::from_utf8(writer.into_inner()).map_err(|_| RuntimeError::InvalidNativeCall)?;
    Ok(Value::Str(Rc::from(text)))
}

struct Budget {
    used: usize,
    limit: usize,
}

impl Budget {
    fn charge<E: serde::de::Error>(&mut self, amount: usize) -> Result<(), E> {
        self.used = self
            .used
            .checked_add(amount)
            .filter(|used| *used <= self.limit)
            .ok_or_else(|| E::custom(RESOURCE))?;
        Ok(())
    }

    fn node<E: serde::de::Error>(&mut self) -> Result<(), E> {
        // Account for the Cog Value plus conservative per-entry/container
        // bookkeeping. The input byte limit alone would allow tiny JSON tokens
        // to expand into a much larger object graph.
        self.charge(2 * std::mem::size_of::<Value>() + 2 * std::mem::size_of::<usize>())
    }
}

struct ValueSeed<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_DEPTH {
            return Err(serde::de::Error::custom(DEPTH));
        }
        deserializer.deserialize_any(ValueVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

struct ValueVisitor<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl ValueVisitor<'_> {
    fn charge_node<E: serde::de::Error>(&mut self) -> Result<(), E> {
        self.budget.node()
    }
}

impl<'de> Visitor<'de> for ValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Nil)
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Int(value))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        if value > i64::MAX as u64 {
            return Err(E::custom(OVERFLOW));
        }
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Int(value as i64))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        if !value.is_finite() {
            return Err(E::custom(OVERFLOW));
        }
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Float(value))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        this.budget.charge(value.len())?;
        Ok(Value::Str(Rc::from(value)))
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        this.budget.charge(value.len())?;
        Ok(Value::Str(Rc::from(value)))
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut sequence: A) -> Result<Value, A::Error> {
        self.charge_node()?;
        self.budget.charge(std::mem::size_of::<ArrayValue>())?;
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(ValueSeed {
            budget: &mut *self.budget,
            depth: self.depth + 1,
        })? {
            values
                .try_reserve(1)
                .map_err(|_| A::Error::custom(RESOURCE))?;
            values.push(value);
        }
        let array = HeapRef::try_new(ArrayValue::dynamic(values)).map_err(|error| match error {
            RuntimeError::ResourceLimit { .. } => A::Error::custom(RESOURCE),
            RuntimeError::InvalidArgument { .. } => A::Error::custom(BORROW),
            _ => A::Error::custom(INVALID),
        })?;
        Ok(Value::Array(array))
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut object: A) -> Result<Value, A::Error> {
        self.charge_node()?;
        self.budget.charge(std::mem::size_of::<RecordValue>())?;
        let mut fields = HashMap::new();
        while let Some(key) = object.next_key::<String>()? {
            self.budget
                .charge(key.len().saturating_add(4 * std::mem::size_of::<usize>()))?;
            if fields.contains_key(&key) {
                return Err(A::Error::custom(DUPLICATE));
            }
            fields
                .try_reserve(1)
                .map_err(|_| A::Error::custom(RESOURCE))?;
            let value = object.next_value_seed(ValueSeed {
                budget: &mut *self.budget,
                depth: self.depth + 1,
            })?;
            fields.insert(key, value);
        }
        let record = HeapRef::try_new(RecordValue::new(fields)).map_err(|error| match error {
            RuntimeError::ResourceLimit { .. } => A::Error::custom(RESOURCE),
            RuntimeError::InvalidArgument { .. } => A::Error::custom(BORROW),
            _ => A::Error::custom(INVALID),
        })?;
        Ok(Value::Record(record))
    }
}

fn parse_error(error: serde_json::Error) -> RuntimeError {
    let message = error.to_string();
    if message.contains(RESOURCE) {
        RuntimeError::ResourceLimit { resource: "bytes" }
    } else if message.contains(DEPTH) {
        RuntimeError::ResourceLimit {
            resource: "JSON nesting",
        }
    } else if message.contains(OVERFLOW) || message.contains("number out of range") {
        RuntimeError::Overflow
    } else if message.contains(BORROW) {
        RuntimeError::InvalidArgument {
            op: "json::parse allocation",
        }
    } else {
        // Duplicate names are rejected to avoid ambiguous object semantics.
        RuntimeError::ParseFailure {
            from: "JSON",
            to: "Cog value",
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    failure: Option<RuntimeError>,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            failure: None,
        }
    }

    fn error(&self) -> Option<RuntimeError> {
        self.failure.clone()
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(bytes.len()) else {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::new(io::ErrorKind::WriteZero, RESOURCE));
        };
        if length > self.limit {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::new(io::ErrorKind::WriteZero, RESOURCE));
        }
        if self.bytes.try_reserve(bytes.len()).is_err() {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::other(RESOURCE));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SerializeState {
    depth: Cell<usize>,
    active: RefCell<HashSet<(u8, usize)>>,
    allocation_limit: usize,
}

struct ValueGuard {
    state: Rc<SerializeState>,
    identity: Option<(u8, usize)>,
}

impl Drop for ValueGuard {
    fn drop(&mut self) {
        self.state
            .depth
            .set(self.state.depth.get().saturating_sub(1));
        if let Some(identity) = self.identity
            && let Ok(mut active) = self.state.active.try_borrow_mut()
        {
            active.remove(&identity);
        }
    }
}

fn enter_value<E: serde::ser::Error>(
    state: &Rc<SerializeState>,
    identity: Option<(u8, usize)>,
) -> Result<ValueGuard, E> {
    let depth = state.depth.get();
    // Match the parser's boundary: a value at depth MAX_DEPTH is allowed,
    // while its child at MAX_DEPTH + 1 is rejected.
    if depth > MAX_DEPTH {
        return Err(E::custom(DEPTH));
    }
    if let Some(identity) = identity {
        let mut active = state
            .active
            .try_borrow_mut()
            .map_err(|_| E::custom(BORROW))?;
        if active.contains(&identity) {
            return Err(E::custom(CYCLE));
        }
        active.try_reserve(1).map_err(|_| E::custom(RESOURCE))?;
        active.insert(identity);
    }
    state.depth.set(depth + 1);
    Ok(ValueGuard {
        state: state.clone(),
        identity,
    })
}

struct JsonValue<'a> {
    value: &'a Value,
    state: Rc<SerializeState>,
}

impl Serialize for JsonValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let identity = match self.value {
            Value::Array(array) => Some((0, array.identity())),
            Value::Record(record) => Some((1, record.identity())),
            _ => None,
        };
        let _guard = enter_value::<S::Error>(&self.state, identity)?;
        match self.value {
            Value::Nil => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Int(value) => serializer.serialize_i64(*value),
            Value::Float(value) if value.is_finite() => serializer.serialize_f64(*value),
            Value::Float(_) => Err(S::Error::custom(INVALID)),
            Value::Str(value) => serializer.serialize_str(value),
            Value::Array(array) => {
                let values = array.try_borrow().map_err(|_| S::Error::custom(BORROW))?;
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values.iter() {
                    sequence.serialize_element(&JsonValue {
                        value,
                        state: self.state.clone(),
                    })?;
                }
                sequence.end()
            }
            Value::Record(record) => {
                let fields = record
                    .try_borrow_fields()
                    .map_err(|_| S::Error::custom(BORROW))?;
                let key_bytes = fields
                    .len()
                    .checked_mul(std::mem::size_of::<&str>())
                    .filter(|bytes| *bytes <= self.state.allocation_limit)
                    .ok_or_else(|| S::Error::custom(RESOURCE))?;
                let mut keys = Vec::new();
                keys.try_reserve_exact(key_bytes / std::mem::size_of::<&str>())
                    .map_err(|_| S::Error::custom(RESOURCE))?;
                keys.extend(fields.keys().map(String::as_str));
                keys.sort_unstable();
                let mut object = serializer.serialize_map(Some(keys.len()))?;
                for key in keys {
                    let value = fields.get(key).ok_or_else(|| S::Error::custom(INVALID))?;
                    object.serialize_entry(
                        key,
                        &JsonValue {
                            value,
                            state: self.state.clone(),
                        },
                    )?;
                }
                object.end()
            }
            _ => Err(S::Error::custom(TYPE)),
        }
    }
}

fn serialize_error(message: &str) -> RuntimeError {
    if message.contains(RESOURCE) {
        RuntimeError::ResourceLimit { resource: "bytes" }
    } else if message.contains(DEPTH) {
        RuntimeError::ResourceLimit {
            resource: "JSON nesting",
        }
    } else if message.contains(CYCLE) {
        RuntimeError::InvalidArgument {
            op: "json::stringify cycle",
        }
    } else if message.contains(BORROW) {
        RuntimeError::InvalidArgument {
            op: "json::stringify borrow",
        }
    } else if message.contains(TYPE) {
        RuntimeError::TypeMismatch {
            op: "json::stringify",
        }
    } else {
        RuntimeError::InvalidArgument {
            op: "json::stringify",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::VmCapabilities;

    #[test]
    fn parser_rejects_ambiguous_and_over_budget_inputs() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from(r#"{"a":1,"a":2}"#))]),
            Err(RuntimeError::ParseFailure { .. })
        ));

        context.limits.max_buffer_bytes = Some(8);
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from("1"))]),
            Err(RuntimeError::ResourceLimit { .. })
        ));

        context.limits.max_buffer_bytes = Some(1024);
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from("9223372036854775808"))]),
            Err(RuntimeError::Overflow)
        ));
    }

    #[test]
    fn parser_limits_nesting_and_builds_heterogeneous_mutable_arrays() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from(nested))]),
            Err(RuntimeError::ResourceLimit {
                resource: "JSON nesting"
            })
        ));

        let Value::Array(array) =
            parse(&mut context, &[Value::Str(Rc::from(r#"[1,"two",null]"#))]).unwrap()
        else {
            panic!("JSON array did not produce a Cog array");
        };
        array.try_push(Value::Bool(false), false).unwrap();
        array.set(0, Value::Str(Rc::from("one"))).unwrap();
        let result = stringify(&mut context, &[Value::Array(array)]).unwrap();
        assert!(
            matches!(result, Value::Str(text) if text.as_ref() == r#"["one","two",null,false]"#)
        );
    }

    #[test]
    fn parser_maximum_nesting_round_trips_through_stringifier() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let nested = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        let value = parse(&mut context, &[Value::Str(Rc::from(nested.as_str()))]).unwrap();
        let result = stringify(&mut context, &[value]).unwrap();
        assert!(matches!(result, Value::Str(text) if text.as_ref() == nested));
    }

    #[test]
    fn serializer_bounds_output_and_rejects_cycles_and_unsupported_values() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        context.limits.max_buffer_bytes = Some(2);
        assert!(matches!(
            stringify(&mut context, &[Value::Str(Rc::from("long"))]),
            Err(RuntimeError::ResourceLimit { .. })
        ));

        context.limits.max_buffer_bytes = Some(1024);
        assert!(matches!(
            stringify(
                &mut context,
                &[Value::Bytes(
                    HeapRef::try_new(RefCell::new(Vec::new())).unwrap()
                )]
            ),
            Err(RuntimeError::TypeMismatch { .. })
        ));

        assert!(matches!(
            stringify(&mut context, &[Value::Float(f64::INFINITY)]),
            Err(RuntimeError::InvalidArgument { .. })
        ));

        let mut nested = Value::Nil;
        for _ in 0..=MAX_DEPTH {
            nested = Value::Array(HeapRef::try_new(ArrayValue::dynamic(vec![nested])).unwrap());
        }
        assert!(matches!(
            stringify(&mut context, &[nested]),
            Err(RuntimeError::ResourceLimit {
                resource: "JSON nesting"
            })
        ));

        let array = HeapRef::try_new(ArrayValue::dynamic(Vec::new())).unwrap();
        array.try_push(Value::Array(array.clone()), false).unwrap();
        assert!(matches!(
            stringify(&mut context, &[Value::Array(array.clone())]),
            Err(RuntimeError::InvalidArgument { .. })
        ));
        drop(array);
    }
}
