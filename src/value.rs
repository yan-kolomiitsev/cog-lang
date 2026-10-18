use crate::error::RuntimeError;
use crate::heap::{HeapObject, HeapRef, ObjectKey};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

/// Runtime identity carried by each shared array object. The descriptor is
/// derived from values when an array is created and is fixed by its first
/// insertion when an empty array is used without an explicit runtime hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeType {
    Int,
    Float,
    Bool,
    Nil,
    Str,
    Error(String),
    Array(Box<RuntimeType>),
    Record,
    Variant(String),
    Function,
    Other,
    Nominal(String),
    Nullable(Box<RuntimeType>),
    Tuple(Vec<RuntimeType>),
}

/// Orders an exact signed integer against an f64 without first rounding the
/// integer into floating-point. `None` represents NaN, like `partial_cmp`.
pub(crate) fn compare_int_float(integer: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float >= 9223372036854775808.0 {
        return Some(Ordering::Less);
    }
    if float < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }
    let truncated = float as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => (truncated as f64).partial_cmp(&float),
        order => Some(order),
    }
}

#[derive(Debug)]
pub struct ArrayValue {
    values: RefCell<Vec<Value>>,
    element_type: RefCell<Option<RuntimeType>>,
    tuple_types: RefCell<Option<Vec<RuntimeType>>>,
    /// Arrays decoded from dynamic formats such as JSON do not have a single
    /// element type. They retain normal mutable array operations while staying
    /// distinct from source arrays whose empty type has not yet been inferred.
    dynamic: Cell<bool>,
    structure_version: Cell<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ArrayTypeError;

/// Compiler-created state for fail-fast array/string traversal. It is a heap
/// object so the source collection remains rooted for the loop's lifetime.
pub struct IteratorValue {
    source: Value,
    index: usize,
    byte_offset: usize,
    expected_structure_version: Option<u64>,
    current: Option<Value>,
}

impl IteratorValue {
    pub(crate) fn new(source: Value) -> Result<Self, RuntimeError> {
        let expected_structure_version = match &source {
            Value::Array(array) => Some(array.structure_version()),
            Value::Str(_) => None,
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    op: "for iteration",
                });
            }
        };
        Ok(Self {
            source,
            index: 0,
            byte_offset: 0,
            expected_structure_version,
            current: None,
        })
    }

    pub(crate) fn next(&mut self) -> Result<bool, RuntimeError> {
        self.current = None;
        match self.source.clone() {
            Value::Array(array) => {
                if Some(array.structure_version()) != self.expected_structure_version {
                    return Err(RuntimeError::InvalidArgument {
                        op: "array iteration invalidated",
                    });
                }
                let values =
                    array
                        .values
                        .try_borrow()
                        .map_err(|_| RuntimeError::InvalidArgument {
                            op: "array iteration",
                        })?;
                let Some(value) = values.get(self.index).cloned() else {
                    return Ok(false);
                };
                self.index += 1;
                self.current = Some(value);
                Ok(true)
            }
            Value::Str(text) => {
                let Some(rest) = text.get(self.byte_offset..) else {
                    return Err(RuntimeError::InvalidArgument {
                        op: "string iteration",
                    });
                };
                let Some(character) = rest.chars().next() else {
                    return Ok(false);
                };
                let end = self.byte_offset + character.len_utf8();
                self.current = Some(Value::Str(Rc::from(&text[self.byte_offset..end])));
                self.byte_offset = end;
                self.index += 1;
                Ok(true)
            }
            _ => Err(RuntimeError::TypeMismatch {
                op: "for iteration",
            }),
        }
    }

    pub(crate) fn current(&self) -> Result<Value, RuntimeError> {
        self.current.clone().ok_or(RuntimeError::InvalidArgument {
            op: "iterator has no current value",
        })
    }
}

