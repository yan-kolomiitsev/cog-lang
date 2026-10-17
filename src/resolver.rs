//! Resolve module/package paths and lexical imports once, before type checking.
//! Bound declarations use unspellable internal names; source spans remain intact.
use crate::ast::*;
use crate::builtins::Builtin;
use crate::diagnostic::{Diagnostic, SourceSpan};
use crate::frontend;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

pub type ModuleKey = (usize, String);
pub const CORE: usize = usize::MAX;

#[derive(Clone)]
pub enum Source {
    File(PathBuf),
    Snapshot(PathBuf, Vec<u8>),
    Embedded(String, String),
    Empty,
}
#[derive(Clone)]
struct Symbol {
    bound: String,
    owner: ModuleKey,
    public: bool,
    variants: Vec<String>,
}
#[derive(Clone)]
enum Binding {
    Local,
    Symbol(Symbol),
    Module(ModuleKey),
    Native(String),
    Poison,
}
type Scope = BTreeMap<String, Binding>;

pub struct Resolver {
    pub sources: BTreeMap<ModuleKey, Source>,
    pub aliases: BTreeMap<(usize, String), usize>,
    symbols: BTreeMap<(ModuleKey, String), Symbol>,
    loaded: BTreeMap<ModuleKey, Program>,
    pending: VecDeque<ModuleKey>,
    output: Vec<Item>,
    diagnostics: Vec<Diagnostic>,
    next_symbol: usize,
    poison: Vec<String>,
    entry: ModuleKey,
    resolved_includes: BTreeSet<(String, Option<String>)>,
    references: BTreeMap<String, BTreeSet<String>>,
    current: Option<String>,
    generic_symbols: BTreeSet<String>,
    type_symbols: BTreeSet<String>,
    active_type_parameters: BTreeSet<String>,
    loaded_source_bytes: usize,
    loaded_syntax_cost: usize,
}

pub struct Resolved {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    pub names: BTreeMap<String, String>,
}

impl Resolver {
    fn type_annotation(&mut self, ty: &mut TypeAnnotation, owner: &ModuleKey, scopes: &[Scope]) {
        match &mut ty.kind {
            TypeKind::Named(name) if name == "type" => {}
            TypeKind::Named(name) => {
                let source = name.clone();
                let symbol = self.name(name, owner, scopes, None);
                ty.kind = TypeKind::ResolvedName { source, symbol };
            }
            TypeKind::Applied { source, arguments } => {
                let source = source.clone();
                let symbol = self.name(&source, owner, scopes, None);
                for argument in arguments.iter_mut() {
                    self.type_annotation(argument, owner, scopes);
                }
                ty.kind = TypeKind::ResolvedApplied {
                    source,
                    symbol,
                    arguments: arguments.clone(),
                };
            }
            TypeKind::ResolvedApplied { arguments, .. } => {
                for argument in arguments {
                    self.type_annotation(argument, owner, scopes);
                }
            }
            TypeKind::ResolvedName { .. } | TypeKind::TypeParameter(_) | TypeKind::SelfType => {}
            TypeKind::Array(element) => self.type_annotation(element, owner, scopes),
            TypeKind::Tuple(items) => {
                for item in items {
                    self.type_annotation(item, owner, scopes);
                }
            }
            TypeKind::Primitive(_) => {}
        }
    }

    pub fn new(
        sources: BTreeMap<ModuleKey, Source>,
        aliases: BTreeMap<(usize, String), usize>,
        entry: ModuleKey,
    ) -> Self {
        Self {
            sources,
            aliases,
            symbols: BTreeMap::new(),
            loaded: BTreeMap::new(),
            pending: VecDeque::new(),
            output: Vec::new(),
            diagnostics: Vec::new(),
            next_symbol: 0,
            poison: Vec::new(),
            entry,
            resolved_includes: BTreeSet::new(),
            references: BTreeMap::new(),
            current: None,
            generic_symbols: BTreeSet::new(),
            type_symbols: BTreeSet::new(),
            active_type_parameters: BTreeSet::new(),
            loaded_source_bytes: 0,
            loaded_syntax_cost: 0,
        }
    }

    /// Only the project loader may acknowledge entry-point inclusion declarations.
    pub(crate) fn with_resolved_includes(
        mut self,
        includes: BTreeSet<(String, Option<String>)>,
    ) -> Self {
        self.resolved_includes = includes;
        self
    }

    fn fresh(&mut self) -> String {
        let name = format!("$symbol{}", self.next_symbol);
        self.next_symbol += 1;
        name
    }
    fn error(&mut self, message: impl Into<String>, span: Option<SourceSpan>) {
        self.diagnostics
            .push(Diagnostic::new("E1003", message, span));
    }
    fn invalid(&mut self) -> String {
        let name = self.fresh();
        self.poison.push(name.clone());
        name
    }

    fn parse_source(&mut self, name: String, text: String) -> crate::hlparser::ParseOutput {
        let remaining = (64 * 1024 * 1024usize).saturating_sub(self.loaded_source_bytes);
        if text.len() > remaining {
            self.error("compiler total source byte limit (64 MiB) exceeded", None);
            return frontend::parse(name, "");
        }
        self.loaded_source_bytes += text.len();
        let mut parsed = frontend::parse(name, text);
        match crate::ast_budget::program(&parsed.program).and_then(|cost| {
            self.loaded_syntax_cost
                .checked_add(cost)
                .filter(|total| *total <= crate::ast_budget::MAX_COST)
                .ok_or_else(|| "compiler total syntax allocation budget exceeded".to_owned())
        }) {
            Ok(total) => self.loaded_syntax_cost = total,
            Err(message) => {
                self.error(message, None);
                parsed.program.items.clear();
            }
        }
        parsed
    }

