//! Semantic checking is independent of instruction emission. Poison represents
//! invalid source, not a runtime error value, and suppresses derivative errors.
use crate::ast::*;
use crate::builtins::Builtin;
use crate::diagnostic::{Diagnostic, SourceSpan};
use std::collections::{HashMap, HashSet};

const CLONE_TRAIT: &str = "$core::clone::Clone";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Type {
    Primitive(VarType, bool),
    Nil,
    Never,
    EmptyArray,
    Array(Box<Type>),
    Tuple(Vec<Type>),
    Bytes,
    StrBuf,
    Handle,
    Record,
    Struct(usize, String, Vec<Type>),
    Enum(usize, String),
    Named(String),
    Generic(String),
    Dynamic,
    ErrorValue,
    Poison,
}

fn type_compatible(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Dynamic, _) | (_, Type::Poison | Type::ErrorValue | Type::Never) => true,
        (Type::Primitive(a, nullable), Type::Primitive(b, actual_nullable)) => {
            a == b && (!*actual_nullable || *nullable)
        }
        (Type::Primitive(_, true), Type::Nil) | (Type::Nil, Type::Nil) => true,
        (Type::Named(expected), Type::Struct(_, actual, _) | Type::Enum(_, actual)) => {
            expected == actual || actual.ends_with(&format!("::{expected}"))
        }
        (
            Type::Struct(expected_id, expected_name, expected_args),
            Type::Struct(actual_id, actual_name, actual_args),
        ) => {
            expected_id == actual_id
                && expected_name == actual_name
                && expected_args.len() == actual_args.len()
                && expected_args
                    .iter()
                    .zip(actual_args)
                    .all(|(expected, actual)| type_compatible(expected, actual))
        }
        (Type::Enum(expected, _), Type::Enum(actual, _)) => expected == actual,
        (Type::Array(_), Type::EmptyArray) => true,
        (Type::Array(expected), Type::Array(actual)) => array_element_compatible(expected, actual),
        (Type::Tuple(a), Type::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| type_compatible(x, y))
        }
        (a, b) => a == b,
    }
}

fn array_element_compatible(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Dynamic, _) | (_, Type::Dynamic) => true,
        (_, Type::Poison | Type::ErrorValue | Type::Never) => true,
        (Type::Enum(expected, _), Type::Enum(actual, _)) => expected == actual,
        (Type::Array(_), Type::EmptyArray) => true,
        (Type::Array(expected), Type::Array(actual)) => array_element_compatible(expected, actual),
        (Type::Tuple(expected), Type::Tuple(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| array_element_compatible(expected, actual))
        }
        (expected, actual) => expected == actual,
    }
}

fn generic_extension_matches(extension: &ExtendDecl, receiver: &Type) -> bool {
    let parameters: HashSet<_> = extension
        .type_params
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    let mut substitutions = HashMap::new();
    if !match_extension_type(&extension.target, receiver, &parameters, &mut substitutions) {
        return false;
    }
    extension
        .type_params
        .iter()
        .all(|parameter| substitutions.contains_key(&parameter.name))
}

fn extension_target_uses_parameter(ty: &TypeAnnotation, name: &str) -> bool {
    match &ty.kind {
        TypeKind::TypeParameter(parameter) => parameter == name,
        TypeKind::Array(element) => extension_target_uses_parameter(element, name),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            arguments
                .iter()
                .any(|argument| extension_target_uses_parameter(argument, name))
        }
        TypeKind::Tuple(elements) => elements
            .iter()
            .any(|element| extension_target_uses_parameter(element, name)),
        TypeKind::Primitive(_)
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::SelfType => false,
    }
}

fn match_extension_type(
    pattern: &TypeAnnotation,
    actual: &Type,
    parameters: &HashSet<&str>,
    substitutions: &mut HashMap<String, Type>,
) -> bool {
    match &pattern.kind {
        TypeKind::TypeParameter(name) if parameters.contains(name.as_str()) => {
            if matches!(
                actual,
                Type::Dynamic | Type::Generic(_) | Type::Poison | Type::ErrorValue
            ) {
                return false;
            }
            match substitutions.get(name) {
                Some(previous) => type_compatible(previous, actual),
                None => {
                    substitutions.insert(name.clone(), actual.clone());
                    true
                }
            }
        }
        TypeKind::Applied { source, arguments }
        | TypeKind::ResolvedApplied {
            symbol: source,
            arguments,
            ..
        } => {
            let Type::Struct(_, actual_name, actual_arguments) = actual else {
                return false;
            };
            (source == actual_name || actual_name.ends_with(&format!("::{source}")))
                && arguments.len() == actual_arguments.len()
                && arguments
                    .iter()
                    .zip(actual_arguments)
                    .all(|(pattern, actual)| {
                        match_extension_type(pattern, actual, parameters, substitutions)
                    })
        }
        TypeKind::Array(element) => match actual {
            Type::Array(actual) => match_extension_type(element, actual, parameters, substitutions),
            _ => false,
        },
        TypeKind::Tuple(elements) => match actual {
            Type::Tuple(actual) if elements.len() == actual.len() => {
                elements.iter().zip(actual).all(|(pattern, actual)| {
                    match_extension_type(pattern, actual, parameters, substitutions)
                })
            }
            _ => false,
        },
        TypeKind::Primitive(base) => match actual {
            Type::Primitive(actual, nullable) => base == actual && (!*nullable || pattern.nilable),
            Type::Nil => *base == VarType::Nil,
            _ => false,
        },
        TypeKind::ResolvedName { symbol, .. } | TypeKind::Named(symbol) => match actual {
            Type::Struct(_, name, arguments) => {
                arguments.is_empty() && (symbol == name || name.ends_with(&format!("::{symbol}")))
            }
            Type::Enum(_, name) => symbol == name || name.ends_with(&format!("::{symbol}")),
            Type::Primitive(base, nullable) => {
                let named = match symbol.as_str() {
                    "int" => VarType::Int,
                    "bool" => VarType::Bool,
                    "str" => VarType::Str,
                    "float" => VarType::Float,
                    _ => return false,
                };
                named == *base && (!*nullable || pattern.nilable)
            }
            _ => false,
        },
        TypeKind::SelfType => false,
        TypeKind::TypeParameter(_) => false,
    }
}

fn contains_unresolved_empty_array(ty: &Type) -> bool {
    match ty {
        Type::EmptyArray => true,
        Type::Array(element) => contains_unresolved_empty_array(element),
        Type::Tuple(items) => items.iter().any(contains_unresolved_empty_array),
        _ => false,
    }
}

fn replace_self_type(ty: &mut TypeAnnotation, target: &TypeAnnotation) {
    match &mut ty.kind {
        TypeKind::SelfType => {
            let nilable = ty.nilable || target.nilable;
            *ty = target.clone();
            ty.nilable = nilable;
        }
        TypeKind::Array(element) => replace_self_type(element, target),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            for argument in arguments {
                replace_self_type(argument, target);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                replace_self_type(element, target);
            }
        }
        TypeKind::Primitive(_)
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_) => {}
    }
}

fn substitute_trait_type(ty: &mut TypeAnnotation, substitutions: &HashMap<String, TypeAnnotation>) {
    if let TypeKind::TypeParameter(name) = &ty.kind {
        if let Some(replacement) = substitutions.get(name) {
            let nilable = ty.nilable || replacement.nilable;
            *ty = replacement.clone();
            ty.nilable = nilable;
        }
        return;
    }
    match &mut ty.kind {
        TypeKind::Array(element) => substitute_trait_type(element, substitutions),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            for argument in arguments {
                substitute_trait_type(argument, substitutions);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                substitute_trait_type(element, substitutions);
            }
        }
        TypeKind::Primitive(_)
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => {}
    }
}

fn is_concrete_trait_argument(ty: &Type) -> bool {
    match ty {
        Type::Dynamic | Type::Generic(_) | Type::EmptyArray | Type::Never | Type::Poison => false,
        Type::Array(element) => is_concrete_trait_argument(element),
        Type::Tuple(elements) => elements.iter().all(is_concrete_trait_argument),
        _ => true,
    }
}

impl Type {
    fn dynamic_array() -> Self {
        Self::Array(Box::new(Self::Dynamic))
    }
    fn concrete(base: VarType) -> Self {
        Self::Primitive(base, false)
    }

    fn display(&self) -> String {
        match self {
            Self::Primitive(base, nilable) => TypeAnnotation {
                kind: TypeKind::Primitive(*base),
                nilable: *nilable,
            }
            .display(),
            Self::Nil => "nil".into(),
            Self::Never => "<no normal exit>".into(),
            Self::EmptyArray => "empty array".into(),
            Self::Array(element) => format!("{}[]", element.display()),
            Self::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(Type::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Bytes => "bytes".into(),
            Self::StrBuf => "string builder".into(),
            Self::Handle => "resource".into(),
            Self::Record => "record".into(),
            Self::Struct(_, name, arguments) if !arguments.is_empty() => format!(
                "{}[{}]",
                name,
                arguments
                    .iter()
                    .map(Type::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Struct(_, _, _) => "record".into(),
            Self::Enum(_, _) => "enum".into(),
            Self::Named(name) => name.clone(),
            Self::Generic(name) => name.clone(),
            Self::Dynamic => "dynamic".into(),
            Self::ErrorValue => "error".into(),
            Self::Poison => "<invalid>".into(),
        }
    }

    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Poison, _) | (_, Self::Poison) => Self::Poison,
            (Self::Never, t) | (t, Self::Never) => t,
            (Self::ErrorValue, t) | (t, Self::ErrorValue) => t,
            (Self::Primitive(a, an), Self::Primitive(b, bn)) if a == b => {
                Self::Primitive(a, an || bn)
            }
            (Self::Primitive(t, _), Self::Nil) | (Self::Nil, Self::Primitive(t, _)) => {
                Self::Primitive(t, true)
            }
            (Self::EmptyArray, Self::Array(element)) | (Self::Array(element), Self::EmptyArray) => {
                Self::Array(element)
            }
            (Self::Array(a), Self::Array(b)) if a == b => Self::Array(a),
            (Self::Array(a), Self::Array(b)) => {
                let element = (*a).join(*b);
                if element == Self::Dynamic {
                    Self::Dynamic
                } else {
                    Self::Array(Box::new(element))
                }
            }
            (Self::Enum(a, left), Self::Enum(b, right)) if a == b => {
                let parent = left
                    .rsplit_once("::")
                    .map_or(left.as_str(), |(parent, _)| parent);
                let right_parent = right
                    .rsplit_once("::")
                    .map_or(right.as_str(), |(parent, _)| parent);
                if parent == right_parent {
                    Self::Enum(a, parent.to_owned())
                } else {
                    Self::Dynamic
                }
            }
            (Self::Tuple(a), Self::Tuple(b)) if a == b => Self::Tuple(a),
            (a, b) if a == b => a,
            _ => Self::Dynamic,
        }
    }
}

fn structurally_cloneable(ty: &Type) -> bool {
    match ty {
        Type::Primitive(_, _) | Type::Nil => true,
        Type::Array(element) => structurally_cloneable(element),
        Type::Tuple(items) => items.iter().all(structurally_cloneable),
        _ => false,
    }
}

fn compatible_eq_hash_traits(checker: &Checker<'_>, left: &str, right: &str) -> bool {
    let Some(left) = checker.traits.get(left) else {
        return false;
    };
    let Some(right) = checker.traits.get(right) else {
        return false;
    };
    let is_equality = |declaration: &TraitDecl| {
        declaration.type_params.len() == 1
            && declaration.methods.len() == 1
            && declaration.methods[0].name == "equals"
    };
    let is_hashable = |declaration: &TraitDecl| {
        declaration.type_params.is_empty()
            && declaration.methods.len() == 2
            && declaration
                .methods
                .iter()
                .any(|method| method.name == "hash_code")
            && declaration
                .methods
                .iter()
                .any(|method| method.name == "equals")
    };
    (is_equality(left) && is_hashable(right)) || (is_hashable(left) && is_equality(right))
}

fn equivalent_equals_body(left: &FunctionDecl, right: &FunctionDecl) -> bool {
    left.body.stmts.is_empty()
        && right.body.stmts.is_empty()
        && left
            .body
            .tail
            .as_deref()
            .zip(right.body.tail.as_deref())
            .is_some_and(|(left, right)| equivalent_expression(left, right))
}

fn equivalent_expression(left: &Expr, right: &Expr) -> bool {
    match (left.node(), right.node()) {
        (Expr::Int(left), Expr::Int(right)) => left == right,
        (Expr::Float(left), Expr::Float(right)) => left.to_bits() == right.to_bits(),
        (Expr::Str(left), Expr::Str(right)) => left == right,
        (Expr::Bytes(left), Expr::Bytes(right)) => left == right,
        (Expr::Bool(left), Expr::Bool(right)) => left == right,
        (Expr::Nil, Expr::Nil) => true,
        (Expr::TypeLiteral(left), Expr::TypeLiteral(right)) => left == right,
        (Expr::Ident(left), Expr::Ident(right)) => left == right,
        (Expr::Index(left_a, left_b), Expr::Index(right_a, right_b))
        | (Expr::Binary(_, left_a, left_b), Expr::Binary(_, right_a, right_b)) => {
            let same_operator = match (left.node(), right.node()) {
                (Expr::Binary(left, ..), Expr::Binary(right, ..)) => left == right,
                (Expr::Index(..), Expr::Index(..)) => true,
                _ => false,
            };
            same_operator
                && equivalent_expression(left_a, right_a)
                && equivalent_expression(left_b, right_b)
        }
        (Expr::Unary(left_op, left), Expr::Unary(right_op, right)) => {
            left_op == right_op && equivalent_expression(left, right)
        }
        (Expr::Propagate(left), Expr::Propagate(right)) => equivalent_expression(left, right),
        (Expr::Array(left), Expr::Array(right)) | (Expr::Tuple(left), Expr::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| equivalent_expression(left, right))
        }
        (Expr::Call(left_name, left_args), Expr::Call(right_name, right_args)) => {
            left_name == right_name && equivalent_arguments(left_args, right_args)
        }
        (
            Expr::SpecializedCall(left_name, left_types, left_args),
            Expr::SpecializedCall(right_name, right_types, right_args),
        ) => {
            left_name == right_name
                && left_types == right_types
                && equivalent_arguments(left_args, right_args)
        }
        (
            Expr::MethodCall(left_recv, left_name, left_args),
            Expr::MethodCall(right_recv, right_name, right_args),
        ) => {
            left_name == right_name
                && equivalent_expression(left_recv, right_recv)
                && left_args.len() == right_args.len()
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| equivalent_expression(left, right))
        }
        (Expr::Record(left), Expr::Record(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((left_name, left), (right_name, right))| {
                        left_name == right_name && equivalent_expression(left, right)
                    })
        }
        _ => false,
    }
}

fn equivalent_arguments(left: &[CallArgument], right: &[CallArgument]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.name == right.name && equivalent_expression(&left.value, &right.value)
        })
}

fn c_record_kind(facts: &mut crate::mutability::Facts, id: usize, ty: Type) {
    use crate::mutability::ValueKind;
    facts.values.insert(
        id,
        match ty {
            Type::Poison => ValueKind::Invalid,
            Type::Never => ValueKind::Never,
            Type::Primitive(_, _) | Type::Nil | Type::Handle | Type::ErrorValue => {
                ValueKind::Scalar
            }
            _ => ValueKind::Reference,
        },
    );
}