impl ArrayValue {
    pub fn new(values: Vec<Value>) -> Result<Self, ArrayTypeError> {
        // Error is orthogonal to the normal element type. Infer an array's
        // element contract from a normal value even when an Error appears first.
        let element_type = values
            .iter()
            .filter(|value| !matches!(value, Value::Error(_)))
            .map(RuntimeType::of)
            .find(|ty| !has_undetermined_array_type(ty))
            .or_else(|| {
                values
                    .iter()
                    .map(RuntimeType::of)
                    .find(|ty| !has_undetermined_array_type(ty))
            });
        if !values.is_empty() && element_type.is_none() {
            return Err(ArrayTypeError);
        }
        if let Some(expected) = &element_type {
            for value in &values {
                validate_value_type(expected, value)?;
            }
            for value in &values {
                apply_value_type(expected, value)?;
            }
        }
        Ok(Self {
            values: RefCell::new(values),
            element_type: RefCell::new(element_type),
            tuple_types: RefCell::new(None),
            dynamic: Cell::new(false),
            structure_version: Cell::new(0),
        })
    }

    /// Build a mutable heterogeneous array for decoded dynamic data formats.
    pub fn dynamic(values: Vec<Value>) -> Self {
        Self {
            values: RefCell::new(values),
            element_type: RefCell::new(None),
            tuple_types: RefCell::new(None),
            dynamic: Cell::new(true),
            structure_version: Cell::new(0),
        }
    }