    fn ensure(&mut self, key: &ModuleKey) {
        if self.loaded.contains_key(key) {
            return;
        }
        if self.loaded.len() >= 4096 {
            self.error("compiler loaded module limit (4096) exceeded", None);
            self.loaded
                .insert(key.clone(), Program { items: Vec::new() });
            return;
        }
        // `resolve` consumes the resolver, so an already-loaded module's
        // source bytes are no longer needed after parsing. Move the source
        // out instead of cloning up to 16 MiB of snapshot data per module.
        let source = self
            .sources
            .get_mut(key)
            .map(|source| std::mem::replace(source, Source::Empty))
            .unwrap_or(Source::Empty);
        let parsed = match source {
            Source::Empty => frontend::parse("<namespace>", ""),
            Source::Embedded(name, text) => self.parse_source(name, text),
            Source::Snapshot(path, bytes) => match String::from_utf8(bytes) {
                Ok(text) => self.parse_source(path.display().to_string(), text),
                Err(_) => {
                    self.error(
                        format!("source {} is not valid UTF-8", path.display()),
                        None,
                    );
                    frontend::parse(path.display().to_string(), "")
                }
            },
            Source::File(path) => match crate::limits::read_source_text(&path) {
                Ok(text) => self.parse_source(path.display().to_string(), text),
                Err(e) => {
                    self.error(format!("cannot read {}: {e}", path.display()), None);
                    frontend::parse(path.display().to_string(), "")
                }
            },
        };
        self.diagnostics.extend(parsed.diagnostics);
        for item in &parsed.program.items {
            let (name, visibility, variants) = match item {
                Item::Function(f) => (&f.name, f.visibility, Vec::new()),
                Item::Trait(declaration) => (&declaration.name, declaration.visibility, Vec::new()),
                Item::GlobalConst {
                    name, visibility, ..
                }
                | Item::Comptime {
                    name, visibility, ..
                }
                | Item::GlobalVar {
                    name, visibility, ..
                }
                | Item::StructDecl {
                    name, visibility, ..
                } => (name, *visibility, Vec::new()),
                Item::EnumDecl {
                    name,
                    visibility,
                    variants,
                } => (
                    name,
                    *visibility,
                    variants.iter().map(|v| v.name.clone()).collect(),
                ),
                Item::Invalid {
                    binding: Some(name),
                } => (name, Visibility::Private, Vec::new()),
                _ => continue,
            };
            let bound = if key == &self.entry && name == "main" && matches!(item, Item::Function(_))
            {
                "main".into()
            } else if key == &(CORE, "err".into())
                && name == "Error"
                && matches!(item, Item::EnumDecl { .. })
            {
                crate::error::ERROR_TYPE.into()
            } else if key == &(CORE, "clone".into())
                && name == "Clone"
                && matches!(item, Item::Trait(_))
            {
                "$core::clone::Clone".into()
            } else {
                self.fresh()
            };
            if matches!(item, Item::Function(f) if !f.type_params.is_empty())
                || matches!(item, Item::StructDecl {type_params, ..} if !type_params.is_empty())
                || matches!(item, Item::Trait(t) if !t.type_params.is_empty())
            {
                self.generic_symbols.insert(bound.clone());
            }
            if matches!(
                item,
                Item::StructDecl { .. } | Item::EnumDecl { .. } | Item::Trait(_)
            ) || matches!(item, Item::Comptime {ty: Some(TypeAnnotation {kind: TypeKind::Named(name), ..}), ..} if name == "type")
            {
                self.type_symbols.insert(bound.clone());
            }
            if self
                .symbols
                .insert(
                    (key.clone(), name.clone()),
                    Symbol {
                        bound,
                        owner: key.clone(),
                        public: visibility == Visibility::Public,
                        variants,
                    },
                )
                .is_some()
            {
                self.error(
                    format!("duplicate declaration `{name}` in module `{}`", key.1),
                    item_span(item),
                );
            }
        }
        self.loaded.insert(key.clone(), parsed.program);
        self.pending.push_back(key.clone());
    }

    fn module(&mut self, key: ModuleKey) -> Option<Binding> {
        if self.sources.contains_key(&key) {
            self.ensure(&key);
            Some(Binding::Module(key))
        } else {
            None
        }
    }

    fn has_std(&self, owner: &ModuleKey) -> bool {
        owner.0 == CORE
            || self
                .aliases
                .iter()
                .any(|((_, alias), package)| alias == "std" && *package == CORE)
    }

    fn member(
        &mut self,
        base: Binding,
        tail: &str,
        owner: &ModuleKey,
        span: Option<SourceSpan>,
    ) -> Option<Binding> {
        match base {
            Binding::Poison => Some(Binding::Poison),
            Binding::Module(key) => {
                self.ensure(&key);
                let (first, rest) = tail.split_once("::").unwrap_or((tail, ""));
                let child = (key.0, join(&key.1, first));
                let found = if let Some(symbol) =
                    self.symbols.get(&(key.clone(), first.into())).cloned()
                {
                    if !symbol.public && &symbol.owner != owner {
                        self.error(
                            format!("`{first}` is private to module `{}`", key.1),
                            span.clone(),
                        );
                        return Some(Binding::Poison);
                    }
                    Some(Binding::Symbol(symbol))
                } else {
                    if key.0 == CORE && Builtin::from_name(&format!("{}::{first}", key.1)).is_some()
                    {
                        Some(Binding::Native(format!("{}::{first}", key.1)))
                    } else {
                        self.module(child)
                    }
                };
                match (found, rest.is_empty()) {
                    (Some(found), true) => Some(found),
                    (Some(found), false) => self.member(found, rest, owner, span),
                    _ => None,
                }
            }
            Binding::Native(module) => {
                let name = format!("{module}::{tail}");
                if Builtin::from_name(&name).is_some() {
                    Some(Binding::Native(name))
                } else {
                    None
                }
            }
            Binding::Symbol(mut symbol) if symbol.variants.iter().any(|v| v == tail) => {
                symbol.bound = format!("{}::{tail}", symbol.bound);
                symbol.variants.clear();
                Some(Binding::Symbol(symbol))
            }
            _ => None,
        }
    }