#[derive(Clone)]
struct Binding {
    ty: Type,
    mutable: bool,
    // Unannotated var constraints: unknown writes require a conservative dynamic
    // result, while contradictory known writes remain type errors.
    inferred: Option<(usize, Type)>,
    id: usize,
    error_only: bool,
}

/// Facts known on one normal control-flow path. Normal-value refinement and
/// Error exclusion are independent because a value can carry either, both, or
/// neither state at runtime.
#[derive(Clone, Default)]
struct FlowFacts {
    normal_types: HashMap<usize, Type>,
    error_excluded: HashSet<usize>,
}

impl FlowFacts {
    fn join(states: impl IntoIterator<Item = Self>) -> Self {
        let mut states = states.into_iter();
        let Some(mut joined) = states.next() else {
            return Self::default();
        };
        for state in states {
            joined
                .normal_types
                .retain(|id, ty| state.normal_types.get(id).is_some_and(|other| other == ty));
            joined
                .error_excluded
                .retain(|id| state.error_excluded.contains(id));
        }
        joined
    }

    fn forget(&mut self, id: usize) {
        self.normal_types.remove(&id);
        self.error_excluded.remove(&id);
    }
}

#[derive(Default)]
struct Scope {
    bindings: HashMap<String, Binding>,
    imports: HashMap<String, String>,
    modules: HashMap<String, String>,
}