    pub fn tuple(values: Vec<Value>) -> Self {
        Self::try_tuple(values).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn try_tuple(values: Vec<Value>) -> Result<Self, RuntimeError> {
        let mut tuple_types = Vec::new();
        tuple_types
            .try_reserve_exact(values.len())
            .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
        tuple_types.extend(values.iter().map(RuntimeType::of));
        Ok(Self {
            values: RefCell::new(values),
            element_type: RefCell::new(None),
            tuple_types: RefCell::new(Some(tuple_types)),
            dynamic: Cell::new(false),
            structure_version: Cell::new(0),
        })
    }

    pub fn typed(values: Vec<Value>, expected: RuntimeType) -> Result<Self, ArrayTypeError> {
        for value in &values {
            validate_value_type(&expected, value)?;
        }
        for value in &values {
            apply_value_type(&expected, value)?;
        }
        Ok(Self {
            values: RefCell::new(values),
            element_type: RefCell::new(Some(expected)),
            tuple_types: RefCell::new(None),
            dynamic: Cell::new(false),
            structure_version: Cell::new(0),
        })
    }

    pub fn runtime_type(&self) -> RuntimeType {
        if let Some(types) = self.tuple_types.borrow().as_ref() {
            RuntimeType::Tuple(types.clone())
        } else {
            RuntimeType::Array(Box::new(
                self.element_type
                    .borrow()
                    .clone()
                    .unwrap_or(RuntimeType::Other),
            ))
        }
    }

    /// Build an independent container with this container's fixed array or
    /// tuple contract, validating copied values before applying constraints.
    pub fn with_values_like(&self, values: Vec<Value>) -> Result<Self, ArrayTypeError> {
        self.try_with_values_like(values)
            .map_err(|_| ArrayTypeError)
    }

    pub(crate) fn try_with_values_like(&self, values: Vec<Value>) -> Result<Self, RuntimeError> {
        if self.dynamic.get() {
            return Ok(Self::dynamic(values));
        }

        let tuple_types = self
            .tuple_types
            .try_borrow()
            .map_err(|_| RuntimeError::InvalidArgument { op: "clone" })?;
        if let Some(tuple_types) = tuple_types.as_ref() {
            let copy = Self::try_tuple(values)?;
            copy.constrain_tuple(tuple_types)
                .map_err(|_| RuntimeError::TypeMismatch { op: "clone" })?;
            return Ok(copy);
        }
        drop(tuple_types);

        let element_type = self
            .element_type
            .try_borrow()
            .map_err(|_| RuntimeError::InvalidArgument { op: "clone" })?
            .clone();
        if let Some(element_type) = element_type {
            Self::typed(values, element_type)
                .map_err(|_| RuntimeError::TypeMismatch { op: "clone" })
        } else {
            Self::new(values).map_err(|_| RuntimeError::TypeMismatch { op: "clone" })
        }
    }

    pub fn element_type(&self) -> Option<RuntimeType> {
        self.element_type.borrow().clone()
    }

    /// Sorts a primitive array in Rust when its runtime element type matches
    /// one of Cog's built-in Ord types. `None` requests the source fallback.
    pub(crate) fn try_sort_primitive(&self, stable: bool) -> Result<Option<Value>, RuntimeError> {
        let expected = self.element_type();
        let Some(expected) = expected else {
            return if self
                .values
                .try_borrow()
                .map_err(|_| RuntimeError::InvalidArgument { op: "sort" })?
                .is_empty()
            {
                Ok(Some(Value::Bool(true)))
            } else {
                Ok(None)
            };
        };
        let supported = matches!(
            &expected,
            RuntimeType::Int | RuntimeType::Float | RuntimeType::Bool | RuntimeType::Str
        );
        if !supported {
            return Ok(None);
        }

        let mut values = self
            .values
            .try_borrow_mut()
            .map_err(|_| RuntimeError::InvalidArgument { op: "sort" })?;
        for value in values.iter() {
            if matches!(value, Value::Error(_)) {
                return Ok(Some(value.clone()));
            }
            let matches_type = matches!(
                (&expected, value),
                (RuntimeType::Int, Value::Int(_))
                    | (RuntimeType::Float, Value::Float(_))
                    | (RuntimeType::Bool, Value::Bool(_))
                    | (RuntimeType::Str, Value::Str(_))
            );
            if !matches_type {
                return Err(RuntimeError::TypeMismatch { op: "sort" });
            }
            if matches!((&expected, value), (RuntimeType::Float, Value::Float(value)) if value.is_nan())
            {
                return Ok(Some(Value::Error(Box::new(RuntimeError::Unordered))));
            }
        }

        let compare = |left: &Value, right: &Value| match (left, right) {
            (Value::Int(left), Value::Int(right)) => left.cmp(right),
            (Value::Float(left), Value::Float(right)) => {
                left.partial_cmp(right).unwrap_or(Ordering::Equal)
            }
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            (Value::Str(left), Value::Str(right)) => left.cmp(right),
            _ => Ordering::Equal,
        };
        if stable {
            values.sort_by(compare);
        } else {
            values.sort_unstable_by(compare);
        }
        Ok(Some(Value::Bool(true)))
    }

    pub fn needs_type_context(&self) -> bool {
        has_undetermined_array_type(&self.runtime_type())
    }

    pub(crate) fn structure_version(&self) -> u64 {
        self.structure_version.get()
    }

    pub fn constrain_from(&self, template: &Self) -> Result<(), ArrayTypeError> {
        if self.needs_type_context() {
            if self.tuple_types.borrow().is_some() {
                let expected = template
                    .tuple_types
                    .borrow()
                    .clone()
                    .ok_or(ArrayTypeError)?;
                return self.constrain_tuple(&expected);
            }
            let expected = template.element_type().ok_or(ArrayTypeError)?;
            self.constrain(expected)?;
        }
        Ok(())
    }

    /// Insert only if the value agrees with the object's established type.
    /// A previously empty array acquires its invariant from its first value.
    pub fn push(&self, value: Value, front: bool) -> Result<(), ArrayTypeError> {
        self.try_push(value, front).map_err(|_| ArrayTypeError)
    }

    /// Runtime mutation variant that preserves the distinction between an
    /// invalid value, a conflicting embedder borrow, and allocation failure.
    pub fn try_push(&self, value: Value, front: bool) -> Result<(), RuntimeError> {
        let op = if front { "arrprepend" } else { "arrpush" };
        let next_version =
            self.structure_version
                .get()
                .checked_add(1)
                .ok_or(RuntimeError::ResourceLimit {
                    resource: "array structure version",
                })?;
        if self.tuple_types.borrow().is_some() {
            return Err(RuntimeError::TypeMismatch { op });
        }
        let actual = RuntimeType::of(&value);
        let expected = self.element_type.borrow().clone();
        if self.dynamic.get() {
            // Dynamic arrays intentionally accept heterogeneous values.
        } else if let Some(expected) = expected.as_ref() {
            validate_value_type(expected, &value).map_err(|_| RuntimeError::TypeMismatch { op })?;
        } else {
            if has_undetermined_array_type(&actual) {
                return Err(RuntimeError::TypeMismatch { op });
            }
        }
        let mut values = self
            .values
            .try_borrow_mut()
            .map_err(|_| RuntimeError::InvalidArgument { op })?;
        values
            .try_reserve(1)
            .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
        if !self.dynamic.get()
            && let Some(expected) = expected.as_ref()
        {
            apply_value_type(expected, &value).map_err(|_| RuntimeError::TypeMismatch { op })?;
        }
        if expected.is_none() && !self.dynamic.get() {
            *self.element_type.borrow_mut() = Some(actual);
        }
        if front {
            values.insert(0, value);
        } else {
            values.push(value);
        }
        self.structure_version.set(next_version);
        Ok(())
    }

    pub fn set(&self, index: usize, value: Value) -> Result<(), ArrayTypeError> {
        if index >= self.values.try_borrow().map_err(|_| ArrayTypeError)?.len() {
            return Err(ArrayTypeError);
        }
        let actual = RuntimeType::of(&value);
        if self.dynamic.get() {
            let mut values = self.values.try_borrow_mut().map_err(|_| ArrayTypeError)?;
            values[index] = value;
            return Ok(());
        }
        if self.tuple_types.borrow().is_some() {
            let expected = {
                let tuple_types = self.tuple_types.borrow();
                tuple_types
                    .as_ref()
                    .and_then(|types| types.get(index))
                    .cloned()
                    .ok_or(ArrayTypeError)?
            };
            validate_value_type(&expected, &value)?;
            let mut values = self.values.try_borrow_mut().map_err(|_| ArrayTypeError)?;
            apply_value_type(&expected, &value)?;
            let actual = RuntimeType::of(&value);
            values[index] = value;
            drop(values);
            self.tuple_types.borrow_mut().as_mut().unwrap()[index] = actual;
            return Ok(());
        }
        let expected = self.element_type.borrow().clone();
        if let Some(expected) = expected.as_ref() {
            validate_value_type(expected, &value)?;
        } else {
            if has_undetermined_array_type(&actual) {
                return Err(ArrayTypeError);
            }
        }
        let mut values = self.values.try_borrow_mut().map_err(|_| ArrayTypeError)?;
        if let Some(expected) = expected.as_ref() {
            apply_value_type(expected, &value)?;
        } else {
            *self.element_type.borrow_mut() = Some(actual);
        }
        values[index] = value;
        Ok(())
    }

    pub fn append_all(&self, values: &[Value]) -> Result<(), ArrayTypeError> {
        self.try_append_all(values).map_err(|_| ArrayTypeError)
    }

    /// Runtime bulk mutation with checked borrowing and fallible capacity growth.
    pub fn try_append_all(&self, values: &[Value]) -> Result<(), RuntimeError> {
        if self.tuple_types.borrow().is_some() {
            return Err(RuntimeError::TypeMismatch {
                op: "array::append_all",
            });
        }
        let next_version = if values.is_empty() {
            self.structure_version.get()
        } else {
            self.structure_version
                .get()
                .checked_add(1)
                .ok_or(RuntimeError::ResourceLimit {
                    resource: "array structure version",
                })?
        };
        let current_type = self.element_type.borrow().clone();
        let inferred = current_type.or_else(|| {
            values
                .iter()
                .map(RuntimeType::of)
                .find(|ty| !has_undetermined_array_type(ty))
        });
        if !self.dynamic.get() && !values.is_empty() && inferred.is_none() {
            return Err(RuntimeError::TypeMismatch {
                op: "array::append_all",
            });
        }
        let mut items =
            self.values
                .try_borrow_mut()
                .map_err(|_| RuntimeError::InvalidArgument {
                    op: "array::append_all",
                })?;
        if !self.dynamic.get()
            && let Some(expected) = &inferred
        {
            for value in values {
                validate_value_type(expected, value).map_err(|_| RuntimeError::TypeMismatch {
                    op: "array::append_all",
                })?;
            }
        }
        items
            .try_reserve(values.len())
            .map_err(|_| RuntimeError::ResourceLimit { resource: "array" })?;
        if !self.dynamic.get()
            && let Some(expected) = &inferred
        {
            for value in values {
                apply_value_type(expected, value).map_err(|_| RuntimeError::TypeMismatch {
                    op: "array::append_all",
                })?;
            }
        }
        if self.element_type.borrow().is_none() && !self.dynamic.get() {
            *self.element_type.borrow_mut() = inferred;
        }
        items.extend(values.iter().cloned());
        self.structure_version.set(next_version);
        Ok(())
    }

    pub fn constrain(&self, expected: RuntimeType) -> Result<(), ArrayTypeError> {
        self.validate_constrain(&expected)?;
        self.apply_constrain(&expected)
    }

    fn validate_tuple(&self, expected: &[RuntimeType]) -> Result<(), ArrayTypeError> {
        let current = self.tuple_types.try_borrow().map_err(|_| ArrayTypeError)?;
        let current = current.as_ref().ok_or(ArrayTypeError)?;
        if current.len() != expected.len() {
            return Err(ArrayTypeError);
        }
        for (expected, value) in expected
            .iter()
            .zip(self.values.try_borrow().map_err(|_| ArrayTypeError)?.iter())
        {
            validate_value_type(expected, value)?;
        }
        Ok(())
    }

    fn constrain_tuple(&self, expected: &[RuntimeType]) -> Result<(), ArrayTypeError> {
        self.validate_tuple(expected)?;
        let values = self.values.try_borrow().map_err(|_| ArrayTypeError)?;
        for (expected, value) in expected.iter().zip(values.iter()) {
            apply_value_type(expected, value)?;
        }
        let mut tuple_types = self
            .tuple_types
            .try_borrow_mut()
            .map_err(|_| ArrayTypeError)?;
        let types = tuple_types.as_mut().ok_or(ArrayTypeError)?;
        for (ty, value) in types.iter_mut().zip(values.iter()) {
            *ty = RuntimeType::of(value);
        }
        drop(values);
        Ok(())
    }

    fn validate_constrain(&self, expected: &RuntimeType) -> Result<(), ArrayTypeError> {
        if self.tuple_types.borrow().is_some() {
            return Err(ArrayTypeError);
        }
        let current = self.element_type.borrow().clone();
        if let Some(actual) = current.as_ref()
            && actual != expected
        {
            match (expected, actual) {
                (RuntimeType::Array(expected_inner), RuntimeType::Array(actual_inner))
                    if **actual_inner == RuntimeType::Other =>
                {
                    for value in self.values.try_borrow().map_err(|_| ArrayTypeError)?.iter() {
                        validate_value_type(&RuntimeType::Array(expected_inner.clone()), value)?;
                    }
                }
                _ => return Err(ArrayTypeError),
            }
        }
        for value in self.values.try_borrow().map_err(|_| ArrayTypeError)?.iter() {
            validate_value_type(expected, value)?;
        }
        Ok(())
    }

    fn apply_constrain(&self, expected: &RuntimeType) -> Result<(), ArrayTypeError> {
        for value in self.values.try_borrow().map_err(|_| ArrayTypeError)?.iter() {
            apply_value_type(expected, value)?;
        }
        *self.element_type.borrow_mut() = Some(expected.clone());
        self.dynamic.set(false);
        Ok(())
    }
}

impl HeapObject for ArrayValue {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(values) = self.values.try_borrow() {
            for value in values.iter() {
                visit(value);
            }
        }
    }

