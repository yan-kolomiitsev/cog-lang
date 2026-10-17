#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UseDecl {
    pub span: Option<crate::diagnostic::SourceSpan>,
    pub module: String,
    /// Optional package alias selected by `use module from package`.
    pub from: Option<String>,
    pub names: Vec<String>,
    // Parallel aliases for explicitly imported members or namespaces.
    pub aliases: Vec<Option<String>>,
    /// `true` for `use module;` / `use module as alias;`.
    pub namespace: bool,
    /// `true` for `use module::*;`.
    pub wildcard: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Invalid {
        binding: Option<String>,
    },
    Function(FunctionDecl),
    GlobalConst {
        name: String,
        visibility: Visibility,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    /// A compile-time value binding. Evaluated and materialized before runtime lowering.
    Comptime {
        name: String,
        visibility: Visibility,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    GlobalVar {
        name: String,
        visibility: Visibility,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    Use(UseDecl),
    /// A compile-time package inclusion. The project loader consumes this
    /// before module resolution; it is never emitted into runtime code.
    ComptimeInclude {
        package: String,
        path: Option<String>,
        span: Option<crate::diagnostic::SourceSpan>,
    },
    Extend(ExtendDecl),
    Trait(TraitDecl),
    StructDecl {
        name: String,
        visibility: Visibility,
        type_params: Vec<TypeParameter>,
        fields: Vec<StructField>,
        /// Resolver-only package/module identity used to enforce private fields.
        owner: Option<String>,
    },
    EnumDecl {
        name: String,
        visibility: Visibility,
        variants: Vec<EnumVariant>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtendDecl {
    /// Generic parameters declared before the target, for example
    /// `extend[K: type] Box[K] { ... }`.
    pub type_params: Vec<TypeParameter>,
    pub target: TypeAnnotation,
    /// Present for a trait implementation (`extend Type with Trait`).
    pub trait_name: Option<String>,
    /// Type arguments supplied to the implemented trait.
    pub trait_arguments: Vec<TypeAnnotation>,
    /// True for `comptime extend`. Extension declarations are resolved before
    /// runtime lowering; this distinguishes their source role for diagnostics.
    pub comptime: bool,
    pub methods: Vec<ExtensionMethod>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeParameter {
    pub name: String,
    pub bounds: Vec<TraitBound>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraitBound {
    pub name: String,
    pub arguments: Vec<TypeAnnotation>,
}

impl TraitBound {
    pub fn display(&self) -> String {
        if self.arguments.is_empty() {
            self.name.clone()
        } else {
            format!(
                "{}[{}]",
                self.name,
                self.arguments
                    .iter()
                    .map(TypeAnnotation::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraitDecl {
    pub name: String,
    pub visibility: Visibility,
    pub type_params: Vec<String>,
    pub methods: Vec<TraitMethod>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraitMethod {
    pub name: String,
    pub params: Vec<Param>,
    pub mutating_receiver: bool,
    pub result_type: Option<TypeAnnotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionMethod {
    pub name: String,
    pub mutating_receiver: bool,
    pub function: FunctionDecl,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<StructField>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MatchPattern {
    Wildcard,
    StringLiteral(String),
    Type(String),
    Variant { name: String, bindings: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub guard: Option<Expr>,
    pub body: Block,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDecl {
    pub name: String,
    /// Resolver-only package/module identity used to enforce private fields.
    pub owner: Option<String>,
    pub visibility: Visibility,
    /// Compile-time-only callable; it is evaluated by `comptime` and is not emitted at runtime.
    pub comptime: bool,
    pub type_params: Vec<TypeParameter>,
    pub params: Vec<Param>,
    pub result_type: Option<TypeAnnotation>,
    pub result_span: Option<crate::diagnostic::SourceSpan>,
    pub body: Block,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The final expression without `;`. Statements always discard their result.
    pub tail: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VarType {
    Int,
    Bool,
    Str,
    Float,
    Nil,
}

impl VarType {
    pub fn name(self) -> &'static str {
        match self {
            VarType::Int => "int",
            VarType::Bool => "bool",
            VarType::Str => "str",
            VarType::Float => "float",
            VarType::Nil => "nil",
        }
    }
}

// Nullability is part of the declared successful-value type. `T?` cannot
// flow to `T` implicitly, even when its current value happens to be non-nil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAnnotation {
    pub kind: TypeKind,
    pub nilable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    Primitive(VarType),
    Named(String),
    Applied {
        source: String,
        arguments: Vec<TypeAnnotation>,
    },
    ResolvedName {
        source: String,
        symbol: String,
    },
    ResolvedApplied {
        source: String,
        symbol: String,
        arguments: Vec<TypeAnnotation>,
    },
    TypeParameter(String),
    SelfType,
    Array(Box<TypeAnnotation>),
    Tuple(Vec<TypeAnnotation>),
}

impl TypeAnnotation {
    /// Stable internal identity used for monomorphized nominal types.
    pub fn canonical(&self) -> String {
        let base = match &self.kind {
            TypeKind::Primitive(base) => base.name().to_string(),
            TypeKind::Named(name) | TypeKind::TypeParameter(name) => name.clone(),
            TypeKind::Applied { source, arguments } => format!(
                "{}[{}]",
                source,
                arguments
                    .iter()
                    .map(TypeAnnotation::canonical)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            TypeKind::ResolvedName { symbol, .. } => symbol.clone(),
            TypeKind::ResolvedApplied {
                symbol, arguments, ..
            } => format!(
                "{}[{}]",
                symbol,
                arguments
                    .iter()
                    .map(TypeAnnotation::canonical)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            TypeKind::SelfType => "Self".to_string(),
            TypeKind::Array(element) => format!("{}[]", element.canonical()),
            TypeKind::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(TypeAnnotation::canonical)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        };
        if self.nilable {
            format!("{base}?")
        } else {
            base
        }
    }

    // For error messages: "int" or "int?".
    pub fn display(&self) -> String {
        let base = match &self.kind {
            TypeKind::Primitive(base) => base.name().to_string(),
            TypeKind::Named(name) => name.clone(),
            TypeKind::ResolvedName { source, .. } => source.clone(),
            TypeKind::Applied { source, arguments }
            | TypeKind::ResolvedApplied {
                source, arguments, ..
            } => format!(
                "{}[{}]",
                source,
                arguments
                    .iter()
                    .map(TypeAnnotation::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TypeKind::TypeParameter(name) => name.clone(),
            TypeKind::SelfType => "Self".to_string(),
            TypeKind::Array(element) => format!("{}[]", element.display()),
            TypeKind::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(|t| t.display())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        if self.nilable {
            format!("{base}?")
        } else {
            base
        }
    }
}

// A single `struct` field: `name`, `name: Type`, `name: Type?`, or any of
// those with `= default_expr` appended. `ty: None` means dynamic (any
// value, including Nil, already legal — same as an unannotated `var`).
// `default` is only usable on trailing fields — see
// `hlparser::parse_struct_decl`, which rejects a non-defaulted field after
// a defaulted one at parse time, same as every other language with
// optional trailing parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct StructField {
    pub name: String,
    pub ty: Option<TypeAnnotation>,
    pub default: Option<Expr>,
    pub visibility: Visibility,
}

// A function parameter is shape-identical to a struct field: a name, an
// optional `Type`/`Type?` annotation, an optional `= default`. Reusing
// `StructField` rather than a near-duplicate `Param` struct means
// shared argument binding/type checking and the "defaults must trail"
// parse-time rule all work on both without a second copy of each to
// keep in sync. The alias exists purely so `FunctionDecl.params`/call
// sites read as parameters, not "struct fields", at the point of use.
pub type Param = StructField;

/// A supplied argument in source evaluation order. Names select parameters;
/// they do not change the order in which expressions are evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct CallArgument {
    pub name: Option<String>,
    pub value: Expr,
}

impl CallArgument {
    pub fn positional(value: Expr) -> Self {
        Self { name: None, value }
    }

    pub fn named(name: impl Into<String>, value: Expr) -> Self {
        Self {
            name: Some(name.into()),
            value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Located(crate::diagnostic::SourceSpan, Box<Stmt>),
    /// Recovery placeholder; a failed declaration poisons its binding.
    Invalid {
        binding: Option<String>,
    },
    // Binding types are inferred or annotated by the semantic pass,
    // independently of instruction emission and mutation capability.
    VarDecl {
        name: String,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    Const {
        name: String,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    /// A compile-time value binding local to the current lexical block.
    Comptime {
        name: String,
        ty: Option<TypeAnnotation>,
        value: Expr,
    },
    Assign {
        target: Expr,
        value: Expr,
    },
    CompoundAssign {
        target: Expr,
        op: BinOp,
        value: Expr,
    },
    Return(Option<Expr>),
    Defer {
        body: Block,
    },
    Try {
        body: Block,
        catch_name: String,
        catch_block: Block,
    },
    Use {
        module: String,
        from: Option<String>,
        names: Vec<String>,
        // See `UseDecl::aliases` — same shape, same reason for being separate.
        aliases: Vec<Option<String>>,
        namespace: bool,
        wildcard: bool,
    },
    If {
        cond: Expr,
        then_block: Block,
        else_block: Option<Block>,
    },
    When {
        name: String,
        value: Expr,
        then_block: Block,
        else_block: Option<Block>,
    },
    While {
        cond: Expr,
        body: Block,
    },
    For {
        var: String,
        start: Expr,
        end: Expr,
        // `step` is `None` for plain `a..b` (implicit step of 1, ascending —
        // unchanged from before this field existed). `Some(expr)` is the
        // `step` clause (`for i in 10..0 step -2 { .. }`); its sign is
        // evaluated once per loop entry and decides the comparison direction
        // (`<` for a non-negative step, `>` for negative), so a *dynamic*
        // (non-literal) step expression is allowed and still does the right
        // thing. The end bound stays exclusive either way, matching the
        // existing ascending-range convention rather than Kotlin's inclusive
        // `downTo` — one exclusivity rule regardless of direction.
        step: Option<Expr>,
        body: Block,
    },
    /// Iterate array elements or Unicode scalar strings, evaluating the
    /// collection expression once at loop entry.
    ForEach {
        var: String,
        iterable: Expr,
        body: Block,
    },
    Break,
    Continue,
    Crash {
        value: Expr,
        context: Option<String>,
    },
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Located(crate::diagnostic::SourceSpan, Box<Expr>),
    Int(i64),
    Float(f64),
    Str(String),
    /// Immutable compiler snapshot, freshly allocated on runtime materialization.
    Bytes(Vec<u8>),
    Interpolated(Vec<StringPart>),
    Bool(bool),
    Nil,
    /// A compiler-only type expression, accepted in a `comptime T: type` initializer.
    TypeLiteral(TypeAnnotation),
    Ident(String),
    Array(Vec<Expr>),
    Tuple(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    Call(String, Vec<CallArgument>),
    /// Brackets are classified by lexical resolution as specialization or indexing.
    BracketApply(Box<Expr>, Vec<Expr>),
    /// A postfix call whose callee has not yet been resolved.
    Apply(Box<Expr>, Vec<CallArgument>),
    /// Explicit compile-time type arguments, erased from the runtime call ABI.
    SpecializedCall(String, Vec<TypeAnnotation>, Vec<CallArgument>),
    /// Method call: `receiver.method(args...)`. The receiver is part of the
    /// expression, not syntactic sugar for a wrapper function. Built-in
    /// methods lower to VM primitives or linked runtime calls.
    MethodCall(Box<Expr>, String, Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// Normal operand passes through; Error exits the enclosing callable after cleanup.
    Propagate(Box<Expr>),
    /// Lazy expression-level recovery, distinct from function propagation.
    Fallback {
        kind: FallbackKind,
        value: Box<Expr>,
        fallback: Box<Expr>,
    },
    Block(Block),
    Match {
        value: Box<Expr>,
        arms: Vec<MatchArm>,
        exhaustive: bool,
    },
    Try {
        body: Block,
        catch_name: String,
        catch_block: Block,
    },
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Option<Box<Expr>>,
    },
    // `expr as Type` — checked conversion through the native ABI.
    // No `?` here — unlike a `var`/`const`
    // `TypeAnnotation`, a cast always produces a concrete value or a runtime
    // `Value::Error` (e.g. `"abc" as int`), never `nil`.
    Cast(Box<Expr>, VarType),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Text(String),
    Expression(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackKind {
    Nil,
    Error,
}

impl Expr {
    pub fn node(&self) -> &Self {
        match self {
            Self::Located(_, inner) => inner.node(),
            _ => self,
        }
    }

    pub fn span(&self) -> Option<&crate::diagnostic::SourceSpan> {
        match self {
            Self::Located(span, _) => Some(span),
            _ => None,
        }
    }
}

impl Stmt {
    pub fn node(&self) -> &Self {
        match self {
            Self::Located(_, inner) => inner.node(),
            _ => self,
        }
    }

    pub fn span(&self) -> Option<&crate::diagnostic::SourceSpan> {
        match self {
            Self::Located(span, _) => Some(span),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Xor,
    Append,
    Prepend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}