    fn import_path(&mut self, path: &str, owner: &ModuleKey, span: Option<SourceSpan>) -> Binding {
        let (head, tail) = path.split_once("::").unwrap_or((path, ""));
        let base = if head == "self" {
            self.module(owner.clone())
        } else if head == "super" {
            owner
                .1
                .rsplit_once("::")
                .map(|(parent, _)| (owner.0, parent.into()))
                .or_else(|| (!owner.1.is_empty()).then_some((owner.0, String::new())))
                .and_then(|key| self.module(key))
        } else if let Some(package) = self
            .aliases
            .get(&(owner.0, head.to_owned()))
            .copied()
            .or_else(|| (head == "std" && self.has_std(owner)).then_some(CORE))
        {
            self.module((package, String::new()))
        } else {
            self.unqualified_module(head, owner, span.clone())
        };
        let found = match base {
            Some(base) if tail.is_empty() => Some(base),
            Some(base) => self.member(base, tail, owner, span.clone()),
            None => None,
        };
        found.unwrap_or_else(|| {
            self.error(format!("unknown import `{path}`"), span);
            Binding::Poison
        })
    }

    fn unqualified_module(
        &mut self,
        name: &str,
        owner: &ModuleKey,
        span: Option<SourceSpan>,
    ) -> Option<Binding> {
        let mut candidates = BTreeSet::new();
        for key in [(owner.0, join(&owner.1, name)), (owner.0, name.to_owned())] {
            if self.sources.contains_key(&key) {
                candidates.insert(key);
            }
        }
        for ((from, _), package) in &self.aliases {
            if *from == owner.0
                && *package != CORE
                && self.sources.contains_key(&(*package, name.to_owned()))
            {
                candidates.insert((*package, name.to_owned()));
            }
        }
        if self.has_std(owner) {
            let key = (CORE, name.to_owned());
            if self.sources.contains_key(&key) || crate::builtins::native_module(name) {
                candidates.insert(key);
            }
        }
        if candidates.len() > 1 {
            let packages = candidates
                .iter()
                .map(|(package, _)| {
                    if *package == CORE {
                        "std".to_owned()
                    } else {
                        self.aliases
                            .iter()
                            .find_map(|((from, alias), target)| {
                                (*from == owner.0 && target == package).then(|| alias.clone())
                            })
                            .unwrap_or_else(|| "the current package".into())
                    }
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(" and ");
            self.error(
                format!(
                    "ambiguous module `{name}` from {packages}; select its package with `from`"
                ),
                span,
            );
            return Some(Binding::Poison);
        }
        let key = candidates.into_iter().next()?;
        if key.0 == CORE && !self.sources.contains_key(&key) && crate::builtins::native_module(name)
        {
            Some(Binding::Native(name.into()))
        } else {
            self.module(key)
        }
    }

    fn package_import_path(
        &mut self,
        path: &str,
        package: &str,
        owner: &ModuleKey,
        span: Option<SourceSpan>,
    ) -> Binding {
        let target = self
            .aliases
            .get(&(owner.0, package.to_owned()))
            .copied()
            .or_else(|| (package == "std" && self.has_std(owner)).then_some(CORE));
        target
            .and_then(|id| {
                self.module((id, path.to_owned())).or_else(|| {
                    (id == CORE && crate::builtins::native_module(path))
                        .then(|| Binding::Native(path.into()))
                })
            })
            .unwrap_or_else(|| {
                self.error(
                    format!("unknown module `{path}` in package `{package}`"),
                    span,
                );
                Binding::Poison
            })
    }

    fn bind_import(
        &mut self,
        scope: &mut Scope,
        name: String,
        binding: Binding,
        span: Option<SourceSpan>,
    ) {
        if scope.insert(name.clone(), binding).is_some() {
            self.error(format!("duplicate import/binding `{name}`"), span);
            scope.insert(name, Binding::Poison);
        }
    }

    fn import(
        &mut self,
        decl: &UseDecl,
        owner: &ModuleKey,
        scope: &mut Scope,
        span: Option<SourceSpan>,
    ) {
        let base = if let Some(package) = &decl.from {
            self.package_import_path(&decl.module, package, owner, span.clone())
        } else {
            self.import_path(&decl.module, owner, span.clone())
        };
        if decl.namespace {
            let alias = decl
                .aliases
                .first()
                .and_then(|v| v.as_deref())
                .unwrap_or_else(|| decl.module.rsplit("::").next().unwrap_or(&decl.module));
            self.bind_import(scope, alias.into(), base, span);
        } else if decl.wildcard {
            match base {
                Binding::Module(key) => {
                    self.ensure(&key);
                    let symbols: Vec<_> = self
                        .symbols
                        .iter()
                        .filter(|((m, _), s)| m == &key && (s.public || m == owner))
                        .map(|((_, name), s)| (name.clone(), Binding::Symbol(s.clone())))
                        .collect();
                    for (name, binding) in symbols {
                        self.bind_import(scope, name, binding, span.clone());
                    }
                    if key.0 == CORE {
                        for name in native_names(&key.1) {
                            let short = name.rsplit("::").next().unwrap_or(&name).to_owned();
                            self.bind_import(scope, short, Binding::Native(name), span.clone());
                        }
                    }
                }
                Binding::Native(module) => {
                    for name in native_names(&module) {
                        let short = name.rsplit("::").next().unwrap_or(&name).to_owned();
                        self.bind_import(scope, short, Binding::Native(name), span.clone());
                    }
                }
                _ => {}
            }
        } else {
            for (i, name) in decl.names.iter().enumerate() {
                let binding = self
                    .member(base.clone(), name, owner, span.clone())
                    .unwrap_or_else(|| {
                        self.error(
                            format!("unknown item `{name}` in `{}`", decl.module),
                            span.clone(),
                        );
                        Binding::Poison
                    });
                let alias = decl
                    .aliases
                    .get(i)
                    .and_then(|v| v.as_deref())
                    .unwrap_or(name);
                self.bind_import(scope, alias.into(), binding, span.clone());
            }
        }
    }

    fn name(
        &mut self,
        name: &str,
        owner: &ModuleKey,
        scopes: &[Scope],
        span: Option<SourceSpan>,
    ) -> String {
        if self.symbols.values().any(|symbol| symbol.bound == name) {
            return name.into();
        }
        let (head, tail) = name.split_once("::").unwrap_or((name, ""));
        let scoped = scopes.iter().rev().find_map(|s| s.get(head)).cloned();
        let binding = scoped
            .or_else(|| {
                self.symbols
                    .get(&(owner.clone(), head.into()))
                    .cloned()
                    .map(Binding::Symbol)
            })
            .or_else(|| {
                // A module may refer to its own members by its own name.
                (owner.1.rsplit("::").next() == Some(head) && !tail.is_empty())
                    .then(|| Binding::Module(owner.clone()))
            })
            .or_else(|| {
                Builtin::from_name(name)
                    .filter(|_| matches!(name, "len" | "exclusive"))
                    .map(|_| Binding::Native(name.into()))
            });
        let result = match binding {
            Some(b) if tail.is_empty() => Some(b),
            Some(b) => self.member(b, tail, owner, span.clone()),
            None => None,
        };
        match result {
            Some(Binding::Local) => name.into(),
            Some(Binding::Symbol(symbol)) => {
                if let Some(current) = &self.current {
                    self.references
                        .entry(current.clone())
                        .or_default()
                        .insert(symbol.bound.clone());
                }
                symbol.bound
            }
            Some(Binding::Native(value)) if Builtin::from_name(&value).is_some() => value,
            Some(Binding::Poison) => self.invalid(),
            _ => {
                self.error(
                    format!("unresolved name `{name}`; import its module with use"),
                    span,
                );
                self.invalid()
            }
        }
    }

    fn expression_type(&self, expr: &Expr, scopes: &[Scope]) -> Option<TypeAnnotation> {
        if let Expr::TypeLiteral(ty) = expr.node() {
            return Some(ty.clone());
        }
        let Expr::Ident(name) = expr.node() else {
            return None;
        };
        if scopes
            .iter()
            .rev()
            .any(|s| matches!(s.get(name), Some(Binding::Local)))
        {
            return None;
        }
        let kind = match name.as_str() {
            "int" => TypeKind::Primitive(VarType::Int),
            "float" => TypeKind::Primitive(VarType::Float),
            "bool" => TypeKind::Primitive(VarType::Bool),
            "str" => TypeKind::Primitive(VarType::Str),
            "nil" => TypeKind::Primitive(VarType::Nil),
            _ if self.active_type_parameters.contains(name) => {
                TypeKind::TypeParameter(name.clone())
            }
            _ if self.type_symbols.contains(name) => TypeKind::ResolvedName {
                source: name.clone(),
                symbol: name.clone(),
            },
            _ => return None,
        };
        Some(TypeAnnotation {
            kind,
            nilable: false,
        })
    }

    fn explicit_types(
        &mut self,
        arguments: &mut [Expr],
        owner: &ModuleKey,
        scopes: &mut Vec<Scope>,
        location: Option<SourceSpan>,
    ) -> Vec<TypeAnnotation> {
        arguments
            .iter_mut()
            .filter_map(|argument| {
                self.expr(argument, owner, scopes, location.clone());
                let ty = self.expression_type(argument, scopes);
                if ty.is_none() {
                    self.error(
                        "generic arguments must be compile-time types",
                        argument.span().cloned().or(location.clone()),
                    );
                }
                ty
            })
            .collect()
    }

    fn expr(
        &mut self,
        expr: &mut Expr,
        owner: &ModuleKey,
        scopes: &mut Vec<Scope>,
        location: Option<SourceSpan>,
    ) {
        match expr {
            Expr::Apply(callee, args) => {
                if let Expr::Index(receiver, index) = callee.as_mut() {
                    **callee = Expr::BracketApply(receiver.clone(), vec![*index.clone()]);
                }
                if let Expr::BracketApply(receiver, arguments) = callee.as_mut() {
                    self.expr(receiver, owner, scopes, location.clone());
                    if let Expr::Ident(name) = receiver.node()
                        && self.generic_symbols.contains(name)
                    {
                        let name = name.clone();
                        let types = self.explicit_types(arguments, owner, scopes, location.clone());
                        for arg in args.iter_mut() {
                            self.expr(&mut arg.value, owner, scopes, location.clone());
                        }
                        *expr = Expr::SpecializedCall(name, types, std::mem::take(args));
                        return;
                    }
                }
                self.expr(callee, owner, scopes, location.clone());
                for arg in args.iter_mut() {
                    self.expr(&mut arg.value, owner, scopes, location.clone());
                }
                if let Expr::Ident(name) = callee.node() {
                    *expr = Expr::Call(name.clone(), std::mem::take(args));
                } else {
                    self.error("this expression is not a named callable", location);
                }
            }
            Expr::BracketApply(receiver, arguments) => {
                self.expr(receiver, owner, scopes, location.clone());
                let generic = matches!(receiver.node(), Expr::Ident(name) if self.generic_symbols.contains(name));
                if generic {
                    let Expr::Ident(symbol) = receiver.node() else {
                        unreachable!()
                    };
                    let symbol = symbol.clone();
                    if !self.type_symbols.contains(&symbol) {
                        self.error(
                            "a specialized function must be followed by a call",
                            location,
                        );
                        return;
                    }
                    let types = self.explicit_types(arguments, owner, scopes, location.clone());
                    if types.is_empty() {
                        self.error("generic type arguments cannot be empty", location);
                    }
                    *expr = Expr::TypeLiteral(TypeAnnotation {
                        kind: TypeKind::ResolvedApplied {
                            source: symbol.clone(),
                            symbol,
                            arguments: types,
                        },
                        nilable: false,
                    });
                } else if arguments.is_empty() {
                    if let Some(ty) = self.expression_type(receiver, scopes) {
                        *expr = Expr::TypeLiteral(TypeAnnotation {
                            kind: TypeKind::Array(Box::new(ty)),
                            nilable: false,
                        });
                    } else {
                        self.error(
                            "an index cannot be empty; `[]` only forms an array type",
                            location,
                        );
                    }
                } else {
                    for arg in arguments.iter_mut() {
                        self.expr(arg, owner, scopes, location.clone());
                    }
                    if arguments.len() != 1 {
                        self.error("indexing requires exactly one index", location);
                        return;
                    }
                    *expr = Expr::Index(receiver.clone(), Box::new(arguments.remove(0)));
                }
            }
            Expr::SpecializedCall(_, _, _) => {
                unreachable!("specialized calls are produced by resolution")
            }
            Expr::Located(span, inner) => self.expr(inner, owner, scopes, Some(span.clone())),
            Expr::TypeLiteral(ty) => self.type_annotation(ty, owner, scopes),
            Expr::Ident(name)
                if (matches!(name.as_str(), "int" | "float" | "bool" | "str" | "nil")
                    || self.active_type_parameters.contains(name))
                    && !scopes.iter().rev().any(|s| s.contains_key(name)) => {}
            Expr::Ident(name) => *name = self.name(name, owner, scopes, location),
            Expr::Call(name, args) => {
                *name = self.name(name, owner, scopes, location.clone());
                for arg in args {
                    self.expr(&mut arg.value, owner, scopes, location.clone());
                }
            }
            Expr::Array(xs) => {
                for x in xs {
                    self.expr(x, owner, scopes, location.clone());
                }
            }
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let StringPart::Expression(value) = part {
                        self.expr(value, owner, scopes, location.clone());
                    }
                }
            }
            Expr::Record(xs) => {
                for (_, x) in xs {
                    self.expr(x, owner, scopes, location.clone());
                }
            }
            Expr::MethodCall(x, _, args) => {
                self.expr(x, owner, scopes, location.clone());
                for arg in args {
                    self.expr(arg, owner, scopes, location.clone());
                }
            }
            Expr::Index(a, b) => {
                self.expr(a, owner, scopes, location.clone());
                if matches!(a.node(), Expr::Ident(name) if self.generic_symbols.contains(name)) {
                    *expr = Expr::BracketApply(a.clone(), vec![*b.clone()]);
                    self.expr(expr, owner, scopes, location);
                } else {
                    self.expr(b, owner, scopes, location);
                }
            }
            Expr::Binary(_, a, b)
            | Expr::Fallback {
                value: a,
                fallback: b,
                ..
            } => {
                self.expr(a, owner, scopes, location.clone());
                self.expr(b, owner, scopes, location);
            }
            Expr::Unary(_, x) | Expr::Cast(x, _) | Expr::Propagate(x) => {
                self.expr(x, owner, scopes, location)
            }
            Expr::Block(body) => self.block(body, owner, scopes, location),
            Expr::Match { value, arms, .. } => {
                self.expr(value, owner, scopes, location.clone());
                for arm in arms {
                    scopes.push(Scope::new());
                    if let MatchPattern::Type(name) = &mut arm.pattern
                        && crate::instruction::ValueType::primitive(name).is_none()
                    {
                        *name = self.name(name, owner, scopes, location.clone());
                    }
                    if let MatchPattern::Variant { name, bindings } = &mut arm.pattern {
                        *name = self.name(name, owner, scopes, location.clone());
                        for binding in bindings {
                            if binding != "_" {
                                scopes
                                    .last_mut()
                                    .unwrap()
                                    .insert(binding.clone(), Binding::Local);
                            }
                        }
                    }
                    if let Some(guard) = &mut arm.guard {
                        self.expr(guard, owner, scopes, location.clone());
                    }
                    self.block(&mut arm.body, owner, scopes, location.clone());
                    scopes.pop();
                }
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, owner, scopes, location.clone());
                scopes.push(BTreeMap::from([(catch_name.clone(), Binding::Local)]));
                self.block(catch_block, owner, scopes, location);
                scopes.pop();
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, owner, scopes, location.clone());
                self.expr(then_branch, owner, scopes, location.clone());
                if let Some(x) = else_branch {
                    self.expr(x, owner, scopes, location);
                }
            }
            _ => {}
        }
    }

    fn block(
        &mut self,
        block: &mut Block,
        owner: &ModuleKey,
        scopes: &mut Vec<Scope>,
        location: Option<SourceSpan>,
    ) {
        scopes.push(Scope::new());
        for stmt in &mut block.stmts {
            self.stmt(stmt, owner, scopes, location.clone());
        }
        if let Some(tail) = &mut block.tail {
            self.expr(tail, owner, scopes, location);
        }
        scopes.pop();
        block
            .stmts
            .retain(|s| !matches!(s.node(), Stmt::Use { .. }));
    }

    fn stmt(
        &mut self,
        stmt: &mut Stmt,
        owner: &ModuleKey,
        scopes: &mut Vec<Scope>,
        location: Option<SourceSpan>,
    ) {
        match stmt {
            Stmt::Located(span, inner) => self.stmt(inner, owner, scopes, Some(span.clone())),
            Stmt::Invalid {
                binding: Some(name),
            } => {
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), Binding::Poison);
            }
            Stmt::Use {
                module,
                from,
                names,
                aliases,
                namespace,
                wildcard,
            } => {
                let decl = UseDecl {
                    span: location.clone(),
                    module: module.clone(),
                    from: from.clone(),
                    names: names.clone(),
                    aliases: aliases.clone(),
                    namespace: *namespace,
                    wildcard: *wildcard,
                };
                self.import(&decl, owner, scopes.last_mut().unwrap(), location);
            }
            Stmt::VarDecl { name, ty, value } | Stmt::Const { name, ty, value } => {
                if let Some(ty) = ty {
                    self.type_annotation(ty, owner, scopes);
                }
                self.expr(value, owner, scopes, location);
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), Binding::Local);
            }
            Stmt::Comptime { name, ty, value } => {
                if let Some(ty) = ty {
                    self.type_annotation(ty, owner, scopes);
                }
                self.expr(value, owner, scopes, location);
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), Binding::Local);
            }
            Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
                self.expr(target, owner, scopes, location.clone());
                self.expr(value, owner, scopes, location);
            }
            Stmt::Expr(x) | Stmt::Crash { value: x, .. } => self.expr(x, owner, scopes, location),
            Stmt::Return(Some(x)) => self.expr(x, owner, scopes, location),
            Stmt::Defer { body } => self.block(body, owner, scopes, location),
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, owner, scopes, location.clone());
                scopes.push(BTreeMap::from([(catch_name.clone(), Binding::Local)]));
                self.block(catch_block, owner, scopes, location);
                scopes.pop();
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                self.expr(cond, owner, scopes, location.clone());
                self.block(then_block, owner, scopes, location.clone());
                if let Some(b) = else_block {
                    self.block(b, owner, scopes, location);
                }
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                self.expr(value, owner, scopes, location.clone());
                scopes.push(BTreeMap::from([(name.clone(), Binding::Local)]));
                self.block(then_block, owner, scopes, location.clone());
                scopes.pop();
                if let Some(b) = else_block {
                    self.block(b, owner, scopes, location);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond, owner, scopes, location.clone());
                self.block(body, owner, scopes, location);
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.expr(start, owner, scopes, location.clone());
                self.expr(end, owner, scopes, location.clone());
                if let Some(step) = step {
                    self.expr(step, owner, scopes, location.clone());
                }
                scopes.push(BTreeMap::from([(var.clone(), Binding::Local)]));
                self.block(body, owner, scopes, location);
                scopes.pop();
            }
            Stmt::ForEach {
                var,
                iterable,
                body,
            } => {
                self.expr(iterable, owner, scopes, location.clone());
                scopes.push(BTreeMap::from([(var.clone(), Binding::Local)]));
                self.block(body, owner, scopes, location);
                scopes.pop();
            }
            _ => {}
        }
    }

    pub fn resolve(mut self) -> Resolved {
        self.ensure(&self.entry.clone());
        while let Some(owner) = self.pending.pop_front() {
            let mut program = self.loaded[&owner].clone();
            let mut imports = Scope::new();
            for item in &program.items {
                if let Item::Use(decl) = item {
                    self.import(decl, &owner, &mut imports, decl.span.clone());
                }
            }
            for mut item in program.items.drain(..) {
                self.active_type_parameters = match &item {
                    Item::Function(f) => f.type_params.iter().map(|p| p.name.clone()).collect(),
                    Item::StructDecl { type_params, .. } => {
                        type_params.iter().map(|p| p.name.clone()).collect()
                    }
                    Item::Trait(t) => t.type_params.iter().cloned().collect(),
                    Item::Extend(extension) => extension
                        .type_params
                        .iter()
                        .map(|parameter| parameter.name.clone())
                        .collect(),
                    _ => BTreeSet::new(),
                };
                let mut scopes = vec![imports.clone()];
                let original = match &item {
                    Item::Function(f) => Some(f.name.clone()),
                    Item::Trait(declaration) => Some(declaration.name.clone()),
                    Item::StructDecl { name, .. }
                    | Item::EnumDecl { name, .. }
                    | Item::GlobalConst { name, .. }
                    | Item::Comptime { name, .. }
                    | Item::GlobalVar { name, .. } => Some(name.clone()),
                    Item::Invalid { binding } => binding.clone(),
                    _ => None,
                };
                let bound = original
                    .as_ref()
                    .and_then(|n| self.symbols.get(&(owner.clone(), n.clone())))
                    .map(|s| s.bound.clone());
                self.current = bound.clone();
                match &mut item {
                    Item::Function(f) => {
                        f.owner = Some(owner_identity(&owner));
                        for parameter in &mut f.type_params {
                            for bound in &mut parameter.bounds {
                                bound.name = self.name(&bound.name, &owner, &scopes, None);
                                for argument in &mut bound.arguments {
                                    self.type_annotation(argument, &owner, &scopes);
                                }
                            }
                        }
                        for param in &mut f.params {
                            if let Some(ty) = &mut param.ty {
                                self.type_annotation(ty, &owner, &scopes);
                            }
                            if let Some(default) = &mut param.default {
                                self.expr(default, &owner, &mut scopes, None);
                            }
                        }
                        if let Some(ty) = &mut f.result_type {
                            self.type_annotation(ty, &owner, &scopes);
                        }
                        scopes.push(
                            f.params
                                .iter()
                                .map(|p| (p.name.clone(), Binding::Local))
                                .collect(),
                        );
                        self.block(&mut f.body, &owner, &mut scopes, None);
                        f.name = bound.unwrap();
                        f.visibility = Visibility::Public;
                    }
                    Item::Extend(extension) => {
                        for parameter in &mut extension.type_params {
                            for bound in &mut parameter.bounds {
                                bound.name = self.name(&bound.name, &owner, &scopes, None);
                                for argument in &mut bound.arguments {
                                    self.type_annotation(argument, &owner, &scopes);
                                }
                            }
                        }
                        self.type_annotation(&mut extension.target, &owner, &scopes);
                        if let Some(trait_name) = &mut extension.trait_name {
                            *trait_name = self.name(trait_name, &owner, &scopes, None);
                        }
                        for argument in &mut extension.trait_arguments {
                            replace_self_type(argument, &extension.target);
                            self.type_annotation(argument, &owner, &scopes);
                        }
                        for method in &mut extension.methods {
                            let f = &mut method.function;
                            self.active_type_parameters = f
                                .type_params
                                .iter()
                                .map(|parameter| parameter.name.clone())
                                .collect();
                            for parameter in &mut f.type_params {
                                for bound in &mut parameter.bounds {
                                    bound.name = self.name(&bound.name, &owner, &scopes, None);
                                    for argument in &mut bound.arguments {
                                        self.type_annotation(argument, &owner, &scopes);
                                    }
                                }
                            }
                            f.owner = Some(owner_identity(&owner));
                            for parameter in &mut f.params {
                                if let Some(ty) = &mut parameter.ty {
                                    replace_self_type(ty, &extension.target);
                                    self.type_annotation(ty, &owner, &scopes);
                                }
                                if let Some(default) = &mut parameter.default {
                                    self.expr(default, &owner, &mut scopes, None);
                                }
                            }
                            if let Some(ty) = &mut f.result_type {
                                replace_self_type(ty, &extension.target);
                                self.type_annotation(ty, &owner, &scopes);
                            }
                            let method_name = self.fresh();
                            self.current = Some(method_name.clone());
                            scopes.push(
                                f.params
                                    .iter()
                                    .map(|parameter| (parameter.name.clone(), Binding::Local))
                                    .collect(),
                            );
                            self.block(&mut f.body, &owner, &mut scopes, None);
                            scopes.pop();
                            f.name = if owner.1.is_empty() {
                                method_name
                            } else {
                                format!("{}::{method_name}", owner.1)
                            };
                            f.visibility = Visibility::Public;
                        }
                    }
                    Item::Trait(declaration) => {
                        for method in &mut declaration.methods {
                            for parameter in &mut method.params {
                                if let Some(ty) = &mut parameter.ty {
                                    self.type_annotation(ty, &owner, &scopes);
                                }
                            }
                            if let Some(ty) = &mut method.result_type {
                                self.type_annotation(ty, &owner, &scopes);
                            }
                        }
                        declaration.name = bound.unwrap();
                        declaration.visibility = Visibility::Public;
                    }
                    Item::StructDecl {
                        name,
                        visibility,
                        type_params,
                        fields,
                        owner: struct_owner,
                    } => {
                        *struct_owner = Some(owner_identity(&owner));
                        for parameter in type_params {
                            for bound in &mut parameter.bounds {
                                bound.name = self.name(&bound.name, &owner, &scopes, None);
                                for argument in &mut bound.arguments {
                                    self.type_annotation(argument, &owner, &scopes);
                                }
                            }
                        }
                        for field in fields {
                            if let Some(ty) = &mut field.ty {
                                self.type_annotation(ty, &owner, &scopes);
                            }
                            if let Some(default) = &mut field.default {
                                self.expr(default, &owner, &mut scopes, None);
                            }
                        }
                        *name = bound.unwrap();
                        *visibility = Visibility::Public;
                    }
                    Item::EnumDecl {
                        name,
                        visibility,
                        variants,
                    } => {
                        for variant in variants {
                            for field in &mut variant.payload {
                                if let Some(ty) = &mut field.ty {
                                    self.type_annotation(ty, &owner, &scopes);
                                }
                            }
                        }
                        *name = bound.unwrap();
                        *visibility = Visibility::Public;
                    }
                    Item::GlobalConst {
                        name,
                        visibility,
                        value,
                        ty,
                        ..
                    }
                    | Item::GlobalVar {
                        name,
                        visibility,
                        value,
                        ty,
                        ..
                    } => {
                        if let Some(ty) = ty {
                            self.type_annotation(ty, &owner, &scopes);
                        }
                        self.expr(value, &owner, &mut scopes, None);
                        *name = bound.unwrap();
                        *visibility = Visibility::Public;
                    }
                    Item::Comptime {
                        name,
                        visibility,
                        value,
                        ty,
                    } => {
                        if let Some(ty) = ty {
                            self.type_annotation(ty, &owner, &scopes);
                        }
                        self.expr(value, &owner, &mut scopes, None);
                        *name = bound.unwrap();
                        *visibility = Visibility::Public;
                    }
                    Item::Invalid { binding } => *binding = bound,
                    Item::ComptimeInclude {
                        package,
                        path,
                        span,
                    } => {
                        let message = if owner != self.entry {
                            Some(
                                "package inclusion is only supported in the project entry point"
                                    .into(),
                            )
                        } else if !self
                            .resolved_includes
                            .contains(&(package.clone(), path.clone()))
                        {
                            Some(format!(
                                "unresolved package inclusion `{package}`; source packages require project::load or project::prepare"
                            ))
                        } else {
                            None
                        };
                        if let Some(message) = message {
                            self.diagnostics
                                .push(Diagnostic::new("E3001", message, span.clone()));
                        }
                        continue;
                    }
                    Item::Use(_) => continue,
                }
                self.output.push(item);
            }
        }
        for name in self.poison {
            self.output.push(Item::Invalid {
                binding: Some(name),
            });
        }
        // These imports describe already-bound native references, not user-visible scope.
        for module in crate::builtins::native_modules() {
            self.output.push(Item::Use(UseDecl {
                span: None,
                module,
                from: None,
                names: vec![],
                aliases: vec![None],
                namespace: true,
                wildcard: false,
            }));
        }
        let names = self
            .symbols
            .iter()
            .map(|(((_, module), name), symbol)| {
                (
                    symbol.bound.clone(),
                    if module.is_empty() {
                        name.clone()
                    } else {
                        format!("{module}::{name}")
                    },
                )
            })
            .collect();
        order_globals(&mut self.output, &self.references, &mut self.diagnostics);
        Resolved {
            program: Program { items: self.output },
            diagnostics: self.diagnostics,
            names,
        }
    }
}