    fn clear_values(&self) {
        if let Ok(mut values) = self.values.try_borrow_mut() {
            values.clear();
        }
    }
}

fn has_undetermined_array_type(ty: &RuntimeType) -> bool {
    match ty {
        RuntimeType::Array(element) => {
            **element == RuntimeType::Other || has_undetermined_array_type(element)
        }
        RuntimeType::Nullable(inner) => has_undetermined_array_type(inner),
        RuntimeType::Tuple(items) => items.iter().any(has_undetermined_array_type),
        _ => false,
    }
}

fn validate_value_type(expected: &RuntimeType, value: &Value) -> Result<(), ArrayTypeError> {
    match (expected, value) {
        (_, Value::Error(_)) => Ok(()),
        (RuntimeType::Nullable(_), Value::Nil) => Ok(()),
        (RuntimeType::Nullable(inner), value) => validate_value_type(inner, value),
        (RuntimeType::Array(inner), Value::Array(array)) => array.validate_constrain(inner),
        (RuntimeType::Tuple(items), Value::Array(array)) => array.validate_tuple(items),
        _ if expected.accepts(&RuntimeType::of(value)) => Ok(()),
        _ => Err(ArrayTypeError),
    }
}

fn apply_value_type(expected: &RuntimeType, value: &Value) -> Result<(), ArrayTypeError> {
    match (expected, value) {
        (_, Value::Error(_)) => Ok(()),
        (RuntimeType::Nullable(_), Value::Nil) => Ok(()),
        (RuntimeType::Nullable(inner), value) => apply_value_type(inner, value),
        (RuntimeType::Array(inner), Value::Array(array)) => array.constrain((**inner).clone()),
        (RuntimeType::Tuple(items), Value::Array(array)) => array.constrain_tuple(items),
        _ if expected.accepts(&RuntimeType::of(value)) => Ok(()),
        _ => Err(ArrayTypeError),
    }
}