struct Checker<'a> {
    functions: HashMap<String, &'a FunctionDecl>,
    traits: HashMap<String, &'a TraitDecl>,
    constructors: HashMap<String, (usize, &'a [StructField])>,
    struct_parameters: HashMap<String, Vec<TypeParameter>>,
    struct_fields: Vec<&'a [StructField]>,
    struct_owners: Vec<Option<String>>,
    enums: HashMap<String, usize>,
    variants: HashMap<String, (usize, &'a [StructField])>,
    enum_variants: Vec<Vec<String>>,
    globals: HashMap<String, Binding>,
    scopes: Vec<Scope>,
    module: Option<String>,
    owner: Option<String>,
    loops: usize,
    loop_breaks: Vec<bool>,
    normal_reachable: bool,
    flow_facts: FlowFacts,
    in_cleanup: bool,
    in_function: bool,
    in_comptime: bool,
    in_comptime_initializer: bool,
    result_type: Option<TypeAnnotation>,
    dynamic_bindings: std::collections::HashSet<usize>,
    unresolved_empty_arrays: HashMap<usize, (String, SourceSpan)>,
    next_binding: usize,
    diagnostics: Vec<Diagnostic>,
    location: Option<SourceSpan>,
    access_facts: crate::mutability::Facts,
    extension_methods: HashMap<(Type, String), &'a ExtensionMethod>,
    extension_method_traits: HashMap<(Type, String), Option<&'a str>>,
    generic_extension_methods: Vec<(&'a ExtendDecl, &'a ExtensionMethod)>,
    trait_implementations: std::collections::HashSet<(Type, String, Vec<Type>)>,
    generic_trait_implementations: Vec<&'a ExtendDecl>,
    current_function: Option<String>,
    method_calls: HashMap<CallSite, String>,
    call_specializations: HashMap<CallSite, String>,
    struct_call_specializations: HashMap<CallSite, String>,
    specializations: Vec<Specialization>,
    specialization_names: HashMap<(String, Vec<Type>), String>,
    struct_specializations: Vec<StructSpecialization>,
    struct_specialization_names: HashMap<(String, Vec<Type>), String>,
}

pub(crate) type CallSite = (Option<String>, usize);

#[derive(Clone)]
pub(crate) struct Specialization {
    pub template: String,
    pub name: String,
    pub arguments: HashMap<String, TypeAnnotation>,
}

#[derive(Clone)]
pub(crate) struct StructSpecialization {
    pub template: String,
    pub name: String,
    pub arguments: HashMap<String, TypeAnnotation>,
}

pub(crate) struct CheckResult {
    pub diagnostics: Vec<Diagnostic>,
    pub method_calls: HashMap<CallSite, String>,
    pub call_specializations: HashMap<CallSite, String>,
    pub struct_call_specializations: HashMap<CallSite, String>,
    pub specializations: Vec<Specialization>,
    pub struct_specializations: Vec<StructSpecialization>,
}

pub(crate) fn expression_site(function: Option<&str>, expr: &Expr) -> CallSite {
    (function.map(str::to_owned), expr as *const Expr as usize)
}

pub fn check(program: &Program, function_modules: &HashMap<String, String>) -> Vec<Diagnostic> {
    check_with_methods(program, function_modules).diagnostics
}

pub(crate) fn check_with_methods(
    program: &Program,
    function_modules: &HashMap<String, String>,
) -> CheckResult {
    let mut c = Checker {
        functions: HashMap::new(),
        traits: HashMap::new(),
        constructors: HashMap::new(),
        struct_parameters: HashMap::new(),
        struct_fields: Vec::new(),
        struct_owners: Vec::new(),
        enums: HashMap::new(),
        variants: HashMap::new(),
        enum_variants: Vec::new(),
        globals: HashMap::new(),
        scopes: vec![Scope::default()],
        module: None,
        owner: None,
        loops: 0,
        loop_breaks: Vec::new(),
        normal_reachable: true,
        flow_facts: FlowFacts::default(),
        in_cleanup: false,
        in_function: false,
        in_comptime: false,
        in_comptime_initializer: false,
        result_type: None,
        dynamic_bindings: Default::default(),
        unresolved_empty_arrays: HashMap::new(),
        next_binding: 1,
        diagnostics: Vec::new(),
        location: None,
        access_facts: Default::default(),
        extension_methods: HashMap::new(),
        extension_method_traits: HashMap::new(),
        generic_extension_methods: Vec::new(),
        trait_implementations: Default::default(),
        generic_trait_implementations: Vec::new(),
        current_function: None,
        method_calls: HashMap::new(),
        call_specializations: HashMap::new(),
        struct_call_specializations: HashMap::new(),
        specializations: Vec::new(),
        specialization_names: HashMap::new(),
        struct_specializations: Vec::new(),
        struct_specialization_names: HashMap::new(),
    };
    for item in &program.items {
        match item {
            Item::Invalid {
                binding: Some(name),
            } => {
                let id = c.next_binding;
                c.next_binding += 1;
                c.globals.insert(
                    name.clone(),
                    Binding {
                        id,
                        error_only: false,
                        inferred: None,
                        ty: Type::Poison,
                        mutable: true,
                    },
                );
            }
            Item::Function(f) if c.functions.insert(f.name.clone(), f).is_some() => {
                c.error(
                    "E1001",
                    format!("function `{}` defined more than once", f.name),
                    f.body
                        .stmts
                        .first()
                        .and_then(Stmt::span)
                        .or_else(|| f.body.tail.as_deref().and_then(Expr::span))
                        .or(f.result_span.as_ref()),
                );
            }
            Item::Function(_) => {}
            Item::Trait(declaration)
                if c.traits
                    .insert(declaration.name.clone(), declaration)
                    .is_some() =>
            {
                c.error(
                    "E1001",
                    format!("trait `{}` defined more than once", declaration.name),
                    None,
                );
            }
            Item::Trait(_) => {}
            Item::StructDecl {
                name,
                type_params,
                fields,
                owner,
                ..
            } => {
                let id = c.struct_fields.len();
                c.struct_fields.push(fields);
                c.struct_owners.push(owner.clone());
                c.constructors.insert(name.clone(), (id, fields));
                c.struct_parameters
                    .insert(name.clone(), type_params.clone());
            }
            Item::EnumDecl { name, variants, .. } => {
                let id = c.enum_variants.len();
                c.enums.insert(name.clone(), id);
                let names = variants
                    .iter()
                    .map(|v| format!("{name}::{}", v.name))
                    .collect();
                c.enum_variants.push(names);
                for v in variants {
                    c.variants
                        .insert(format!("{name}::{}", v.name), (id, &v.payload));
                }
            }
            Item::GlobalConst { name, ty, .. }
            | Item::GlobalVar { name, ty, .. }
            | Item::Comptime { name, ty, .. } => {
                // Predeclare globals so forward references can be checked without execution.
                let ty = ty.as_ref().map_or(Type::Dynamic, |t| c.annotation_type(t));
                let id = c.next_binding;
                c.next_binding += 1;
                c.globals.insert(
                    name.clone(),
                    Binding {
                        id,
                        error_only: false,
                        inferred: None,
                        ty,
                        mutable: matches!(item, Item::GlobalVar { .. }),
                    },
                );
            }
            _ => {}
        }
    }
    for item in &program.items {
        let Item::Extend(extension) = item else {
            continue;
        };
        let target = c.annotation_type(&extension.target);
        if !extension.type_params.is_empty()
            && !matches!(target, Type::Struct(_, _, _) | Type::Array(_))
        {
            c.error(
                "E2001",
                "generic extensions currently require a generic struct or array target",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
        }
        for parameter in &extension.type_params {
            if !extension_target_uses_parameter(&extension.target, &parameter.name) {
                c.error(
                    "E2001",
                    format!(
                        "generic extension parameter `{}` must appear in its target type",
                        parameter.name
                    ),
                    extension
                        .methods
                        .first()
                        .and_then(|method| method.function.result_span.as_ref()),
                );
            }
        }
        if extension.trait_name.is_none()
            && !extension.comptime
            && !matches!(
                target,
                Type::Struct(_, _, _) | Type::Enum(_, _) | Type::Array(_)
            )
        {
            c.error(
                "E2001",
                format!(
                    "`extend` requires a named struct, enum, or generic array type; use `comptime extend` for `{}`",
                    extension.target.display()
                ),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
        }
        if let Some(trait_name) = &extension.trait_name {
            c.register_trait_implementation(&target, trait_name, extension);
        }
        for method in &extension.methods {
            let function = &method.function;
            if let Some(receiver) = function.params.first()
                && let Some(receiver_type) = &receiver.ty
                && !type_compatible(&target, &c.annotation_type(receiver_type))
            {
                c.error(
                    "E2001",
                    format!(
                        "extension method `{}()` receiver must have type {}, got {}",
                        method.name,
                        extension.target.display(),
                        receiver_type.display()
                    ),
                    function
                        .body
                        .stmts
                        .first()
                        .and_then(Stmt::span)
                        .or_else(|| function.body.tail.as_deref().and_then(Expr::span)),
                );
            }
            let duplicate = if extension.type_params.is_empty() {
                let key = (target.clone(), method.name.clone());
                let previous = c.extension_methods.get(&key).copied();
                let previous_trait = c.extension_method_traits.get(&key).copied().flatten();
                let equivalent = previous.is_some_and(|previous| {
                    method.name == "equals"
                        && previous.function.params == function.params
                        && previous.function.result_type == function.result_type
                        && equivalent_equals_body(&previous.function, function)
                        && previous_trait
                            .zip(extension.trait_name.as_deref())
                            .is_some_and(|(left, right)| compatible_eq_hash_traits(&c, left, right))
                });
                if previous.is_none() {
                    c.extension_method_traits
                        .entry(key.clone())
                        .or_insert(extension.trait_name.as_deref());
                    c.extension_methods.entry(key).or_insert(method);
                    false
                } else {
                    !equivalent
                }
            } else {
                let duplicate = c
                    .generic_extension_methods
                    .iter()
                    .any(|(existing, previous)| {
                        previous.name == method.name
                            && existing.target.canonical() == extension.target.canonical()
                    });
                if !duplicate {
                    c.generic_extension_methods.push((extension, method));
                }
                duplicate
            };
            if duplicate {
                c.error(
                    "E1001",
                    format!(
                        "extension method `{}()` is defined more than once for `{}`",
                        method.name,
                        extension.target.display()
                    ),
                    function.result_span.as_ref(),
                );
            }
            c.functions.insert(function.name.clone(), function);
        }
    }
    for item in &program.items {
        if let Item::Use(decl) = item {
            c.import(decl);
        }
    }
    for item in &program.items {
        if let Item::GlobalConst {
            name, ty, value, ..
        }
        | Item::GlobalVar {
            name, ty, value, ..
        }
        | Item::Comptime {
            name, ty, value, ..
        } = item
        {
            c.module = name.rsplit_once("::").map(|(m, _)| m.to_owned());
            let actual = c.expr(value);
            if ty.is_none() && contains_unresolved_empty_array(&actual) {
                c.error(
                    "E2001",
                    format!("cannot infer array element type for `{name}`; add a `T[]` annotation"),
                    value.span(),
                );
            }
            let valid = ty.clone().is_none_or(|expected| {
                c.assignable(expected, actual.clone(), value.span(), &format!("`{name}`"))
            });
            let binding = c.globals.get_mut(name).expect("predeclared global");
            if !valid || actual == Type::Poison {
                binding.ty = Type::Poison;
            } else if !binding.mutable && ty.is_none() {
                binding.ty = actual;
            }
        }
    }
    for item in &program.items {
        match item {
            Item::Function(f) => c.check_function(f, function_modules),
            Item::Trait(_) => {}
            Item::Extend(extension) => {
                for method in &extension.methods {
                    c.check_function(&method.function, function_modules);
                }
            }
            Item::StructDecl {
                name,
                type_params,
                fields,
                ..
            } => {
                c.module = name.rsplit_once("::").map(|(m, _)| m.to_owned());
                if type_params.is_empty() {
                    c.defaults(fields);
                }
            }
            _ => {}
        }
    }
    c.diagnostics
        .extend(crate::mutability::check(program, &c.access_facts));
    CheckResult {
        diagnostics: c.diagnostics,
        method_calls: c.method_calls,
        call_specializations: c.call_specializations,
        struct_call_specializations: c.struct_call_specializations,
        specializations: c.specializations,
        struct_specializations: c.struct_specializations,
    }
}

impl<'a> Checker<'a> {
    fn owns_struct(&self, id: usize) -> bool {
        self.owner.as_ref().is_some_and(|owner| {
            self.struct_owners
                .get(id)
                .is_some_and(|struct_owner| struct_owner.as_ref() == Some(owner))
        })
    }

    fn implements_trait(&self, ty: &Type, trait_name: &str, arguments: &[Type]) -> bool {
        self.implements_trait_inner(ty, trait_name, arguments, &mut HashSet::new())
    }

    fn implements_trait_inner(
        &self,
        ty: &Type,
        trait_name: &str,
        arguments: &[Type],
        visiting: &mut HashSet<(Type, String, Vec<Type>)>,
    ) -> bool {
        if self.trait_implementations.contains(&(
            ty.clone(),
            trait_name.to_owned(),
            arguments.to_vec(),
        )) || (trait_name == CLONE_TRAIT && arguments.is_empty() && structurally_cloneable(ty))
        {
            return true;
        }

        let query = (ty.clone(), trait_name.to_owned(), arguments.to_vec());
        if !visiting.insert(query.clone()) {
            return false;
        }
        let implemented = self.generic_trait_implementations.iter().any(|extension| {
            if extension.trait_name.as_deref() != Some(trait_name)
                || extension.trait_arguments.len() != arguments.len()
            {
                return false;
            }
            let parameters: HashSet<_> = extension
                .type_params
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect();
            let mut substitutions = HashMap::new();
            if !match_extension_type(&extension.target, ty, &parameters, &mut substitutions)
                || !extension
                    .trait_arguments
                    .iter()
                    .zip(arguments)
                    .all(|(pattern, actual)| {
                        match_extension_type(pattern, actual, &parameters, &mut substitutions)
                    })
            {
                return false;
            }

            for parameter in &extension.type_params {
                let Some(actual) = substitutions.get(&parameter.name) else {
                    return false;
                };
                for bound in &parameter.bounds {
                    let Some(bound_arguments) = bound
                        .arguments
                        .iter()
                        .map(|argument| self.concrete_annotation(argument, &substitutions, ty))
                        .collect::<Option<Vec<_>>>()
                    else {
                        return false;
                    };
                    if !self.implements_trait_inner(actual, &bound.name, &bound_arguments, visiting)
                    {
                        return false;
                    }
                }
            }
            true
        });
        visiting.remove(&query);
        implemented
    }

    fn concrete_annotation(
        &self,
        annotation: &TypeAnnotation,
        substitutions: &HashMap<String, Type>,
        self_type: &Type,
    ) -> Option<Type> {
        let ty = match &annotation.kind {
            TypeKind::Primitive(base) if *base == VarType::Nil && !annotation.nilable => Type::Nil,
            TypeKind::Primitive(base) => Type::Primitive(*base, annotation.nilable),
            TypeKind::TypeParameter(name) => substitutions.get(name)?.clone(),
            TypeKind::SelfType => self_type.clone(),
            TypeKind::Array(element) => Type::Array(Box::new(self.concrete_annotation(
                element,
                substitutions,
                self_type,
            )?)),
            TypeKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.concrete_annotation(element, substitutions, self_type))
                    .collect::<Option<Vec<_>>>()?,
            ),
            TypeKind::Applied { source, arguments } => {
                let name = self
                    .constructors
                    .keys()
                    .find(|name| *name == source || name.ends_with(&format!("::{source}")))?;
                let (id, _) = self.constructors.get(name)?;
                Type::Struct(
                    *id,
                    name.clone(),
                    arguments
                        .iter()
                        .map(|argument| {
                            self.concrete_annotation(argument, substitutions, self_type)
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } => {
                let name = if self.constructors.contains_key(symbol) {
                    symbol
                } else {
                    self.constructors
                        .keys()
                        .find(|name| *name == source || name.ends_with(&format!("::{source}")))?
                };
                let (id, _) = self.constructors.get(name)?;
                Type::Struct(
                    *id,
                    name.clone(),
                    arguments
                        .iter()
                        .map(|argument| {
                            self.concrete_annotation(argument, substitutions, self_type)
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            TypeKind::Named(name) | TypeKind::ResolvedName { source: name, .. } => {
                if let Some((id, _)) = self.constructors.get(name) {
                    Type::Struct(*id, name.clone(), Vec::new())
                } else if let Some(id) = self.enums.get(name) {
                    Type::Enum(*id, name.clone())
                } else {
                    match name.as_str() {
                        "int" => Type::concrete(VarType::Int),
                        "float" => Type::concrete(VarType::Float),
                        "bool" => Type::concrete(VarType::Bool),
                        "str" => Type::concrete(VarType::Str),
                        _ => Type::Named(name.clone()),
                    }
                }
            }
        };
        Some(ty)
    }

    fn type_annotation_for(&self, ty: &Type) -> Option<TypeAnnotation> {
        let kind = match ty {
            Type::Primitive(base, _) => TypeKind::Primitive(*base),
            Type::Nil => TypeKind::Primitive(VarType::Nil),
            Type::Array(element) => TypeKind::Array(Box::new(self.type_annotation_for(element)?)),
            Type::Tuple(items) => TypeKind::Tuple(
                items
                    .iter()
                    .map(|item| self.type_annotation_for(item))
                    .collect::<Option<Vec<_>>>()?,
            ),
            Type::Struct(_, name, arguments) if !arguments.is_empty() => {
                TypeKind::ResolvedApplied {
                    source: name.clone(),
                    symbol: name.clone(),
                    arguments: arguments
                        .iter()
                        .map(|argument| self.type_annotation_for(argument))
                        .collect::<Option<Vec<_>>>()?,
                }
            }
            Type::Struct(_, name, _) | Type::Enum(_, name) | Type::Named(name) => {
                TypeKind::ResolvedName {
                    source: name.clone(),
                    symbol: name.clone(),
                }
            }
            Type::Dynamic
            | Type::Generic(_)
            | Type::Never
            | Type::EmptyArray
            | Type::Bytes
            | Type::StrBuf
            | Type::Handle
            | Type::Record
            | Type::ErrorValue
            | Type::Poison => return None,
        };
        Some(TypeAnnotation {
            kind,
            nilable: matches!(ty, Type::Primitive(_, true)),
        })
    }

    fn register_trait_implementation(
        &mut self,
        target: &Type,
        trait_name: &str,
        extension: &'a ExtendDecl,
    ) {
        let Some(declaration) = self
            .traits
            .get(trait_name)
            .map(|declaration| (*declaration).clone())
        else {
            self.error(
                "E1002",
                format!("unknown trait `{trait_name}`"),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        };
        if matches!(
            target,
            Type::Dynamic | Type::Generic(_) | Type::EmptyArray | Type::Poison
        ) {
            self.error(
                "E2001",
                "trait implementations require a concrete target type",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        if extension.trait_arguments.len() != declaration.type_params.len() {
            self.error(
                "E2001",
                format!(
                    "trait `{trait_name}` expects {} type argument(s), got {}",
                    declaration.type_params.len(),
                    extension.trait_arguments.len()
                ),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        let generic = !extension.type_params.is_empty();
        let trait_arguments: Vec<_> = extension
            .trait_arguments
            .iter()
            .map(|argument| self.annotation_type(argument))
            .collect();
        if !generic
            && trait_arguments
                .iter()
                .any(|argument| !is_concrete_trait_argument(argument))
        {
            self.error(
                "E2001",
                "trait implementations require concrete trait type arguments",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        let duplicate_generic = self.generic_trait_implementations.iter().any(|existing| {
            existing.trait_name.as_deref() == Some(trait_name)
                && existing.target.canonical() == extension.target.canonical()
                && existing
                    .trait_arguments
                    .iter()
                    .map(TypeAnnotation::canonical)
                    .eq(extension
                        .trait_arguments
                        .iter()
                        .map(TypeAnnotation::canonical))
        });
        if self
            .trait_implementations
            .iter()
            .any(|(implemented, name, _)| implemented == target && name == trait_name)
            || duplicate_generic
        {
            self.error(
                "E1001",
                format!(
                    "trait `{trait_name}` is implemented more than once for `{}`",
                    target.display()
                ),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        let key = (
            target.clone(),
            trait_name.to_owned(),
            trait_arguments.clone(),
        );
        if !generic {
            self.trait_implementations.insert(key.clone());
        }

        let trait_substitutions: HashMap<_, _> = declaration
            .type_params
            .iter()
            .cloned()
            .zip(extension.trait_arguments.iter().cloned())
            .collect();

        let target_annotation = self
            .type_annotation_for(target)
            .or_else(|| Some(extension.target.clone()));
        let mut implementation_valid = true;
        for required in &declaration.methods {
            let Some(implementation) = extension
                .methods
                .iter()
                .find(|method| method.name == required.name)
            else {
                self.error(
                    "E2001",
                    format!(
                        "implementation of `{trait_name}` for `{}` is missing method `{}()`",
                        target.display(),
                        required.name
                    ),
                    extension
                        .methods
                        .first()
                        .and_then(|method| method.function.result_span.as_ref()),
                );
                implementation_valid = false;
                continue;
            };
            let function = &implementation.function;
            let mut valid = function.params.len() == required.params.len();
            if implementation.mutating_receiver != required.mutating_receiver {
                self.error(
                    "E2001",
                    format!(
                        "mutability of the `self` receiver for `{}()` does not match trait `{trait_name}`",
                        required.name
                    ),
                    function.result_span.as_ref(),
                );
                valid = false;
            }
            if !valid {
                self.error(
                    "E2001",
                    format!(
                        "method `{}()` for trait `{trait_name}` has {} parameter(s), expected {}",
                        required.name,
                        function.params.len(),
                        required.params.len()
                    ),
                    function.result_span.as_ref(),
                );
            }
            if function.comptime {
                self.error(
                    "E4002",
                    "trait methods must be runtime functions",
                    function.result_span.as_ref(),
                );
                valid = false;
            }
            for (index, (expected, actual)) in
                required.params.iter().zip(&function.params).enumerate()
            {
                if expected.name != actual.name {
                    self.error(
                        "E2001",
                        format!(
                            "parameter {} of `{}()` must be named `{}` to match trait `{trait_name}`",
                            index + 1,
                            required.name,
                            expected.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
                if index == 0 {
                    continue;
                }
                let Some(mut expected_type) = expected.ty.clone() else {
                    self.error(
                        "E2001",
                        format!(
                            "trait method `{}()` must type every non-self parameter",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                    continue;
                };
                let Some(actual_type) = actual.ty.as_ref() else {
                    self.error(
                        "E2001",
                        format!(
                            "implementation method `{}()` must type every parameter",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                    continue;
                };
                substitute_trait_type(&mut expected_type, &trait_substitutions);
                if let Some(target_annotation) = &target_annotation {
                    replace_self_type(&mut expected_type, target_annotation);
                }
                let expected_type = self.annotation_type(&expected_type);
                let actual_type = self.annotation_type(actual_type);
                if expected_type != actual_type {
                    self.error(
                        "E2001",
                        format!(
                            "parameter `{}` of `{}()` does not match trait `{trait_name}`",
                            actual.name, required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
            }
            match (&required.result_type, &function.result_type) {
                (Some(expected), Some(actual)) => {
                    let mut expected = expected.clone();
                    substitute_trait_type(&mut expected, &trait_substitutions);
                    if let Some(target_annotation) = &target_annotation {
                        replace_self_type(&mut expected, target_annotation);
                    }
                    let expected = self.annotation_type(&expected);
                    let actual = self.annotation_type(actual);
                    if expected != actual {
                        self.error(
                            "E2001",
                            format!(
                                "result of `{}()` does not match trait `{trait_name}`",
                                required.name
                            ),
                            function.result_span.as_ref(),
                        );
                        valid = false;
                    }
                }
                (None, None) => {}
                _ => {
                    self.error(
                        "E2001",
                        format!(
                            "result annotation of `{}()` does not match trait `{trait_name}`",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
            }
            implementation_valid &= valid;
        }
        if generic && implementation_valid {
            self.generic_trait_implementations.push(extension);
        } else if !generic && !implementation_valid {
            self.trait_implementations.remove(&key);
        }
    }

    fn explicit_substitutions(
        &mut self,
        parameters: &[TypeParameter],
        expr: &Expr,
    ) -> HashMap<String, Type> {
        let Expr::SpecializedCall(_, arguments, _) = expr.node() else {
            return HashMap::new();
        };
        if arguments.len() != parameters.len() {
            self.error("E2001", format!("expected {} explicit generic arguments, got {}; partial arguments are not supported", parameters.len(), arguments.len()), expr.span());
            return HashMap::new();
        }
        parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), self.annotation_type(argument)))
            .collect()
    }

    fn generic_call(
        &mut self,
        name: &str,
        function: &FunctionDecl,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        let bindings = crate::arguments::bind(name, &function.params, args);
        for error in bindings.errors {
            self.error(
                "E1005",
                error.message,
                error
                    .argument
                    .and_then(|index| args[index].value.span())
                    .or(expr.span()),
            );
        }

        let type_parameters: std::collections::HashSet<_> = function
            .type_params
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        let mut substitutions = self.explicit_substitutions(&function.type_params, expr);
        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(parameter_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            let Some(expected) = function.params[parameter_index].ty.as_ref() else {
                continue;
            };
            self.infer_generic_type(
                expected,
                actual_type,
                &type_parameters,
                &mut substitutions,
                args[argument_index].value.span(),
            );
        }

        for parameter in &function.type_params {
            let Some(actual_type) = substitutions.get(&parameter.name).cloned() else {
                self.error(
                    "E2001",
                    format!(
                        "cannot infer generic type parameter `{}` for `{name}`",
                        parameter.name
                    ),
                    expr.span(),
                );
                continue;
            };
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(&actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{name}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        expr.span(),
                    );
                }
            }
        }

        let missing: Vec<_> = function
            .type_params
            .iter()
            .filter(|parameter| !substitutions.contains_key(&parameter.name))
            .map(|parameter| parameter.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Type::Poison;
        }

        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(parameter_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = function.params[parameter_index].ty.as_ref() {
                let expected_type = self.substituted_type(expected, &substitutions);
                if !type_compatible(&expected_type, actual_type) {
                    self.error(
                        "E2001",
                        format!(
                            "type mismatch: parameter `{}` of `{name}` expects {}, got {}",
                            function.params[parameter_index].name,
                            expected_type.display(),
                            actual_type.display()
                        ),
                        args[argument_index].value.span(),
                    );
                }
            }
        }

        let mut annotations = HashMap::new();
        for parameter in &function.type_params {
            let Some(actual_type) = substitutions.get(&parameter.name) else {
                return Type::Poison;
            };
            let Some(annotation) = self.type_annotation_for(actual_type) else {
                self.error(
                    "E2001",
                    format!(
                        "type {} cannot be used as a runtime generic argument",
                        actual_type.display()
                    ),
                    expr.span(),
                );
                return Type::Poison;
            };
            annotations.insert(parameter.name.clone(), annotation);
        }

        let type_arguments = function
            .type_params
            .iter()
            .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
            .collect::<Vec<_>>();
        let key = (function.name.clone(), type_arguments);
        let instance = if let Some(name) = self.specialization_names.get(&key) {
            name.clone()
        } else {
            let specialized_name = format!("{}$spec{}", function.name, self.specializations.len());
            self.specialization_names
                .insert(key, specialized_name.clone());
            self.specializations.push(Specialization {
                template: function.name.clone(),
                name: specialized_name.clone(),
                arguments: annotations,
            });
            specialized_name
        };
        self.call_specializations.insert(
            expression_site(self.current_function.as_deref(), expr),
            instance.clone(),
        );
        // Mutation analysis consumes concrete effects when a specialization is
        // available, while the ordinary call mapping keeps the generic template
        // as a fallback during specialization discovery.
        self.access_facts
            .specializations
            .insert(crate::mutability::expr_id(expr), instance);

        function
            .result_type
            .as_ref()
            .map_or(Type::Dynamic, |result| {
                self.substituted_type(result, &substitutions)
            })
    }

    fn generic_struct_call(
        &mut self,
        name: &str,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        let Some((id, fields)) = self.constructors.get(name).copied() else {
            return Type::Poison;
        };
        let parameters = self
            .struct_parameters
            .get(name)
            .cloned()
            .unwrap_or_default();
        let bindings = crate::arguments::bind(name, fields, args);
        for error in bindings.errors {
            self.error(
                "E1005",
                error.message,
                error
                    .argument
                    .and_then(|index| args[index].value.span())
                    .or(expr.span()),
            );
        }
        let type_parameters: std::collections::HashSet<_> = parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        let mut substitutions = self.explicit_substitutions(&parameters, expr);
        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(field_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = fields[field_index].ty.as_ref() {
                self.infer_generic_type(
                    expected,
                    actual_type,
                    &type_parameters,
                    &mut substitutions,
                    args[argument_index].value.span(),
                );
            }
        }
        for parameter in &parameters {
            let Some(actual_type) = substitutions.get(&parameter.name).cloned() else {
                self.error(
                    "E2001",
                    format!(
                        "cannot infer generic type parameter `{}` for `{name}`",
                        parameter.name
                    ),
                    expr.span(),
                );
                continue;
            };
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(&actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{name}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        expr.span(),
                    );
                }
            }
        }
        if parameters
            .iter()
            .any(|parameter| !substitutions.contains_key(&parameter.name))
        {
            return Type::Poison;
        }
        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(field_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = fields[field_index].ty.as_ref() {
                let expected_type = self.substituted_type(expected, &substitutions);
                if actual_type == &Type::EmptyArray
                    && let Type::Array(element) = expected_type
                {
                    self.refine_empty_array(&args[argument_index].value, &element);
                } else if !type_compatible(&expected_type, actual_type) {
                    self.error(
                        "E2001",
                        format!(
                            "type mismatch: field `{}` of `{name}` expects {}, got {}",
                            fields[field_index].name,
                            expected_type.display(),
                            actual_type.display()
                        ),
                        args[argument_index].value.span(),
                    );
                }
            } else if contains_unresolved_empty_array(actual_type) {
                self.error(
                    "E2001",
                    "cannot infer array element type for an empty argument to a dynamic field; declare a typed array first",
                    args[argument_index].value.span(),
                );
            }
        }
        let Some(specialized_name) = self.request_struct_specialization(
            name,
            &parameters,
            &parameters
                .iter()
                .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
                .collect::<Vec<_>>(),
        ) else {
            return Type::Poison;
        };
        self.struct_call_specializations.insert(
            expression_site(self.current_function.as_deref(), expr),
            specialized_name,
        );
        Type::Struct(
            id,
            name.to_owned(),
            parameters
                .iter()
                .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
                .collect(),
        )
    }

    fn infer_generic_type(
        &mut self,
        pattern: &TypeAnnotation,
        actual: &Type,
        parameters: &std::collections::HashSet<&str>,
        substitutions: &mut HashMap<String, Type>,
        span: Option<&SourceSpan>,
    ) {
        match &pattern.kind {
            TypeKind::TypeParameter(name) if parameters.contains(name.as_str()) => {
                if matches!(actual, Type::ErrorValue | Type::Poison) {
                    return;
                }
                if substitutions.contains_key(name)
                    && matches!(actual, Type::Dynamic | Type::EmptyArray | Type::Generic(_))
                {
                    return;
                }
                if matches!(actual, Type::Dynamic | Type::EmptyArray | Type::Generic(_)) {
                    self.error(
                        "E2001",
                        format!("cannot infer `{name}` from {}", actual.display()),
                        span,
                    );
                    return;
                }
                match substitutions.get(name) {
                    Some(previous) if !type_compatible(previous, actual) => self.error(
                        "E2001",
                        format!(
                            "generic parameter `{name}` inferred as both {} and {}",
                            previous.display(),
                            actual.display()
                        ),
                        span,
                    ),
                    Some(_) => {}
                    None => {
                        substitutions.insert(name.clone(), actual.clone());
                    }
                }
            }
            TypeKind::Array(element) => match actual {
                Type::Array(actual_element) => self.infer_generic_type(
                    element,
                    actual_element,
                    parameters,
                    substitutions,
                    span,
                ),
                Type::EmptyArray => self.error(
                    "E2001",
                    "cannot infer a generic element type from an empty array; annotate the array first",
                    span,
                ),
                _ => {}
            },
            TypeKind::Tuple(elements) => {
                if let Type::Tuple(actual_elements) = actual
                    && elements.len() == actual_elements.len()
                {
                    for (expected, actual) in elements.iter().zip(actual_elements) {
                        self.infer_generic_type(
                            expected,
                            actual,
                            parameters,
                            substitutions,
                            span,
                        );
                    }
                }
            }
            TypeKind::Applied {
                source, arguments, ..
            }
            | TypeKind::ResolvedApplied {
                symbol: source,
                arguments,
                ..
            } => {
                if let Type::Struct(_, actual_name, actual_arguments) = actual
                    && (source == actual_name
                        || actual_name.ends_with(&format!("::{source}")))
                    && arguments.len() == actual_arguments.len()
                {
                    for (expected, actual) in arguments.iter().zip(actual_arguments) {
                        self.infer_generic_type(
                            expected,
                            actual,
                            parameters,
                            substitutions,
                            span,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn substituted_type(
        &mut self,
        annotation: &TypeAnnotation,
        substitutions: &HashMap<String, Type>,
    ) -> Type {
        match &annotation.kind {
            TypeKind::TypeParameter(name) => substitutions
                .get(name)
                .cloned()
                .unwrap_or(Type::Generic(name.clone())),
            TypeKind::Array(element) => {
                Type::Array(Box::new(self.substituted_type(element, substitutions)))
            }
            TypeKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.substituted_type(element, substitutions))
                    .collect(),
            ),
            TypeKind::Applied { source, arguments } => {
                let symbol = self.resolve(source, None).unwrap_or_else(|| source.clone());
                self.substituted_applied_type(&symbol, arguments, substitutions)
            }
            TypeKind::ResolvedApplied {
                symbol, arguments, ..
            } => self.substituted_applied_type(symbol, arguments, substitutions),
            _ => self.annotation_type(annotation),
        }
    }

    fn substituted_applied_type(
        &mut self,
        symbol: &str,
        arguments: &[TypeAnnotation],
        substitutions: &HashMap<String, Type>,
    ) -> Type {
        let Some((id, _)) = self.constructors.get(symbol).copied() else {
            return Type::Poison;
        };
        Type::Struct(
            id,
            symbol.to_owned(),
            arguments
                .iter()
                .map(|argument| self.substituted_type(argument, substitutions))
                .collect(),
        )
    }

    fn check_function(&mut self, f: &FunctionDecl, function_modules: &HashMap<String, String>) {
        if !f.type_params.is_empty() {
            return;
        }
        let previous_flow_facts = std::mem::take(&mut self.flow_facts);
        let previous_function = self.current_function.replace(f.name.clone());
        let previous_owner = std::mem::replace(&mut self.owner, f.owner.clone());
        self.module = function_modules
            .get(&f.name)
            .cloned()
            .or_else(|| f.name.rsplit_once("::").map(|(m, _)| m.to_owned()));
        loop {
            self.flow_facts = FlowFacts::default();
            self.unresolved_empty_arrays.clear();
            let diagnostics_start = self.diagnostics.len();
            let unresolved = self.dynamic_bindings.len();
            self.scopes.push(Scope::default());
            self.in_comptime = f.comptime;
            self.defaults(&f.params);
            for parameter in &f.params {
                let ty = parameter
                    .ty
                    .as_ref()
                    .map_or(Type::Dynamic, |ty| self.annotation_type(ty));
                self.define(
                    &parameter.name,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty,
                        mutable: true,
                    },
                    None,
                );
            }
            self.in_function = true;
            self.result_type = f.result_type.clone();
            self.location = f.result_span.clone();
            let result = self.block(&f.body);
            for (_, (name, span)) in std::mem::take(&mut self.unresolved_empty_arrays) {
                self.error(
                    "E2001",
                    format!("cannot infer array element type for `{name}`; add a `T[]` annotation"),
                    Some(&span),
                );
            }
            if let Some(expected) = f.result_type.clone() {
                self.assignable(
                    expected,
                    result,
                    f.body
                        .tail
                        .as_deref()
                        .and_then(Expr::span)
                        .or(f.result_span.as_ref()),
                    &format!("normal result of `{}`", f.name),
                );
            } else if contains_unresolved_empty_array(&result) {
                self.error(
                    "E2001",
                    format!("cannot infer array element type in the result of `{}`; add a `T[]` annotation", f.name),
                    f.body.tail.as_deref().and_then(Expr::span),
                );
            }
            self.in_function = false;
            self.in_comptime = false;
            self.result_type = None;
            self.location = None;
            self.scopes.pop();
            if unresolved == self.dynamic_bindings.len() {
                break;
            }
            self.diagnostics.truncate(diagnostics_start);
        }
        self.flow_facts = previous_flow_facts;
        self.current_function = previous_function;
        self.owner = previous_owner;
    }

    fn annotation_type(&mut self, ty: &TypeAnnotation) -> Type {
        match &ty.kind {
            TypeKind::Primitive(base) if *base == VarType::Nil && !ty.nilable => Type::Nil,
            TypeKind::Primitive(base) => Type::Primitive(*base, ty.nilable),
            TypeKind::Named(name) => {
                let resolved = self.resolve(name, None).unwrap_or_else(|| name.clone());
                if let Some((id, _)) = self.constructors.get(&resolved) {
                    if self
                        .struct_parameters
                        .get(&resolved)
                        .is_some_and(|parameters| !parameters.is_empty())
                    {
                        self.error(
                            "E2001",
                            format!("generic struct `{name}` requires type arguments"),
                            None,
                        );
                        Type::Poison
                    } else {
                        Type::Struct(*id, resolved, Vec::new())
                    }
                } else if let Some(id) = self.enums.get(&resolved) {
                    Type::Enum(*id, resolved)
                } else {
                    Type::Named(resolved)
                }
            }
            TypeKind::ResolvedName { source, symbol } => {
                if let Some((id, _)) = self.constructors.get(symbol) {
                    if self
                        .struct_parameters
                        .get(symbol)
                        .is_some_and(|parameters| !parameters.is_empty())
                    {
                        self.error(
                            "E2001",
                            format!("generic struct `{source}` requires type arguments"),
                            None,
                        );
                        Type::Poison
                    } else {
                        Type::Struct(*id, symbol.clone(), Vec::new())
                    }
                } else if let Some(id) = self.enums.get(symbol) {
                    Type::Enum(*id, symbol.clone())
                } else {
                    Type::Named(if symbol == source {
                        source.clone()
                    } else {
                        symbol.clone()
                    })
                }
            }
            TypeKind::Applied { source, arguments } => {
                let symbol = self.resolve(source, None).unwrap_or_else(|| source.clone());
                self.applied_struct_type(source, &symbol, arguments)
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } => self.applied_struct_type(source, symbol, arguments),
            TypeKind::TypeParameter(name) => Type::Generic(name.clone()),
            TypeKind::SelfType => Type::Named("Self".into()),
            TypeKind::Array(element) => Type::Array(Box::new(self.annotation_type(element))),
            TypeKind::Tuple(items) => Type::Tuple(
                items
                    .iter()
                    .map(|item| self.annotation_type(item))
                    .collect(),
            ),
        }
    }

    fn applied_struct_type(
        &mut self,
        source: &str,
        symbol: &str,
        arguments: &[TypeAnnotation],
    ) -> Type {
        let Some((id, _)) = self.constructors.get(symbol).copied() else {
            self.error("E2001", format!("`{source}` is not a generic struct"), None);
            return Type::Poison;
        };
        let parameters = self
            .struct_parameters
            .get(symbol)
            .cloned()
            .unwrap_or_default();
        if parameters.is_empty() || parameters.len() != arguments.len() {
            self.error(
                "E2001",
                format!(
                    "generic struct `{source}` expects {} type argument(s), got {}",
                    parameters.len(),
                    arguments.len()
                ),
                None,
            );
            return Type::Poison;
        }
        let actual: Vec<_> = arguments
            .iter()
            .map(|argument| self.annotation_type(argument))
            .collect();
        let substitutions: HashMap<_, _> = parameters
            .iter()
            .zip(&actual)
            .map(|(parameter, actual)| (parameter.name.clone(), actual.clone()))
            .collect();
        for (parameter, actual_type) in parameters.iter().zip(&actual) {
            if !is_concrete_trait_argument(actual_type) {
                continue;
            }
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{source}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        None,
                    );
                }
            }
        }
        if actual.iter().all(is_concrete_trait_argument) {
            self.request_struct_specialization(symbol, &parameters, &actual);
        }
        Type::Struct(id, symbol.to_owned(), actual)
    }

    fn request_struct_specialization(
        &mut self,
        template: &str,
        parameters: &[TypeParameter],
        arguments: &[Type],
    ) -> Option<String> {
        if parameters.len() != arguments.len() || !arguments.iter().all(is_concrete_trait_argument)
        {
            return None;
        }
        let key = (template.to_owned(), arguments.to_vec());
        if let Some(name) = self.struct_specialization_names.get(&key) {
            return Some(name.clone());
        }
        let annotations: Vec<_> = arguments
            .iter()
            .map(|argument| self.type_annotation_for(argument))
            .collect::<Option<_>>()?;
        let name = format!(
            "{}[{}]",
            template,
            annotations
                .iter()
                .map(TypeAnnotation::canonical)
                .collect::<Vec<_>>()
                .join(",")
        );
        let substitutions: HashMap<_, _> = parameters
            .iter()
            .zip(&annotations)
            .map(|(parameter, annotation)| (parameter.name.clone(), annotation.clone()))
            .collect();
        self.struct_specialization_names.insert(key, name.clone());
        self.struct_specializations.push(StructSpecialization {
            template: template.to_owned(),
            name: name.clone(),
            arguments: substitutions,
        });
        Some(name)
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: Option<&SourceSpan>) {
        self.diagnostics.push(Diagnostic::new(
            code,
            message,
            span.cloned().or_else(|| self.location.clone()),
        ));
    }

    fn assignable(
        &mut self,
        expected: TypeAnnotation,
        actual: Type,
        span: Option<&SourceSpan>,
        binding: &str,
    ) -> bool {
        let expected_type = self.annotation_type(&expected);
        let valid = matches!(actual, Type::Poison | Type::ErrorValue | Type::Never)
            || type_compatible(&expected_type, &actual)
            || matches!(actual, Type::Nil) && expected.nilable;
        if !valid {
            let mut diagnostic = Diagnostic::new(
                "E2001",
                format!(
                    "type mismatch: {binding} declared as {} but given {}",
                    expected.display(),
                    actual.display()
                ),
                span.cloned().or_else(|| self.location.clone()),
            );
            if let TypeKind::Primitive(base) = &expected.kind
                && *base != VarType::Nil
                && matches!(actual, Type::Dynamic | Type::Primitive(_, _))
            {
                diagnostic = diagnostic.note(format!(
                    "use `as {}` to request a checked conversion; it does not require try/catch",
                    base.name()
                ));
            }
            self.diagnostics.push(diagnostic);
        }
        valid
    }

    fn assignable_type(
        &mut self,
        expected: Type,
        actual: Type,
        span: Option<&SourceSpan>,
        binding: &str,
    ) {
        match expected {
            Type::Primitive(base, nilable) => {
                self.assignable(
                    TypeAnnotation {
                        kind: TypeKind::Primitive(base),
                        nilable,
                    },
                    actual,
                    span,
                    binding,
                );
            }
            Type::Nil => {
                self.assignable(
                    TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Nil),
                        nilable: false,
                    },
                    actual,
                    span,
                    binding,
                );
            }
            Type::Dynamic | Type::Poison => {}
            _ if type_compatible(&expected, &actual) => {}
            _ => self.error(
                "E2001",
                format!(
                    "type mismatch: {binding} declared as {} but given {}",
                    expected.display(),
                    actual.display()
                ),
                span,
            ),
        }
    }

    fn define(&mut self, name: &str, mut binding: Binding, span: Option<&SourceSpan>) {
        binding.id = self.next_binding;
        self.next_binding += 1;
        let scope = self.scopes.last_mut().expect("root scope");
        if scope.bindings.insert(name.to_owned(), binding).is_some() {
            self.error(
                "E1001",
                format!("binding `{name}` defined more than once in this scope"),
                span,
            );
        }
    }

    fn local(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.bindings.get(name).cloned())
    }

    fn effective_type(&self, binding: &Binding) -> Type {
        if binding.error_only {
            Type::ErrorValue
        } else {
            self.flow_facts
                .normal_types
                .get(&binding.id)
                .cloned()
                .unwrap_or_else(|| binding.ty.clone())
        }
    }

    fn binding_for_value(&self, value: &Expr) -> Option<Binding> {
        let Expr::Ident(name) = value.node() else {
            return None;
        };
        self.local(name).or_else(|| self.globals.get(name).cloned())
    }

    fn apply_type_pattern(&mut self, value: &Expr, name: &str, positive: bool) {
        let Some(binding) = self.binding_for_value(value) else {
            return;
        };
        if binding.id == 0 {
            return;
        }
        let error_pattern = name == "Error"
            || name == crate::error::ERROR_TYPE
            || name.starts_with(&format!("{}::", crate::error::ERROR_TYPE));
        if error_pattern {
            if positive {
                self.flow_facts.error_excluded.remove(&binding.id);
            } else if name == "Error" || name == crate::error::ERROR_TYPE {
                self.flow_facts.error_excluded.insert(binding.id);
            }
            return;
        }

        let tested = if name == "nil" {
            Some(Type::Nil)
        } else if let Some(primitive) = crate::instruction::ValueType::primitive(name) {
            match primitive {
                crate::instruction::ValueType::Int => Some(Type::concrete(VarType::Int)),
                crate::instruction::ValueType::Float => Some(Type::concrete(VarType::Float)),
                crate::instruction::ValueType::Bool => Some(Type::concrete(VarType::Bool)),
                crate::instruction::ValueType::Str => Some(Type::concrete(VarType::Str)),
                crate::instruction::ValueType::Nil | crate::instruction::ValueType::Error => None,
                crate::instruction::ValueType::Enum(_)
                | crate::instruction::ValueType::Variant(_) => None,
            }
        } else {
            self.enums
                .get(name)
                .map(|id| Type::Enum(*id, name.to_owned()))
        };

        if positive {
            // Any successful non-Error type test proves the value is a normal
            // value. A nil test retains a nil normal type; a concrete primitive
            // test removes nullable from the matching primitive.
            self.flow_facts.error_excluded.insert(binding.id);
            let current = self.effective_type(&binding);
            let refined = if name == "nil" {
                Some(Type::Nil)
            } else {
                match (&current, tested) {
                    (Type::Primitive(actual, true), Some(Type::Primitive(expected, false)))
                        if actual == &expected =>
                    {
                        Some(Type::Primitive(*actual, false))
                    }
                    (Type::Dynamic, Some(tested)) => Some(tested),
                    (_, Some(tested)) if type_compatible(&current, &tested) => Some(tested),
                    _ => None,
                }
            };
            if let Some(refined) = refined {
                self.flow_facts.normal_types.insert(binding.id, refined);
            }
        } else if name == "nil" {
            let current = self.effective_type(&binding);
            if let Type::Primitive(base, true) = current {
                self.flow_facts
                    .normal_types
                    .insert(binding.id, Type::Primitive(base, false));
            }
        } else if name != "Error" {
            // A failed Error test proves only that Error is absent. It does not
            // remove nil or otherwise change the declared normal type.
            self.flow_facts.error_excluded.insert(binding.id);
        }
    }

    fn invalidate_mutable_flow_facts(&mut self) {
        let mutable_ids: HashSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .chain(self.globals.values())
            .filter(|binding| binding.mutable)
            .map(|binding| binding.id)
            .collect();
        for id in mutable_ids {
            self.flow_facts.forget(id);
        }
    }

    fn join_flow_exits(&mut self, states: impl IntoIterator<Item = FlowFacts>) {
        self.flow_facts = FlowFacts::join(states);
    }

    fn refine_empty_array(&mut self, receiver: &Expr, element: &Type) {
        let Expr::Ident(name) = receiver.node() else {
            return;
        };
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get_mut(name) {
                if binding.ty == Type::EmptyArray {
                    let inferred = Type::Array(Box::new(element.clone()));
                    binding.ty = inferred.clone();
                    if let Some((id, _)) = binding.inferred {
                        binding.inferred = Some((id, inferred));
                        self.unresolved_empty_arrays.remove(&id);
                    }
                }
                return;
            }
        }
    }

    fn refine_error_binding(&mut self, value: &Expr) {
        let Expr::Ident(name) = value.node() else {
            return;
        };
        let binding = self
            .local(name)
            .or_else(|| self.globals.get(name).cloned().filter(|b| !b.mutable));
        if let Some(mut binding) = binding {
            binding.error_only = true;
            let overlay = self.scopes.len() - 2;
            self.scopes[overlay].bindings.insert(name.clone(), binding);
        }
    }

    fn type_test(&mut self, name: &str, span: Option<&SourceSpan>) -> Option<String> {
        if crate::instruction::ValueType::primitive(name).is_some() {
            return Some(name.into());
        }
        let resolved = self.resolve(name, span)?;
        if self.enums.contains_key(&resolved) || self.variants.contains_key(&resolved) {
            return Some(resolved);
        }
        if self
            .globals
            .get(&resolved)
            .is_some_and(|binding| binding.ty == Type::Poison)
        {
            return None;
        }
        let message = if self.constructors.contains_key(&resolved) {
            "struct type tests require nominal runtime identity and are not implemented".to_owned()
        } else {
            format!("unknown type or enum variant `{name}` in type test")
        };
        self.error("E2001", message, span);
        None
    }

    fn known(&self, name: &str) -> bool {
        self.functions.contains_key(name)
            || self.globals.contains_key(name)
            || self.constructors.contains_key(name)
            || self.variants.contains_key(name)
            || self.enums.contains_key(name)
            || Builtin::from_name(name).is_some()
    }

    fn resolve(&mut self, name: &str, span: Option<&SourceSpan>) -> Option<String> {
        if let Some(target) = self.scopes.iter().rev().find_map(|s| s.imports.get(name)) {
            return Some(target.clone());
        }
        if self.enums.contains_key(name) || self.variants.contains_key(name) {
            return Some(name.to_owned());
        }
        if let Some((prefix, tail)) = name.split_once("::") {
            if self.module.as_deref() == Some(prefix) {
                return Some(name.to_owned());
            }
            if let Some(module) = self.scopes.iter().rev().find_map(|s| s.modules.get(prefix)) {
                return Some(format!("{module}::{tail}"));
            }
            // Enum qualification is a type path, not a module import.
            if self.variants.contains_key(name) {
                return Some(name.to_owned());
            }
            if let Some(module) = &self.module {
                let local = format!("{module}::{name}");
                if self.known(&local) {
                    return Some(local);
                }
            }
            self.error("E1002", format!("undeclared module `{prefix}`"), span);
            return None;
        }
        if let Some(module) = &self.module {
            let local = format!("{module}::{name}");
            if self.known(&local) {
                return Some(local);
            }
        }
        Some(name.to_owned())
    }

    fn import(&mut self, decl: &UseDecl) {
        if decl.namespace {
            let alias = decl
                .aliases
                .first()
                .and_then(|s| s.as_deref())
                .unwrap_or(&decl.module);
            self.scopes
                .last_mut()
                .unwrap()
                .modules
                .insert(alias.to_owned(), decl.module.clone());
        } else if decl.wildcard {
            let prefix = format!("{}::", decl.module);
            let names: Vec<_> = self
                .functions
                .keys()
                .chain(self.globals.keys())
                .chain(self.constructors.keys())
                .chain(self.variants.keys())
                .chain(self.enums.keys())
                .filter_map(|n| {
                    n.strip_prefix(&prefix)
                        .map(|short| (short.to_owned(), n.clone()))
                })
                .collect();
            self.scopes.last_mut().unwrap().imports.extend(names);
            // Builtin wildcard resolution uses the imported namespace as fallback.
            self.scopes
                .last_mut()
                .unwrap()
                .modules
                .insert(decl.module.clone(), decl.module.clone());
        } else {
            for (i, name) in decl.names.iter().enumerate() {
                let alias = decl
                    .aliases
                    .get(i)
                    .and_then(|a| a.as_deref())
                    .unwrap_or(name);
                self.scopes
                    .last_mut()
                    .unwrap()
                    .imports
                    .insert(alias.to_owned(), format!("{}::{name}", decl.module));
            }
        }
    }

    fn defaults(&mut self, fields: &[StructField]) {
        for field in fields {
            if let Some(value) = &field.default {
                let actual = self.expr(value);
                if let Some(ty) = field.ty.clone() {
                    self.assignable(
                        ty,
                        actual,
                        value.span(),
                        &format!("default for `{}`", field.name),
                    );
                } else if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        format!("cannot infer array element type for default `{}`; add a type annotation", field.name),
                        value.span(),
                    );
                }
            }
        }
    }

    fn block(&mut self, block: &Block) -> Type {
        self.scopes.push(Scope::default());
        let mut reaches_tail = true;
        let previous_reachability = self.normal_reachable;
        // Check all source, including unreachable statements. Only reachable
        // normal paths contribute to a block result; return/break/crash do not.
        for stmt in &block.stmts {
            self.normal_reachable = previous_reachability && reaches_tail;
            let result = self.stmt(stmt);
            reaches_tail &= result != Type::Never;
        }
        self.normal_reachable = previous_reachability && reaches_tail;
        let tail = block
            .tail
            .as_ref()
            .map_or(Type::Nil, |tail| self.expr(tail));
        self.normal_reachable = previous_reachability;
        self.scopes.pop();
        if reaches_tail { tail } else { Type::Never }
    }

    fn stmt(&mut self, stmt: &Stmt) -> Type {
        let previous = self.location.clone();
        if let Some(span) = stmt.span() {
            self.location = Some(span.clone());
        }
        let result = self.stmt_inner(stmt);
        self.location = previous;
        result
    }

    fn stmt_inner(&mut self, stmt: &Stmt) -> Type {
        match stmt.node() {
            Stmt::Located(_, _) => unreachable!("node removes locations"),
            Stmt::Invalid { binding } => {
                if let Some(name) = binding {
                    self.define(
                        name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty: Type::Poison,
                            mutable: true,
                        },
                        stmt.span(),
                    );
                }
            }
            Stmt::VarDecl { name, ty, value }
            | Stmt::Const { name, ty, value }
            | Stmt::Comptime { name, ty, value } => {
                let is_comptime_initializer = matches!(stmt.node(), Stmt::Comptime { .. });
                let previous_comptime_initializer = self.in_comptime_initializer;
                self.in_comptime_initializer |= is_comptime_initializer;
                let actual = self.expr(value);
                self.in_comptime_initializer = previous_comptime_initializer;
                let valid = ty.clone().is_none_or(|t| {
                    self.assignable(t, actual.clone(), value.span(), &format!("`{name}`"))
                });
                let mutable = matches!(stmt.node(), Stmt::VarDecl { .. });
                let access_id = stmt.node() as *const Stmt as usize;
                if !valid || actual == Type::Poison {
                    self.access_facts.invalid_bindings.insert(access_id);
                } else {
                    self.access_facts.invalid_bindings.remove(&access_id);
                }
                let id = stmt as *const Stmt as usize;
                let inferred = mutable && ty.is_none() && actual != Type::Poison;
                if ty.is_none()
                    && contains_unresolved_empty_array(&actual)
                    && let Some(span) = value.span().cloned()
                {
                    self.unresolved_empty_arrays
                        .insert(id, (name.clone(), span));
                }
                let binding_type = if !valid || actual == Type::Poison {
                    Type::Poison
                } else if inferred
                    && (actual == Type::ErrorValue || self.dynamic_bindings.contains(&id))
                {
                    Type::Dynamic
                } else {
                    ty.as_ref()
                        .map_or(actual.clone(), |t| self.annotation_type(t))
                };
                self.define(
                    name,
                    Binding {
                        id: 0,
                        error_only: false,
                        ty: binding_type,
                        mutable,
                        inferred: inferred.then_some((
                            id,
                            if actual == Type::ErrorValue {
                                Type::Dynamic
                            } else {
                                actual.clone()
                            },
                        )),
                    },
                    stmt.span(),
                );
                if actual == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::CompoundAssign { target, op, value } => {
                if matches!(op, BinOp::Append | BinOp::Prepend) {
                    self.error(
                        "E1004",
                        "sequence mutation is not a compound assignment; use << / >> or append/prepend",
                        stmt.span(),
                    );
                }
                let left = self.expr(target);
                let right = self.expr(value);
                let actual = self.binary_types(*op, left, right, value.span());
                return self.assignment(target, actual, value.span());
            }
            Stmt::Assign { target, value } => {
                let actual = if matches!(value.node(), Expr::Array(items) if items.is_empty()) {
                    let expected = self.expr(target);
                    if matches!(expected, Type::Array(_)) {
                        expected
                    } else {
                        self.expr(value)
                    }
                } else {
                    self.expr(value)
                };
                return self.assignment(target, actual, value.span());
            }
            Stmt::Use {
                module,
                from,
                names,
                aliases,
                namespace,
                wildcard,
            } => self.import(&UseDecl {
                span: self.location.clone(),
                module: module.clone(),
                from: from.clone(),
                names: names.clone(),
                aliases: aliases.clone(),
                namespace: *namespace,
                wildcard: *wildcard,
            }),
            Stmt::Return(value) => {
                if !self.in_function {
                    self.error(
                        "E1004",
                        "`return` requires an enclosing function body",
                        stmt.span(),
                    );
                }
                if self.in_cleanup {
                    self.error("E1004", "`return` cannot leave a defer block", stmt.span());
                }
                let actual = value.as_ref().map_or(Type::Nil, |expr| self.expr(expr));
                if let Some(expected) = self.result_type.clone() {
                    self.assignable(expected, actual, stmt.span(), "return value");
                } else if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        "cannot infer array element type for an empty return value; add a result type annotation",
                        stmt.span(),
                    );
                }
                return Type::Never;
            }
            Stmt::Expr(expr) => {
                let actual = self.expr(expr);
                if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        "cannot infer array element type for an empty expression; add a type context",
                        expr.span(),
                    );
                }
                if actual == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::Crash { value, .. } => {
                self.expr(value);
                return Type::Never;
            }
            Stmt::Defer { body } => {
                let loops = std::mem::replace(&mut self.loops, 0);
                let in_cleanup = std::mem::replace(&mut self.in_cleanup, true);
                self.block(body);
                self.loops = loops;
                self.in_cleanup = in_cleanup;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                if self.try_blocks(body, catch_name, catch_block) == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                let condition = self.condition(cond);
                let reachable = self.normal_reachable;
                let base_flow = self.flow_facts.clone();
                let then_reachable = reachable && !matches!(cond.node(), Expr::Bool(false));
                self.normal_reachable = then_reachable;
                let a = self.block(then_block);
                let then_flow = self.flow_facts.clone();
                self.flow_facts = base_flow.clone();
                let else_reachable = reachable && !matches!(cond.node(), Expr::Bool(true));
                self.normal_reachable = else_reachable;
                let b = else_block
                    .as_ref()
                    .map_or(Type::Nil, |block| self.block(block));
                let else_flow = self.flow_facts.clone();
                self.normal_reachable = reachable;
                let mut exits = Vec::new();
                if then_reachable && a != Type::Never {
                    exits.push(then_flow);
                }
                if else_reachable && b != Type::Never {
                    exits.push(else_flow);
                }
                if !exits.is_empty() {
                    self.join_flow_exits(exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if condition == Type::Never || a.join(b) == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                let ty = self.expr(value);
                let base_flow = self.flow_facts.clone();
                self.scopes.push(Scope::default());
                self.define(
                    name,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty,
                        mutable: true,
                    },
                    value.span(),
                );
                let then_result = self.block(then_block);
                let then_flow = self.flow_facts.clone();
                self.scopes.pop();
                self.flow_facts = base_flow.clone();
                if let Some(block) = else_block {
                    let else_result = self.block(block);
                    let else_flow = self.flow_facts.clone();
                    let mut exits = Vec::new();
                    if then_result != Type::Never {
                        exits.push(then_flow);
                    }
                    if else_result != Type::Never {
                        exits.push(else_flow);
                    }
                    if !exits.is_empty() {
                        self.join_flow_exits(exits);
                    } else {
                        self.flow_facts = base_flow;
                    }
                } else if then_result != Type::Never {
                    self.join_flow_exits([then_flow, base_flow]);
                } else {
                    self.flow_facts = base_flow;
                }
            }
            Stmt::While { cond, body } => {
                let condition = self.condition(cond);
                let loop_flow = self.flow_facts.clone();
                self.loops += 1;
                self.loop_breaks.push(false);
                self.block(body);
                self.flow_facts = loop_flow;
                self.invalidate_mutable_flow_facts();
                let has_break = self.loop_breaks.pop().unwrap();
                self.loops -= 1;
                if condition == Type::Never
                    || (matches!(cond.node(), Expr::Bool(true)) && !has_break)
                {
                    return Type::Never;
                }
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let a = self.expr(start);
                let b = self.expr(end);
                let step_type = step.as_ref().map(|step| self.expr(step));
                self.scopes.push(Scope::default());
                self.define(
                    var,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty: a.clone().join(b.clone()),
                        mutable: true,
                    },
                    start.span(),
                );
                let loop_flow = self.flow_facts.clone();
                self.loops += 1;
                self.loop_breaks.push(false);
                self.block(body);
                self.flow_facts = loop_flow;
                self.invalidate_mutable_flow_facts();
                self.loop_breaks.pop();
                self.loops -= 1;
                self.scopes.pop();
                if a == Type::Never || b == Type::Never || step_type == Some(Type::Never) {
                    return Type::Never;
                }
            }
            Stmt::ForEach {
                var,
                iterable,
                body,
            } => {
                let collection = self.expr(iterable);
                if self.in_comptime {
                    self.error(
                        "E4002",
                        "collection iteration is available only in runtime functions",
                        stmt.span(),
                    );
                }
                let element = match &collection {
                    Type::Array(element) => (**element).clone(),
                    Type::EmptyArray | Type::Dynamic => Type::Dynamic,
                    Type::Primitive(VarType::Str, _) => Type::concrete(VarType::Str),
                    Type::ErrorValue | Type::Never => Type::Never,
                    Type::Poison => Type::Poison,
                    _ => {
                        self.error(
                            "E2001",
                            format!(
                                "for-in requires an array or str, got {}",
                                collection.display()
                            ),
                            iterable.span(),
                        );
                        Type::Poison
                    }
                };
                self.scopes.push(Scope::default());
                self.define(
                    var,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty: element,
                        mutable: true,
                    },
                    iterable.span(),
                );
                let loop_flow = self.flow_facts.clone();
                self.loops += 1;
                self.loop_breaks.push(false);
                self.block(body);
                self.flow_facts = loop_flow;
                self.invalidate_mutable_flow_facts();
                self.loop_breaks.pop();
                self.loops -= 1;
                self.scopes.pop();
                if collection == Type::Never || collection == Type::ErrorValue {
                    return Type::Never;
                }
            }
            Stmt::Break if self.loops == 0 => {
                self.error("E1004", "`break` used outside of a loop", stmt.span())
            }
            Stmt::Break => {
                if self.normal_reachable {
                    *self.loop_breaks.last_mut().unwrap() = true;
                }
                return Type::Never;
            }
            Stmt::Continue if self.loops == 0 => {
                self.error("E1005", "`continue` used outside of a loop", stmt.span())
            }
            Stmt::Continue => return Type::Never,
        }
        Type::Nil
    }

    fn binary_types(&mut self, op: BinOp, a: Type, b: Type, span: Option<&SourceSpan>) -> Type {
        if a == Type::Poison || b == Type::Poison {
            return Type::Poison;
        }
        if a == Type::Never || b == Type::Never {
            return Type::Never;
        }
        if a == Type::ErrorValue || b == Type::ErrorValue {
            return Type::ErrorValue;
        }
        if matches!(op, BinOp::Eq | BinOp::NotEq) {
            return Type::concrete(VarType::Bool);
        }
        if a == Type::Dynamic || b == Type::Dynamic {
            return Type::Dynamic;
        }
        match (op, a.clone(), b.clone()) {
            (BinOp::Append, Type::EmptyArray, element) => Type::Array(Box::new(element)),
            (BinOp::Prepend, element, Type::EmptyArray) => Type::Array(Box::new(element)),
            (BinOp::Append, Type::Array(a), b) if type_compatible(&a, &b) => Type::Array(a),
            (BinOp::Prepend, a, Type::Array(b)) if type_compatible(&b, &a) => Type::Array(b),
            (BinOp::Add, Type::Array(a), Type::Array(b)) if a == b => Type::Array(a),
            (BinOp::Append, Type::Array(element), actual) if *element != Type::Dynamic => {
                self.error(
                    "E2001",
                    format!(
                        "type mismatch: array element is {} but appended value is {}",
                        element.display(),
                        actual.display()
                    ),
                    span,
                );
                Type::Poison
            }
            (BinOp::Prepend, actual, Type::Array(element)) if *element != Type::Dynamic => {
                self.error(
                    "E2001",
                    format!(
                        "type mismatch: array element is {} but prepended value is {}",
                        element.display(),
                        actual.display()
                    ),
                    span,
                );
                Type::Poison
            }
            (
                BinOp::Add,
                Type::Primitive(VarType::Str, false),
                Type::Primitive(VarType::Str, false),
            ) => Type::concrete(VarType::Str),
            (
                BinOp::And | BinOp::Or | BinOp::Xor,
                Type::Primitive(VarType::Bool, false),
                Type::Primitive(VarType::Bool, false),
            ) => Type::concrete(VarType::Bool),
            (
                BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Mod
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq,
                Type::Primitive(x @ (VarType::Int | VarType::Float), false),
                Type::Primitive(y @ (VarType::Int | VarType::Float), false),
            ) => {
                if matches!(op, BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq) {
                    Type::concrete(VarType::Bool)
                } else if x == VarType::Float || y == VarType::Float {
                    Type::concrete(VarType::Float)
                } else {
                    Type::concrete(VarType::Int)
                }
            }
            _ => {
                self.error(
                    "E2002",
                    format!(
                        "invalid operands {} and {} for {op:?}",
                        a.display(),
                        b.display()
                    ),
                    span,
                );
                Type::Poison
            }
        }
    }

    fn assignment(&mut self, target: &Expr, actual: Type, span: Option<&SourceSpan>) -> Type {
        match target.node() {
            Expr::Ident(name) => {
                let binding = self.local(name).or_else(|| {
                    self.resolve(name, target.span()).and_then(|n| {
                        self.access_facts
                            .globals
                            .insert(crate::mutability::expr_id(target), n.clone());
                        self.globals.get(&n).cloned()
                    })
                });
                if let Some(binding) = binding {
                    // Writes invalidate pattern knowledge, including
                    // overlays in enclosing arms. Do not infer new facts
                    // from conditional writes without a CFG join.
                    if binding.mutable && self.normal_reachable {
                        self.flow_facts.forget(binding.id);
                        for scope in &mut self.scopes {
                            for entry in scope.bindings.values_mut() {
                                if entry.id == binding.id {
                                    entry.error_only = false;
                                }
                            }
                        }
                    }
                    if !binding.mutable {
                        self.error(
                            "E1003",
                            format!("cannot assign to const `{name}`"),
                            target.span(),
                        );
                    }
                    if let Some((id, initial)) = binding.inferred {
                        if actual == Type::Dynamic {
                            self.dynamic_bindings.insert(id);
                        } else if initial != Type::Dynamic {
                            self.assignable_type(
                                initial,
                                actual.clone(),
                                span,
                                &format!("`{name}`"),
                            );
                        }
                    } else {
                        self.assignable_type(
                            binding.ty,
                            actual.clone(),
                            span,
                            &format!("`{name}`"),
                        );
                    }
                } else {
                    self.error(
                        "E1002",
                        format!("cannot assign to undefined variable `{name}`"),
                        target.span(),
                    );
                }
            }
            Expr::Index(receiver, _) => {
                if matches!(self.expr(receiver), Type::Enum(_, _) | Type::ErrorValue) {
                    self.error(
                        "E1003",
                        "enum payload bindings are immutable",
                        target.span(),
                    );
                }
                let expected = self.expr(target);
                self.assignable_type(expected, actual.clone(), span, "field");
            }
            _ => self.error(
                "E1003",
                "invalid assignment target: expected a variable or indexed expression",
                target.span(),
            ),
        }
        if actual == Type::Never {
            Type::Never
        } else {
            Type::Nil
        }
    }

    fn condition(&mut self, expr: &Expr) -> Type {
        let actual = self.expr(expr);
        // Dynamic conditions remain runtime checked; a statically invalid condition is rejected.
        if !matches!(actual, Type::Dynamic | Type::ErrorValue | Type::Poison) {
            self.assignable(
                TypeAnnotation {
                    kind: TypeKind::Primitive(VarType::Bool),
                    nilable: false,
                },
                actual.clone(),
                expr.span(),
                "condition",
            );
        }
        actual
    }

    fn try_blocks(&mut self, body: &Block, name: &str, catch: &Block) -> Type {
        let base_flow = self.flow_facts.clone();
        let success = self.block(body);
        let success_flow = self.flow_facts.clone();
        self.flow_facts = base_flow;
        // A failure may happen after the body has performed arbitrary writes.
        self.invalidate_mutable_flow_facts();
        self.scopes.push(Scope::default());
        self.define(
            name,
            Binding {
                id: 0,
                error_only: false,
                inferred: None,
                ty: Type::ErrorValue,
                mutable: false,
            },
            None,
        );
        let failure = self.block(catch);
        let failure_flow = self.flow_facts.clone();
        self.scopes.pop();
        let exits = [
            (success != Type::Never).then_some(success_flow),
            (failure != Type::Never).then_some(failure_flow),
        ]
        .into_iter()
        .flatten();
        self.join_flow_exits(exits);
        success.join(failure)
    }

    fn expr(&mut self, expr: &Expr) -> Type {
        let previous = self.location.clone();
        if let Some(span) = expr.span() {
            self.location = Some(span.clone());
        }
        let result = self.expr_inner(expr);
        use crate::mutability::expr_id;
        c_record_kind(&mut self.access_facts, expr_id(expr), result.clone());
        self.location = previous;
        result
    }

    fn expr_inner(&mut self, expr: &Expr) -> Type {
        match expr.node() {
            Expr::BracketApply(_, _) | Expr::Apply(_, _) => {
                self.error(
                    "E1003",
                    "resolve brackets and callable names before semantic analysis",
                    expr.span(),
                );
                Type::Poison
            }
            Expr::Located(_, _) => unreachable!("node removes locations"),
            Expr::Int(_) => Type::concrete(VarType::Int),
            Expr::Float(_) => Type::concrete(VarType::Float),
            Expr::Bool(_) => Type::concrete(VarType::Bool),
            Expr::Str(_) => Type::concrete(VarType::Str),
            Expr::Bytes(_) => Type::Bytes,
            Expr::Interpolated(parts) => {
                let mut invalid = false;
                let mut never = false;
                for part in parts {
                    if let StringPart::Expression(value) = part {
                        let ty = self.expr(value);
                        invalid |= ty == Type::Poison;
                        never |= ty == Type::Never;
                    }
                }
                if invalid {
                    Type::Poison
                } else if never {
                    Type::Never
                } else {
                    Type::concrete(VarType::Str)
                }
            }
            Expr::Nil => Type::Nil,
            Expr::TypeLiteral(_) => {
                self.error(
                    "E4002",
                    "type values are only valid in compile-time type bindings",
                    expr.span(),
                );
                Type::Poison
            }
            Expr::Ident(name) => {
                if let Some(binding) = self.local(name) {
                    return self.effective_type(&binding);
                }
                let Some(resolved) = self.resolve(name, expr.span()) else {
                    return Type::Poison;
                };
                if let Some(binding) = self.globals.get(&resolved) {
                    self.access_facts
                        .globals
                        .insert(crate::mutability::expr_id(expr), resolved);
                    return self.effective_type(binding);
                }
                if let Some(function) = self.functions.get(&resolved) {
                    let function_name = function.name.clone();
                    let comptime = function.comptime;
                    if comptime != self.in_comptime {
                        let (caller_phase, callee_phase) = if self.in_comptime {
                            ("comptime", "runtime")
                        } else {
                            ("runtime", "comptime")
                        };
                        self.error(
                            "E4002",
                            format!(
                                "{caller_phase} code cannot use {callee_phase}-only function `{function_name}` as a value"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    self.error(
                        "E4002",
                        format!(
                            "function values are not supported; call `{function_name}` directly"
                        ),
                        expr.span(),
                    );
                    return Type::Poison;
                }
                self.error("E1002", format!("undefined variable `{name}`"), expr.span());
                Type::Poison
            }
            Expr::Array(items) => {
                let mut never = false;
                let mut element: Option<Type> = None;
                let mut inconsistent = false;
                for item in items {
                    let ty = self.expr(item);
                    never |= ty == Type::Never;
                    if ty != Type::Never && ty != Type::ErrorValue {
                        if let Some(previous) = &element
                            && previous.clone().join(ty.clone()) == Type::Dynamic
                            && *previous != Type::Dynamic
                            && ty != Type::Dynamic
                        {
                            inconsistent = true;
                        }
                        element = Some(element.map_or(ty.clone(), |current| current.join(ty)));
                    }
                }
                if inconsistent {
                    self.error(
                        "E2001",
                        "type mismatch: array elements must have a consistent type",
                        expr.span(),
                    );
                    return Type::Poison;
                }
                if never {
                    Type::Never
                } else if items.is_empty() {
                    Type::EmptyArray
                } else {
                    Type::Array(Box::new(element.unwrap_or(Type::Dynamic)))
                }
            }
            Expr::Tuple(items) => {
                let types: Vec<_> = items.iter().map(|item| self.expr(item)).collect();
                if types.contains(&Type::Never) {
                    Type::Never
                } else {
                    Type::Tuple(types)
                }
            }
            Expr::Record(fields) => {
                let mut never = false;
                for (_, value) in fields {
                    never |= self.expr(value) == Type::Never;
                }
                if never { Type::Never } else { Type::Record }
            }
            Expr::Cast(inner, target) => match self.expr(inner) {
                Type::Poison => Type::Poison,
                Type::Never => Type::Never,
                _ => Type::concrete(*target),
            },
            Expr::Unary(op, inner) => {
                let ty = self.expr(inner);
                match (op, ty.clone()) {
                    (_, Type::Poison | Type::ErrorValue | Type::Dynamic | Type::Never) => {
                        ty.clone()
                    }
                    (UnOp::Neg, Type::Primitive(VarType::Int | VarType::Float, false)) => {
                        ty.clone()
                    }
                    (UnOp::Not, Type::Primitive(VarType::Bool, false)) => ty.clone(),
                    _ => {
                        self.error(
                            "E2002",
                            format!("invalid operand {} for {op:?}", ty.display()),
                            expr.span(),
                        );
                        Type::Poison
                    }
                }
            }
            Expr::Propagate(inner) => {
                if !self.in_function || (self.in_comptime_initializer && !self.in_comptime) {
                    self.error(
                        "E1004",
                        "`!!` requires an enclosing callable body; a runtime comptime initializer is not a callable",
                        expr.span(),
                    );
                }
                match self.expr(inner) {
                    Type::ErrorValue => Type::Never,
                    ty => ty,
                }
            }
            Expr::Fallback {
                kind,
                value,
                fallback,
            } => self.fallback(*kind, value, fallback, expr.span()),
            Expr::Binary(op, a, b) => {
                let left = self.expr(a);
                let right = self.expr(b);
                if matches!(op, BinOp::Append) {
                    self.refine_empty_array(a, &right);
                }
                if matches!(op, BinOp::Prepend) {
                    self.refine_empty_array(b, &left);
                }
                self.binary_types(*op, left, right, expr.span())
            }
            Expr::Index(a, b) => {
                let receiver = self.expr(a);
                let index = self.expr(b);
                if receiver == Type::Poison || index == Type::Poison {
                    return Type::Poison;
                }
                if receiver == Type::Never || index == Type::Never {
                    return Type::Never;
                }
                if let (Type::Struct(id, struct_name, type_arguments), Expr::Str(name)) =
                    (&receiver, b.node())
                {
                    if let Some(field) = self.struct_fields[*id]
                        .iter()
                        .find(|field| &field.name == name)
                    {
                        if field.visibility == Visibility::Private && !self.owns_struct(*id) {
                            self.error("E1002", format!("field `{name}` is private"), b.span());
                            return Type::Poison;
                        }
                        return field.ty.as_ref().map_or(Type::Dynamic, |field_type| {
                            let parameters = self
                                .struct_parameters
                                .get(struct_name)
                                .cloned()
                                .unwrap_or_default();
                            let substitutions: HashMap<_, _> = parameters
                                .iter()
                                .zip(type_arguments)
                                .map(|(parameter, argument)| {
                                    (parameter.name.clone(), argument.clone())
                                })
                                .collect();
                            self.substituted_type(field_type, &substitutions)
                        });
                    }
                    self.error("E1002", format!("unknown field `{name}`"), b.span());
                    return Type::Poison;
                }
                match (&receiver, b.node()) {
                    (Type::Array(element), _) => return (**element).clone(),
                    (Type::EmptyArray, _) => return Type::Dynamic,
                    (Type::Tuple(items), Expr::Int(index)) if *index >= 0 => {
                        if let Some(item) = items.get(*index as usize) {
                            return item.clone();
                        }
                        self.error(
                            "E2001",
                            format!(
                                "tuple position {index} is out of range for {} elements",
                                items.len()
                            ),
                            b.span(),
                        );
                        return Type::Poison;
                    }
                    (Type::Tuple(_), _) => {
                        self.error(
                            "E2001",
                            "tuple access requires a numeric constant position",
                            b.span(),
                        );
                        return Type::Poison;
                    }
                    _ => {}
                }
                Type::Dynamic
            }
            Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                self.call(name, args, expr)
            }
            Expr::MethodCall(receiver_expr, method, args) => {
                let receiver = self.expr(receiver_expr);
                let mut poisoned = receiver == Type::Poison;
                let mut never = receiver == Type::Never;
                let mut argument_types = Vec::with_capacity(args.len());
                for arg in args {
                    let ty = self.expr(arg);
                    poisoned |= ty == Type::Poison;
                    never |= ty == Type::Never;
                    argument_types.push(ty);
                }
                self.invalidate_mutable_flow_facts();
                let exact_method = self
                    .extension_methods
                    .get(&(receiver.clone(), method.clone()))
                    .copied();
                let mut generic_candidates: Vec<_> = self
                    .generic_extension_methods
                    .iter()
                    .filter_map(|(extension, extension_method)| {
                        (extension_method.name == *method
                            && generic_extension_matches(extension, &receiver))
                        .then_some(*extension_method)
                    })
                    .collect();
                let ambiguous_generic_method = generic_candidates.len() > 1;
                let generic_method = generic_candidates.first().copied();
                generic_candidates.clear();
                if exact_method.is_none() && ambiguous_generic_method {
                    self.error(
                        "E2001",
                        format!("method `{method}()` has multiple applicable generic extensions"),
                        expr.span(),
                    );
                    return Type::Poison;
                }
                if let Some(extension_method) = exact_method.or(generic_method) {
                    let function = &extension_method.function;
                    if function.comptime && !self.in_comptime {
                        self.error(
                            "E4002",
                            format!(
                                "runtime code cannot call comptime-only extension method `{method}()`"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if self.in_comptime && !function.comptime {
                        self.error(
                            "E4002",
                            format!(
                                "comptime code cannot call runtime extension method `{method}()`"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if poisoned {
                        return Type::Poison;
                    }
                    if never {
                        return Type::Never;
                    }
                    self.access_facts
                        .calls
                        .insert(crate::mutability::expr_id(expr), function.name.clone());
                    if extension_method.mutating_receiver {
                        self.access_facts
                            .mutating_calls
                            .insert(crate::mutability::expr_id(expr));
                    }
                    if !function.type_params.is_empty() {
                        let mut call_arguments = Vec::with_capacity(args.len() + 1);
                        call_arguments.push(CallArgument::positional((**receiver_expr).clone()));
                        call_arguments.extend(args.iter().cloned().map(CallArgument::positional));
                        let mut actual_types = Vec::with_capacity(argument_types.len() + 1);
                        actual_types.push(receiver.clone());
                        actual_types.extend(argument_types.iter().cloned());
                        return self.generic_call(
                            &function.name,
                            function,
                            &call_arguments,
                            actual_types,
                            expr,
                        );
                    }
                    let expected = function.params.len().saturating_sub(1);
                    if args.len() != expected {
                        self.error(
                            "E1005",
                            format!(
                                "method `{method}()` expects {expected} argument(s), got {}",
                                args.len()
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if let Some(receiver_param) = function.params.first()
                        && let Some(ty) = &receiver_param.ty
                    {
                        self.assignable(
                            ty.clone(),
                            receiver.clone(),
                            receiver_expr.span(),
                            "method receiver",
                        );
                    }
                    for ((argument, actual), parameter) in args
                        .iter()
                        .zip(argument_types.iter())
                        .zip(function.params.iter().skip(1))
                    {
                        if let Some(expected) = &parameter.ty {
                            self.assignable(
                                expected.clone(),
                                actual.clone(),
                                argument.span(),
                                &format!("argument `{}` of `{method}()`", parameter.name),
                            );
                        }
                    }
                    self.method_calls.insert(
                        expression_site(self.current_function.as_deref(), expr),
                        function.name.clone(),
                    );
                    return function
                        .result_type
                        .as_ref()
                        .map_or(Type::Dynamic, |ty| self.annotation_type(ty));
                }
                if method == "clone"
                    && matches!(receiver, Type::Array(_) | Type::Tuple(_))
                    && self.implements_trait(&receiver, CLONE_TRAIT, &[])
                {
                    if !args.is_empty() {
                        self.error(
                            "E1005",
                            format!("method `clone()` expects 0 argument(s), got {}", args.len()),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if poisoned {
                        return Type::Poison;
                    }
                    if never {
                        return Type::Never;
                    }
                    self.access_facts
                        .structural_clones
                        .insert(crate::mutability::expr_id(expr));
                    self.method_calls.insert(
                        expression_site(self.current_function.as_deref(), expr),
                        "$vm::structural_clone".to_owned(),
                    );
                    return receiver;
                }
                let expected = match method.as_str() {
                    "expect" | "append" | "prepend" => 1,
                    "len" | "to_int" | "to_float" | "to_bool" | "to_str" | "error_kind"
                    | "unwrap" | "empty_like" => 0,
                    _ => {
                        self.error("E1002", format!("unknown method `{method}()`"), expr.span());
                        return Type::Poison;
                    }
                };
                if args.len() != expected {
                    self.error(
                        "E1005",
                        format!(
                            "method `{method}()` expects {expected} argument(s), got {}",
                            args.len()
                        ),
                        expr.span(),
                    );
                    return Type::Poison;
                }
                if poisoned {
                    return Type::Poison;
                }
                if never {
                    return Type::Never;
                }
                match method.as_str() {
                    "empty_like" => match receiver {
                        Type::Array(_) | Type::Dynamic | Type::ErrorValue => receiver,
                        _ => {
                            self.error(
                                "E2001",
                                "empty_like() requires an array receiver",
                                receiver_expr.span(),
                            );
                            Type::Poison
                        }
                    },
                    "append" | "prepend" => {
                        if !matches!(
                            receiver,
                            Type::Array(_) | Type::EmptyArray | Type::Dynamic | Type::ErrorValue
                        ) {
                            self.error(
                                "E2001",
                                "array mutation requires an array receiver",
                                expr.span(),
                            );
                            Type::Poison
                        } else {
                            if let (Type::Array(element), Some(actual)) =
                                (&receiver, argument_types.first())
                            {
                                self.assignable_type(
                                    (**element).clone(),
                                    actual.clone(),
                                    args[0].span(),
                                    "array element",
                                );
                                if **element == Type::Dynamic {
                                    self.refine_empty_array(receiver_expr, actual);
                                }
                            }
                            if receiver == Type::EmptyArray
                                && let Some(actual) = argument_types.first()
                            {
                                self.refine_empty_array(receiver_expr, actual);
                                return Type::Array(Box::new(actual.clone()));
                            }
                            receiver
                        }
                    }
                    "to_int" | "len" => Type::concrete(VarType::Int),
                    "to_float" => Type::concrete(VarType::Float),
                    "to_bool" | "exclusive" => Type::concrete(VarType::Bool),
                    "to_str" | "error_kind" => Type::concrete(VarType::Str),
                    "unwrap" | "expect" => receiver,
                    _ => Type::Dynamic,
                }
            }
            Expr::Block(block) => self.block(block),
            Expr::Match {
                value,
                arms,
                exhaustive,
            } => {
                let actual = self.expr(value);
                let mut result = None;
                let mut covered = std::collections::HashSet::new();
                let mut wildcard = false;
                let base_flow = self.flow_facts.clone();
                let mut unmatched_flow = Some(base_flow.clone());
                let mut normal_exits = Vec::new();
                for arm in arms {
                    let arm_flow = unmatched_flow.clone().unwrap_or_else(|| base_flow.clone());
                    self.flow_facts = arm_flow.clone();
                    let previous_reachability = self.normal_reachable;
                    let arm_reachable = unmatched_flow.is_some();
                    self.normal_reachable &= arm_reachable;
                    // Separate pattern facts from payload bindings so a payload
                    // can shadow the scrutinee name without a duplicate binding.
                    self.scopes.push(Scope::default());
                    self.scopes.push(Scope::default());
                    let mut tested_type = None;
                    match &arm.pattern {
                        MatchPattern::Wildcard => wildcard |= arm.guard.is_none(),
                        MatchPattern::StringLiteral(_) => {}
                        MatchPattern::Type(name) => {
                            let tested = self.type_test(name, expr.span());
                            tested_type.clone_from(&tested);
                            if tested.as_deref().is_some_and(|name| {
                                name == crate::error::ERROR_TYPE
                                    || name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                            }) {
                                self.refine_error_binding(value);
                            }
                            if let Some(name) = &tested {
                                self.apply_type_pattern(value, name, true);
                            }
                            if let Some(name) = tested
                                && arm.guard.is_none()
                            {
                                if let Some(id) = self.enums.get(&name) {
                                    covered.extend(self.enum_variants[*id].iter().cloned());
                                } else if self.variants.contains_key(&name) {
                                    covered.insert(name);
                                }
                            }
                        }
                        MatchPattern::Variant { name, bindings } => {
                            let resolved = self.resolve(name, expr.span());
                            let variant = resolved
                                .as_ref()
                                .and_then(|name| self.variants.get(name))
                                .copied();
                            if let Some((id, fields)) = variant {
                                if resolved.as_deref().is_some_and(|name| {
                                    name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                                }) {
                                    self.refine_error_binding(value);
                                }
                                let error_pattern = resolved.as_deref().is_some_and(|name| {
                                    name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                                });
                                if error_pattern && let Some(name) = resolved.as_deref() {
                                    self.apply_type_pattern(value, name, true);
                                }
                                if !error_pattern
                                    && matches!(actual, Type::Enum(other, _) if other != id)
                                {
                                    self.error(
                                        "E2001",
                                        "pattern belongs to a different enum",
                                        expr.span(),
                                    );
                                }
                                if fields.len() != bindings.len() {
                                    self.error(
                                        "E1005",
                                        format!(
                                            "pattern `{name}` expects {} binding(s), got {}",
                                            fields.len(),
                                            bindings.len()
                                        ),
                                        expr.span(),
                                    );
                                }
                                if arm.guard.is_none() {
                                    covered.insert(resolved.unwrap());
                                }
                                for (i, binding) in bindings.iter().enumerate() {
                                    if binding != "_" {
                                        let ty = fields
                                            .get(i)
                                            .and_then(|field| field.ty.as_ref())
                                            .map_or(Type::Dynamic, |t| self.annotation_type(t));
                                        self.define(
                                            binding,
                                            Binding {
                                                id: 0,
                                                error_only: false,
                                                inferred: None,
                                                ty,
                                                mutable: false,
                                            },
                                            expr.span(),
                                        );
                                    }
                                }
                            } else {
                                if !resolved.as_deref().is_some_and(|name| {
                                    self.globals.get(name).is_some_and(|b| b.ty == Type::Poison)
                                }) {
                                    self.error(
                                        "E1002",
                                        format!("unknown enum variant `{name}` in pattern"),
                                        expr.span(),
                                    );
                                }
                                for binding in bindings {
                                    if binding != "_" {
                                        self.define(
                                            binding,
                                            Binding {
                                                id: 0,
                                                error_only: false,
                                                inferred: None,
                                                ty: Type::Poison,
                                                mutable: false,
                                            },
                                            expr.span(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if let Some(guard) = &arm.guard {
                        self.condition(guard);
                    }
                    let guard_flow = self.flow_facts.clone();
                    let ty = self.block(&arm.body);
                    if arm_reachable && ty != Type::Never {
                        normal_exits.push(self.flow_facts.clone());
                    }
                    result = Some(result.map_or(ty.clone(), |current: Type| current.join(ty)));
                    self.scopes.pop();
                    self.scopes.pop();
                    self.normal_reachable = previous_reachability;

                    if !arm_reachable {
                        unmatched_flow = None;
                    } else if arm.guard.is_some() {
                        // A guard can fail even after the type test succeeds.
                        unmatched_flow = Some(guard_flow);
                    } else {
                        match &arm.pattern {
                            MatchPattern::Wildcard => unmatched_flow = None,
                            MatchPattern::StringLiteral(_) => {}
                            MatchPattern::Type(_) => {
                                let mut next = arm_flow;
                                if let Some(name) = tested_type.as_deref() {
                                    self.flow_facts = next.clone();
                                    self.apply_type_pattern(value, name, false);
                                    next = self.flow_facts.clone();
                                }
                                unmatched_flow = Some(next);
                            }
                            MatchPattern::Variant { .. } => {
                                unmatched_flow = Some(arm_flow);
                            }
                        }
                    }
                }
                if *exhaustive && !wildcard && actual != Type::Poison {
                    let enum_id = match actual {
                        Type::Enum(id, _) => Some(id),
                        Type::ErrorValue => self
                            .variants
                            .get(&format!("{}::DivisionByZero", crate::error::ERROR_TYPE))
                            .map(|(id, _)| *id),
                        _ => None,
                    };
                    let complete = enum_id.is_some_and(|id| {
                        self.enum_variants[id]
                            .iter()
                            .all(|name| covered.contains(name))
                    });
                    if !complete {
                        self.error(
                            "E2003",
                            "non-exhaustive match: add missing variants or a `_` arm",
                            expr.span(),
                        );
                    } else {
                        unmatched_flow = None;
                    }
                }
                if let Some(unmatched) = unmatched_flow {
                    normal_exits.push(unmatched);
                }
                if !normal_exits.is_empty() {
                    self.join_flow_exits(normal_exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if actual == Type::Never {
                    return Type::Never;
                }
                let result = result.unwrap_or(Type::Nil);
                if !*exhaustive && !wildcard {
                    result.join(Type::Nil)
                } else {
                    result
                }
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => self.try_blocks(body, catch_name, catch_block),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let condition = self.condition(cond);
                let reachable = self.normal_reachable;
                let base_flow = self.flow_facts.clone();
                let then_reachable = reachable && !matches!(cond.node(), Expr::Bool(false));
                self.normal_reachable = then_reachable;
                let a = self.expr(then_branch);
                let then_flow = self.flow_facts.clone();
                self.flow_facts = base_flow.clone();
                let else_reachable = reachable && !matches!(cond.node(), Expr::Bool(true));
                self.normal_reachable = else_reachable;
                let b = else_branch.as_ref().map_or(Type::Nil, |b| self.expr(b));
                let else_flow = self.flow_facts.clone();
                self.normal_reachable = reachable;
                let mut exits = Vec::new();
                if then_reachable && a != Type::Never {
                    exits.push(then_flow);
                }
                if else_reachable && b != Type::Never {
                    exits.push(else_flow);
                }
                if !exits.is_empty() {
                    self.join_flow_exits(exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if condition == Type::Never
                    || (a == Type::Never && matches!(cond.node(), Expr::Bool(true)))
                    || (b == Type::Never && matches!(cond.node(), Expr::Bool(false)))
                {
                    Type::Never
                } else {
                    a.join(b)
                }
            }
        }
    }

    fn fallback(
        &mut self,
        kind: FallbackKind,
        value: &Expr,
        fallback: &Expr,
        span: Option<&SourceSpan>,
    ) -> Type {
        let lhs = self.expr(value);
        let alternative = match (kind, lhs.clone()) {
            (FallbackKind::Nil, Type::Nil) => Type::Never,
            (FallbackKind::Nil, Type::Primitive(VarType::Nil, _)) => Type::Never,
            (FallbackKind::Nil, Type::Primitive(t, _)) => Type::Primitive(t, false),
            (FallbackKind::Error, Type::ErrorValue) => Type::Never,
            _ => lhs.clone(),
        };
        let can_fallback = match kind {
            FallbackKind::Nil => lhs == Type::Dynamic || alternative != lhs,
            FallbackKind::Error => {
                lhs != Type::Never
                    && !matches!(
                        value.node(),
                        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Str(_) | Expr::Nil
                    )
            }
        };
        let reachable = self.normal_reachable;
        self.normal_reachable = reachable && can_fallback && lhs != Type::Never;
        self.access_facts
            .fallback_possible
            .insert(crate::mutability::expr_id(fallback), can_fallback);
        // A skipped RHS is still checked for static violations.
        let rhs = self.expr(fallback);
        self.normal_reachable = reachable;
        if lhs == Type::Poison || rhs == Type::Poison {
            return Type::Poison;
        }
        if !can_fallback {
            return lhs;
        }
        let result = alternative.clone().join(rhs.clone());
        if result == Type::Dynamic && alternative != Type::Dynamic && rhs != Type::Dynamic {
            self.error(
                "E2001",
                format!(
                    "type mismatch: incompatible normal fallback types {} and {}",
                    alternative.display(),
                    rhs.display()
                ),
                span,
            );
            Type::Poison
        } else {
            result
        }
    }

    fn call(&mut self, name: &str, args: &[CallArgument], expr: &Expr) -> Type {
        let before = self.diagnostics.len();
        let actual: Vec<_> = args.iter().map(|arg| self.expr(&arg.value)).collect();
        let poisoned = actual.contains(&Type::Poison);
        let never = actual.contains(&Type::Never);
        let result = self.call_inner(name, args, actual, expr);
        if poisoned || self.diagnostics.len() > before {
            Type::Poison
        } else if never {
            Type::Never
        } else {
            result
        }
    }

    fn call_inner(
        &mut self,
        name: &str,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        // Without effect summaries, any call may mutate a captured local or a
        // global. Drop refinements for mutable bindings before checking what
        // follows the call; immutable scalar bindings keep their facts.
        self.invalidate_mutable_flow_facts();
        let span = expr.span();
        let Some(resolved) = self.resolve(name, span) else {
            return Type::Poison;
        };
        if let Some((id, fields)) = self.constructors.get(&resolved).copied()
            && fields
                .iter()
                .any(|field| field.visibility == Visibility::Private)
            && !self.owns_struct(id)
        {
            self.error(
                "E1002",
                format!("cannot construct `{name}` outside its defining module because it has private fields"),
                span,
            );
            return Type::Poison;
        }
        self.access_facts
            .calls
            .insert(crate::mutability::expr_id(expr), resolved.clone());
        if self
            .globals
            .get(&resolved)
            .is_some_and(|b| b.ty == Type::Poison)
        {
            return Type::Poison;
        }
        if let Some((is_comptime, function_name)) = self
            .functions
            .get(&resolved)
            .map(|function| (function.comptime, function.name.clone()))
            && is_comptime != self.in_comptime
        {
            let (caller_phase, callee_phase) = if self.in_comptime {
                ("comptime", "runtime")
            } else {
                ("runtime", "comptime")
            };
            self.error(
                "E4002",
                format!(
                    "{caller_phase} code cannot call {callee_phase}-only function `{function_name}`"
                ),
                span,
            );
            return Type::Poison;
        }
        if let Some(function) = self
            .functions
            .get(&resolved)
            .filter(|function| !function.type_params.is_empty())
            .cloned()
        {
            return self.generic_call(&resolved, function, args, actual, expr);
        }
        if self
            .struct_parameters
            .get(&resolved)
            .is_some_and(|parameters| !parameters.is_empty())
        {
            return self.generic_struct_call(&resolved, args, actual, expr);
        }
        let fields = self
            .functions
            .get(&resolved)
            .map(|f| f.params.as_slice())
            .or_else(|| self.constructors.get(&resolved).map(|(_, fields)| *fields))
            .or_else(|| self.variants.get(&resolved).map(|(_, fields)| *fields));
        if let Some(fields) = fields {
            let bindings = crate::arguments::bind(name, fields, args);
            for error in bindings.errors {
                self.error(
                    "E1005",
                    error.message,
                    error.argument.and_then(|i| args[i].value.span()).or(span),
                );
            }
            for (i, actual) in actual.into_iter().enumerate() {
                if let Some(index) = bindings.by_argument[i] {
                    if let Some(ty) = fields[index].ty.clone() {
                        if actual == Type::EmptyArray
                            && let Type::Array(element) = self.annotation_type(&ty)
                        {
                            self.refine_empty_array(&args[i].value, &element);
                        }
                        self.assignable(
                            ty,
                            actual,
                            args[i].value.span(),
                            &format!("parameter `{}` of `{name}`", fields[index].name),
                        );
                    } else if contains_unresolved_empty_array(&actual) {
                        self.error(
                            "E2001",
                            "cannot infer array element type for an empty argument to a dynamic parameter; declare a typed array first",
                            args[i].value.span(),
                        );
                    }
                }
            }
            return if let Some((id, _)) = self.constructors.get(&resolved) {
                Type::Struct(*id, resolved.clone(), Vec::new())
            } else if let Some((id, _)) = self.variants.get(&resolved) {
                if resolved.starts_with(&format!("{}::", crate::error::ERROR_TYPE)) {
                    Type::ErrorValue
                } else {
                    Type::Enum(*id, resolved.clone())
                }
            } else {
                self.functions
                    .get(&resolved)
                    .and_then(|function| function.result_type.as_ref())
                    .map_or(Type::Dynamic, |t| self.annotation_type(t))
            };
        }
        if let Some(builtin) = Builtin::from_name(&resolved) {
            if matches!(builtin, Builtin::BuildInput) {
                if !self.in_comptime {
                    self.error("E4002", format!("`{resolved}` requires a comptime-known path and is only available at comptime"), span);
                    return Type::Poison;
                }
                if actual.len() != 1 || args[0].name.as_deref().is_some_and(|name| name != "path") {
                    self.error(
                        "E1005",
                        "build input calls require exactly one `path` argument",
                        span,
                    );
                    return Type::Poison;
                }
                if !matches!(actual[0], Type::Dynamic)
                    && !type_compatible(&Type::concrete(VarType::Str), &actual[0])
                {
                    self.error("E2001", "build input path must be a string", span);
                }
                return if resolved.ends_with("include_str") {
                    Type::Primitive(VarType::Str, resolved.starts_with("build::try_"))
                } else if resolved.starts_with("build::try_") {
                    Type::Dynamic
                } else {
                    Type::Bytes
                };
            }
            if self.in_comptime && matches!(builtin, Builtin::Native(_)) {
                self.error(
                    "E4002",
                    format!("runtime-only function `{resolved}` cannot execute at comptime"),
                    span,
                );
                return Type::Poison;
            }
            if let Some(index) = actual.iter().position(contains_unresolved_empty_array) {
                self.error(
                    "E2001",
                    "cannot infer array element type for an empty argument; declare a typed array first",
                    args.get(index).and_then(|arg| arg.value.span()),
                );
            }
            if builtin.arity() != args.len() {
                self.error(
                    "E1005",
                    format!(
                        "`{name}` expects {} argument(s), got {}",
                        builtin.arity(),
                        args.len()
                    ),
                    span,
                );
            }
            if args.iter().any(|arg| arg.name.is_some()) {
                let message = if matches!(builtin, Builtin::Native(_)) {
                    "native functions use positional arguments"
                } else {
                    "VM primitives use positional arguments"
                };
                self.error("E1005", message, span);
            }
            if let Builtin::Native(ref import) = builtin {
                for ((parameter, actual), argument) in
                    import.parameters().iter().zip(&actual).zip(args)
                {
                    let value = &argument.value;
                    use crate::runtime::NativeType as N;
                    if *parameter == N::Number {
                        if !matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            && *actual != Type::concrete(VarType::Int)
                            && *actual != Type::concrete(VarType::Float)
                        {
                            self.error(
                                "E2001",
                                format!(
                                    "type mismatch: `{name}` expects int or float, got {}",
                                    actual.display()
                                ),
                                value.span(),
                            );
                        }
                        continue;
                    }
                    let expected = match parameter {
                        N::Any => continue,
                        N::Number => unreachable!(),
                        N::Int => Type::concrete(VarType::Int),
                        N::Float => Type::concrete(VarType::Float),
                        N::Bool => Type::concrete(VarType::Bool),
                        N::StrBuf => Type::StrBuf,
                        N::Str => Type::concrete(VarType::Str),
                        N::Bytes => Type::Bytes,
                        N::Handle => Type::Handle,
                        N::Nil => Type::Nil,
                        N::Array => Type::dynamic_array(),
                        N::Record => Type::Record,
                    };
                    if !matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                        && !type_compatible(&expected, actual)
                    {
                        self.error(
                            "E2001",
                            format!(
                                "type mismatch: `{name}` expects {}, got {}",
                                expected.display(),
                                actual.display()
                            ),
                            value.span(),
                        );
                    }
                }
            }
            return match builtin {
                Builtin::Len => Type::concrete(VarType::Int),
                Builtin::ErrorKind => Type::concrete(VarType::Str),
                Builtin::Exclusive => Type::concrete(VarType::Bool),
                Builtin::BuildInput => Type::Poison,
                Builtin::Native(import) => match import.result() {
                    crate::runtime::NativeType::Int => Type::concrete(VarType::Int),
                    crate::runtime::NativeType::Float => Type::concrete(VarType::Float),
                    crate::runtime::NativeType::Bool => Type::concrete(VarType::Bool),
                    crate::runtime::NativeType::StrBuf => Type::StrBuf,
                    crate::runtime::NativeType::Str => Type::concrete(VarType::Str),
                    crate::runtime::NativeType::Nil => Type::Nil,
                    crate::runtime::NativeType::Bytes => Type::Bytes,
                    crate::runtime::NativeType::Handle => Type::Handle,
                    crate::runtime::NativeType::Array => Type::dynamic_array(),
                    crate::runtime::NativeType::Record => Type::Record,
                    _ => Type::Dynamic,
                },
            };
        }
        // Wildcard native imports have no source declarations to enumerate.
        for scope in self.scopes.iter().rev() {
            for module in scope.modules.values() {
                if let Some(builtin) = Builtin::from_name(&format!("{module}::{name}"))
                    && builtin.arity() == args.len()
                {
                    if let Builtin::Native(ref import) = builtin {
                        self.access_facts
                            .calls
                            .insert(crate::mutability::expr_id(expr), import.name().into());
                    }
                    return Type::Dynamic;
                }
            }
        }
        self.error("E1002", format!("unknown function `{name}`"), span);
        Type::Poison
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;

    #[test]
    fn normal_type_and_error_exclusion_facts_join_independently() {
        let id = 7;
        let non_nil_with_error_possible = FlowFacts {
            normal_types: HashMap::from([(id, Type::concrete(VarType::Int))]),
            error_excluded: HashSet::new(),
        };
        let mut non_nil_without_error = non_nil_with_error_possible.clone();
        non_nil_without_error.error_excluded.insert(id);

        let joined = FlowFacts::join([non_nil_with_error_possible, non_nil_without_error.clone()]);
        assert_eq!(
            joined.normal_types.get(&id),
            Some(&Type::concrete(VarType::Int))
        );
        assert!(!joined.error_excluded.contains(&id));

        let nullable_without_error = FlowFacts {
            normal_types: HashMap::new(),
            error_excluded: HashSet::from([id]),
        };
        let joined = FlowFacts::join([non_nil_without_error, nullable_without_error]);
        assert!(!joined.normal_types.contains_key(&id));
        assert!(joined.error_excluded.contains(&id));
    }
}