fn join(base: &str, child: &str) -> String {
    if base.is_empty() {
        child.into()
    } else {
        format!("{base}::{child}")
    }
}

fn owner_identity(owner: &ModuleKey) -> String {
    format!("{}:{}", owner.0, owner.1)
}

fn item_span(item: &Item) -> Option<SourceSpan> {
    match item {
        Item::Function(f) => f
            .body
            .stmts
            .first()
            .and_then(Stmt::span)
            .or_else(|| f.body.tail.as_deref().and_then(Expr::span))
            .or(f.result_span.as_ref())
            .cloned(),
        Item::GlobalConst { value, .. }
        | Item::Comptime { value, .. }
        | Item::GlobalVar { value, .. } => value.span().cloned(),
        _ => None,
    }
}
fn native_names(module: &str) -> Vec<String> {
    ["err::kind".to_owned()]
        .into_iter()
        .chain(crate::runtime::names())
        .filter(|name| name.starts_with(&format!("{module}::")))
        .collect()
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

fn order_globals(
    items: &mut Vec<Item>,
    references: &BTreeMap<String, BTreeSet<String>>,
    errors: &mut Vec<Diagnostic>,
) {
    let mut globals = BTreeMap::new();
    let mut declaration_order = Vec::new();
    items.retain(|item| {
        if let Item::GlobalConst { name, .. }
        | Item::Comptime { name, .. }
        | Item::GlobalVar { name, .. } = item
        {
            declaration_order.push(name.clone());
            globals.insert(name.clone(), item.clone());
            false
        } else {
            true
        }
    });
    let mut dependencies = BTreeMap::<String, BTreeSet<String>>::new();
    for name in globals.keys() {
        let mut pending: Vec<_> = references
            .get(name)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let mut seen = BTreeSet::new();
        while let Some(target) = pending.pop() {
            if !seen.insert(target.clone()) {
                continue;
            }
            if globals.contains_key(&target) {
                dependencies.entry(name.clone()).or_default().insert(target);
            } else {
                pending.extend(references.get(&target).into_iter().flatten().cloned());
            }
        }
    }
    fn visit(
        name: &str,
        globals: &BTreeMap<String, Item>,
        deps: &BTreeMap<String, BTreeSet<String>>,
        done: &mut BTreeSet<String>,
        active: &mut Vec<String>,
        order: &mut Vec<String>,
        errors: &mut Vec<Diagnostic>,
    ) {
        if done.contains(name) {
            return;
        }
        if active.iter().any(|n| n == name) {
            errors.push(Diagnostic::new(
                "E1004",
                format!(
                    "cyclic global initialization: {} -> {name}",
                    active.join(" -> ")
                ),
                globals.get(name).and_then(item_span),
            ));
            return;
        }
        active.push(name.into());
        for dep in deps.get(name).into_iter().flatten() {
            visit(dep, globals, deps, done, active, order, errors);
        }
        active.pop();
        done.insert(name.into());
        order.push(name.into());
    }
    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    for name in &declaration_order {
        visit(
            name,
            &globals,
            &dependencies,
            &mut done,
            &mut Vec::new(),
            &mut order,
            errors,
        );
    }
    let mut output: Vec<_> = order
        .into_iter()
        .map(|name| globals.remove(&name).expect("ordered global"))
        .collect();
    output.append(items);
    *items = output;
}

#[cfg(test)]
mod tests {
    use super::{ModuleKey, Resolver, Source};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn loading_snapshot_consumes_the_retained_source_bytes() {
        let key: ModuleKey = (0, String::new());
        let source = b"fn main() {}".to_vec();
        let resolver = Resolver::new(
            BTreeMap::from([(
                key.clone(),
                Source::Snapshot(PathBuf::from("main.cog"), source),
            )]),
            BTreeMap::new(),
            key.clone(),
        );
        let mut resolver = resolver;

        resolver.ensure(&key);

        assert!(matches!(resolver.sources.get(&key), Some(Source::Empty)));
        assert_eq!(resolver.loaded.get(&key).unwrap().items.len(), 1);
        assert!(resolver.diagnostics.is_empty());
    }
}