impl std::ops::Deref for ArrayValue {
    type Target = RefCell<Vec<Value>>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

#[derive(Debug)]
pub struct RecordValue {
    fields: RefCell<HashMap<String, Value>>,
    nominal: Option<String>,
}

impl RecordValue {
    pub fn new(fields: HashMap<String, Value>) -> Self {
        Self {
            fields: RefCell::new(fields),
            nominal: None,
        }
    }
    pub fn nominal(fields: HashMap<String, Value>, name: String) -> Self {
        Self {
            fields: RefCell::new(fields),
            nominal: Some(name),
        }
    }
    pub fn runtime_type(&self) -> RuntimeType {
        self.nominal
            .clone()
            .map_or(RuntimeType::Record, RuntimeType::Nominal)
    }

    pub fn is_nominal(&self) -> bool {
        self.nominal.is_some()
    }

    pub(crate) fn try_borrow_fields(
        &self,
    ) -> Result<std::cell::Ref<'_, HashMap<String, Value>>, std::cell::BorrowError> {
        self.fields.try_borrow()
    }
}

/// Immutable named values retained by an enum variant or language Error.
/// Interior mutation is private to the collector, which clears unreachable
/// objects before releasing their graph edges.
pub struct ValueFields {
    fields: RefCell<HashMap<String, Value>>,
}

