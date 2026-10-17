use crate::template::ArrayTemplate;
use crate::value::{RuntimeType, Value};
use std::rc::Rc;

/// Runtime value classification. Nominal enum identities are resolved by the
/// compiler; predicates never cast, narrow a binding, or propagate an Error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Int,
    Float,
    Bool,
    Str,
    Nil,
    Error,
    Enum(Rc<str>),
    Variant(Rc<str>),
}

impl ValueType {
    pub(crate) fn primitive(name: &str) -> Option<Self> {
        Some(match name {
            "int" => Self::Int,
            "float" => Self::Float,
            "bool" => Self::Bool,
            "str" => Self::Str,
            "nil" => Self::Nil,
            _ => return None,
        })
    }

    pub(crate) fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Int, Value::Int(_))
            | (Self::Float, Value::Float(_))
            | (Self::Bool, Value::Bool(_))
            | (Self::Str, Value::Str(_))
            | (Self::Nil, Value::Nil)
            | (Self::Error, Value::Error(_)) => true,
            (Self::Enum(name), Value::Variant(tag, _)) => tag
                .rsplit_once("::")
                .is_some_and(|(parent, _)| parent == name.as_ref()),
            (Self::Variant(name), Value::Variant(tag, _)) => name == tag,
            (Self::Variant(name), Value::Error(error)) => match error.primary() {
                crate::error::RuntimeError::LanguageError { tag, .. } => name == tag,
                error => {
                    name.strip_prefix(crate::error::ERROR_TYPE)
                        .and_then(|tail| tail.strip_prefix("::"))
                        == Some(error.kind())
                }
            },
            _ => false,
        }
    }
}

impl std::fmt::Display for ValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enum(name) => write!(f, "enum {name}"),
            Self::Variant(name) => write!(f, "variant {name}"),
            Self::Int => f.write_str("int"),
            Self::Float => f.write_str("float"),
            Self::Bool => f.write_str("bool"),
            Self::Str => f.write_str("str"),
            Self::Nil => f.write_str("nil"),
            Self::Error => f.write_str("error"),
        }
    }
}
#[derive(Debug)]
pub enum Instruction {
    /// Index into the program's checked and linked native import table.
    CallNative(usize),
    Add,
    Mul,
    Div,
    Mod,
    Sub,
    Push(Value),
    PushArray(ArrayTemplate),
    Load(usize),
    Store(usize),
    LoadGlobal(usize),
    StoreGlobal(usize),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    Halt,
    Call(usize, usize), // target, argument count
    Return,
    IsType(ValueType),
    ErrorKind,
    And,
    Or,
    Xor,
    Not,
    Len,
    Exclusive,
    Dup,
    Pop,
    Swap,
    /// Restore the operand stack to a depth relative to the current call frame.
    StackTruncate(usize),
    CallValue(usize),
    Index,
    IndexSet,
    MakeCell,
    CellGet,
    CellSet,
    MakeRecord(usize),
    MakeArray(usize),
    MakeTuple(usize),
    MakeTypedArray(usize, RuntimeType),
    ConstrainArrayType(RuntimeType),
    ConstrainArrayFrom,
    ArrayEmptyLike,
    MakeClosure(usize, usize),
    LoadCapture(usize),
    Crash(Option<Rc<str>>),
    // `<<` — mutate the array in place (push to the end), unlike Add/Sub/etc this
    // does NOT allocate a new array: it must preserve Rc<RefCell<..>> aliasing
    // (see docs/LANGUAGE.md on shared-mutation semantics). Returns the same array
    // value so `arr << x` can be chained/used as an expression.
    ArrayPush,
    // `>>` — mutate the array in place (insert at the front). Same aliasing
    // requirement as ArrayPush.
    ArrayPrepend,
}

impl Instruction {
    /// Required operands and resulting values, relative to the active frame.
    /// StackTruncate sets an absolute frame-relative height instead of applying
    /// the ordinary pop/push rule. Return and Crash have no normal successor.
    pub(crate) fn stack_effect(
        &self,
        imports: &[crate::runtime::NativeImport],
    ) -> Result<(usize, usize), crate::error::RuntimeError> {
        use crate::error::RuntimeError;
        Ok(match self {
            Self::Push(_)
            | Self::PushArray(_)
            | Self::Load(_)
            | Self::LoadGlobal(_)
            | Self::LoadCapture(_) => (0, 1),
            Self::Store(_)
            | Self::StoreGlobal(_)
            | Self::Pop
            | Self::JumpIfFalse(_)
            | Self::JumpIfTrue(_)
            | Self::Return
            | Self::Crash(_) => (1, 0),
            Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Mod
            | Self::Eq
            | Self::Ne
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::And
            | Self::Or
            | Self::Xor
            | Self::Index
            | Self::CellSet
            | Self::ArrayPush
            | Self::ArrayPrepend
            | Self::ConstrainArrayFrom => (2, 1),
            Self::IndexSet => (3, 1),
            Self::IsType(_)
            | Self::ErrorKind
            | Self::Not
            | Self::Len
            | Self::Exclusive
            | Self::MakeCell
            | Self::CellGet
            | Self::ConstrainArrayType(_)
            | Self::ArrayEmptyLike => (1, 1),
            Self::Dup => (1, 2),
            Self::Swap => (2, 2),
            Self::Call(_, n)
            | Self::MakeArray(n)
            | Self::MakeTuple(n)
            | Self::MakeTypedArray(n, _)
            | Self::MakeClosure(_, n) => (*n, 1),
            Self::CallValue(n) => (n.checked_add(1).ok_or(RuntimeError::StackUnderflow)?, 1),
            Self::MakeRecord(n) => (n.checked_mul(2).ok_or(RuntimeError::StackUnderflow)?, 1),
            Self::CallNative(index) => (
                imports
                    .get(*index)
                    .ok_or(RuntimeError::InvalidNativeCall)?
                    .parameters()
                    .len(),
                1,
            ),
            Self::StackTruncate(depth) => (*depth, 0),
            Self::Jump(_) | Self::Halt => (0, 0),
        })
    }
}