impl ValueFields {
    pub fn new(fields: HashMap<String, Value>) -> Self {
        Self {
            fields: RefCell::new(fields),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<Value> {
        self.fields.try_borrow().ok()?.get(name).cloned()
    }

    pub(crate) fn try_borrow(
        &self,
    ) -> Result<std::cell::Ref<'_, HashMap<String, Value>>, std::cell::BorrowError> {
        self.fields.try_borrow()
    }
}

impl HeapObject for ValueFields {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(fields) = self.fields.try_borrow() {
            for value in fields.values() {
                visit(value);
            }
        }
    }

    fn clear_values(&self) {
        if let Ok(mut fields) = self.fields.try_borrow_mut() {
            fields.clear();
        }
    }
}

/// Immutable closure captures. The collector owns the only mutation path.
pub struct ClosureCaptures {
    values: RefCell<Vec<Value>>,
}

impl ClosureCaptures {
    pub fn new(values: Vec<Value>) -> Self {
        Self {
            values: RefCell::new(values),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.values.borrow().len()
    }

    pub(crate) fn get(&self, index: usize) -> Option<Value> {
        self.values.try_borrow().ok()?.get(index).cloned()
    }
}

impl HeapObject for ClosureCaptures {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(values) = self.values.try_borrow() {
            for value in values.iter() {
                visit(value);
            }
        }
    }

    fn clear_values(&self) {
        if let Ok(mut values) = self.values.try_borrow_mut() {
            values.clear();
        }
    }
}

impl HeapObject for RecordValue {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(fields) = self.fields.try_borrow() {
            for value in fields.values() {
                visit(value);
            }
        }
    }

    fn clear_values(&self) {
        if let Ok(mut fields) = self.fields.try_borrow_mut() {
            fields.clear();
        }
    }
}

impl HeapObject for RefCell<Value> {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(value) = self.try_borrow() {
            visit(&value);
        }
    }

    fn clear_values(&self) {
        if let Ok(mut value) = self.try_borrow_mut() {
            *value = Value::Nil;
        }
    }
}

impl HeapObject for RefCell<IteratorValue> {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(iterator) = self.try_borrow() {
            visit(&iterator.source);
            if let Some(value) = &iterator.current {
                visit(value);
            }
        }
    }

    fn clear_values(&self) {
        if let Ok(mut iterator) = self.try_borrow_mut() {
            iterator.source = Value::Nil;
            iterator.current = None;
        }
    }
}

impl HeapObject for RefCell<Vec<u8>> {
    fn visit_values(&self, _: &mut dyn FnMut(&Value)) {}

    fn clear_values(&self) {
        if let Ok(mut bytes) = self.try_borrow_mut() {
            bytes.clear();
        }
    }
}

impl HeapObject for RefCell<String> {
    fn visit_values(&self, _: &mut dyn FnMut(&Value)) {}

    fn clear_values(&self) {
        if let Ok(mut text) = self.try_borrow_mut() {
            text.clear();
        }
    }
}

impl std::ops::Deref for RecordValue {
    type Target = RefCell<HashMap<String, Value>>;
    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl RuntimeType {
    pub fn accepts(&self, actual: &Self) -> bool {
        match self {
            Self::Nullable(inner) => actual == &Self::Nil || inner.accepts(actual),
            Self::Nominal(name) => {
                actual == self
                    || matches!(actual, Self::Variant(parent) if parent == name)
                    || (name == crate::error::ERROR_TYPE && matches!(actual, Self::Error(_)))
            }
            _ => self == actual,
        }
    }

    pub fn of(value: &Value) -> Self {
        match value {
            Value::Int(_) => Self::Int,
            Value::Float(_) => Self::Float,
            Value::Bool(_) => Self::Bool,
            Value::Nil => Self::Nil,
            Value::Str(_) | Value::StrBuf(_) => Self::Str,
            Value::Error(_) => Self::Error("error".to_owned()),
            Value::Array(array) => array.runtime_type(),
            Value::Record(value) => value.runtime_type(),
            Value::Variant(tag, _) => Self::Variant(
                tag.rsplit_once("::")
                    .map_or(tag.as_ref(), |(parent, _)| parent)
                    .to_owned(),
            ),
            Value::TypeTag(tag) => Self::Variant(tag.to_string()),
            Value::Function(_) | Value::Closure(_, _) => Self::Function,
            Value::Bytes(_) | Value::Handle(_) | Value::Cell(_) | Value::Iterator(_) => Self::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Error(Box<RuntimeError>),
    Str(Rc<str>),
    StrBuf(HeapRef<RefCell<String>>),
    /// Fixed-length mutable byte storage. Every alias retains byte/bounds checks.
    Bytes(HeapRef<RefCell<Vec<u8>>>),
    Handle(crate::runtime::ResourceHandle),
    Function(usize),
    Array(HeapRef<ArrayValue>),
    Record(HeapRef<RecordValue>),
    /// Immutable nominal tag and payload slots. Nested reference values retain
    /// their own mutability; source-level field writes cannot change the tag.
    Variant(Rc<str>, HeapRef<ValueFields>),
    /// Compiler-generated type metadata for the generic record allocator.
    TypeTag(Rc<str>),
    Cell(HeapRef<RefCell<Value>>),
    Closure(usize, HeapRef<ClosureCaptures>),
    Iterator(HeapRef<RefCell<IteratorValue>>),
}

impl Value {
    pub(crate) fn visit_heap_objects(
        &self,
        visit: &mut dyn FnMut(ObjectKey),
    ) -> Result<(), RuntimeError> {
        let mut values = Vec::new();
        values
            .try_reserve(1)
            .map_err(|_| RuntimeError::ResourceLimit {
                resource: "VM heap",
            })?;
        values.push(self);
        let mut errors = Vec::new();
        loop {
            while let Some(value) = values.pop() {
                match value {
                    Self::Array(value) => visit(value.key()),
                    Self::Record(value) => visit(value.key()),
                    Self::StrBuf(value) => visit(value.key()),
                    Self::Bytes(value) => visit(value.key()),
                    Self::Variant(_, value) => visit(value.key()),
                    Self::Cell(value) => visit(value.key()),
                    Self::Closure(_, value) => visit(value.key()),
                    Self::Iterator(value) => visit(value.key()),
                    Self::Error(error) => {
                        errors
                            .try_reserve(1)
                            .map_err(|_| RuntimeError::ResourceLimit {
                                resource: "VM heap",
                            })?;
                        errors.push(error.as_ref());
                    }
                    _ => {}
                }
            }
            let Some(error) = errors.pop() else {
                break;
            };
            match error {
                RuntimeError::LanguageError { fields, .. } => {
                    visit(fields.key());
                }
                RuntimeError::Crashed { value, .. } => {
                    values
                        .try_reserve(1)
                        .map_err(|_| RuntimeError::ResourceLimit {
                            resource: "VM heap",
                        })?;
                    values.push(value);
                }
                RuntimeError::WithSuppressed {
                    primary,
                    suppressed,
                } => {
                    let additional =
                        suppressed
                            .len()
                            .checked_add(1)
                            .ok_or(RuntimeError::ResourceLimit {
                                resource: "VM heap",
                            })?;
                    errors
                        .try_reserve(additional)
                        .map_err(|_| RuntimeError::ResourceLimit {
                            resource: "VM heap",
                        })?;
                    errors.push(primary);
                    errors.extend(suppressed.iter());
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(x) => write!(f, "{x}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Bool(x) => write!(f, "{x}"),
            Value::Nil => write!(f, "nil"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Handle(_) => write!(f, "<resource>"),
            Value::Function(addr) => write!(f, "<function@{addr}>"),
            Value::Array(_)
            | Value::Record(_)
            | Value::Variant(_, _)
            | Value::Cell(_)
            | Value::Iterator(_)
            | Value::StrBuf(_)
            | Value::Bytes(_)
            | Value::Error(_) => {
                // Diagnostics must not recurse without bounds or panic while
                // inspecting a borrowed graph. Language conversions use the
                // same renderer with their explicit host buffer policy.
                match crate::runtime::text::render(self, 64 * 1024, "diagnostic") {
                    Ok(text) => f.write_str(&text),
                    Err(error) => write!(f, "<unrenderable value: {}>", error.kind()),
                }
            }
            Value::TypeTag(tag) => write!(f, "<type:{tag}>"),
            Value::Closure(addr, captures) => {
                write!(f, "<closure@{addr}/{}>", captures.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ArrayValue, RuntimeType, Value};
    use crate::heap::HeapRef;

    #[test]
    fn tuple_set_refreshes_the_type_after_constraining_an_empty_array() {
        let original =
            HeapRef::try_new(ArrayValue::typed(vec![Value::Int(1)], RuntimeType::Int).unwrap())
                .unwrap();
        let tuple = ArrayValue::try_tuple(vec![Value::Array(original)]).unwrap();
        let replacement = HeapRef::try_new(ArrayValue::new(Vec::new()).unwrap()).unwrap();
        tuple.set(0, Value::Array(replacement.clone())).unwrap();
        assert_eq!(
            replacement.runtime_type(),
            RuntimeType::Array(Box::new(RuntimeType::Int))
        );
        assert_eq!(
            tuple.runtime_type(),
            RuntimeType::Tuple(vec![RuntimeType::Array(Box::new(RuntimeType::Int))])
        );
        assert!(tuple.set(0, Value::Bool(false)).is_err());
        assert_eq!(
            tuple.runtime_type(),
            RuntimeType::Tuple(vec![RuntimeType::Array(Box::new(RuntimeType::Int))])
        );
        assert!(replacement.try_borrow().unwrap().is_empty());
    }

    #[test]
    fn tuple_constraint_refreshes_nested_array_type() {
        let untyped_inner = HeapRef::try_new(ArrayValue::new(Vec::new()).unwrap()).unwrap();
        let source = ArrayValue::try_tuple(vec![Value::Array(untyped_inner)]).unwrap();

        let typed_inner =
            HeapRef::try_new(ArrayValue::typed(Vec::new(), RuntimeType::Int).unwrap()).unwrap();
        let template = ArrayValue::try_tuple(vec![Value::Array(typed_inner)]).unwrap();

        source.constrain_from(&template).unwrap();
        assert_eq!(
            source.runtime_type(),
            RuntimeType::Tuple(vec![RuntimeType::Array(Box::new(RuntimeType::Int))])
        );
    }

    #[test]
    fn fallible_tuple_copy_preserves_the_nested_array_contract() {
        let original_inner =
            HeapRef::try_new(ArrayValue::typed(vec![Value::Int(1)], RuntimeType::Int).unwrap())
                .unwrap();
        let original = ArrayValue::try_tuple(vec![Value::Array(original_inner)]).unwrap();

        let copied_inner = HeapRef::try_new(ArrayValue::new(vec![Value::Int(2)]).unwrap()).unwrap();
        let copy = original
            .try_with_values_like(vec![Value::Array(copied_inner)])
            .unwrap();

        assert_eq!(copy.runtime_type(), original.runtime_type());
    }
}
