//! Read-only access capabilities and inferred heap mutation effects. This is a
//! source semantic pass: assignment still shares objects and bytecode is unchanged.
use crate::ast::*;
use crate::diagnostic::{Diagnostic, SourceSpan};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) fn expr_id(expr: &Expr) -> usize {
    expr.node() as *const Expr as usize
}
fn stmt_id(stmt: &Stmt) -> usize {
    stmt.node() as *const Stmt as usize
}
fn annotation_may_reference(ty: &TypeAnnotation) -> bool {
    matches!(
        &ty.kind,
        TypeKind::Named(_)
            | TypeKind::ResolvedName { .. }
            | TypeKind::Applied { .. }
            | TypeKind::ResolvedApplied { .. }
            | TypeKind::TypeParameter(_)
            | TypeKind::SelfType
            | TypeKind::Array(_)
            | TypeKind::Tuple(_)
    )
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueKind {
    Scalar,
    Reference,
    Never,
    Invalid,
}
#[derive(Default)]
pub(crate) struct Facts {
    pub values: HashMap<usize, ValueKind>,
    pub calls: HashMap<usize, String>,
    pub specializations: HashMap<usize, String>,
    pub mutating_calls: BTreeSet<usize>,
    pub structural_clones: BTreeSet<usize>,
    pub globals: HashMap<usize, String>,
    pub invalid_bindings: BTreeSet<usize>,
    pub fallback_possible: HashMap<usize, bool>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Root {
    Parameter(usize),
    Global(String),
    Fresh(usize, usize),
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Field(String),
    Index(i64),
    AnyIndex,
    Descendant,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Path {
    root: Root,
    keys: Vec<Key>,
}
impl Path {
    fn child(&self, key: Key) -> Self {
        let mut path = self.clone();
        // Recursive summaries have a finite abstract domain. Widen deep paths
        // to all descendants rather than dropping a read-only fact.
        if path.keys.last() != Some(&Key::Descendant) {
            path.keys.push(if path.keys.len() >= 4 {
                Key::Descendant
            } else {
                key
            });
        }
        path
    }
}
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Access {
    refs: BTreeSet<Reference>,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    path: Path,
    readonly: BTreeSet<String>,
    // Provenance also counts mutable uses through aliases of a local var.
    locals: BTreeSet<usize>,
}
impl Access {
    fn root(root: Root) -> Self {
        Self {
            refs: [Reference {
                path: Path { root, keys: vec![] },
                readonly: BTreeSet::new(),
                locals: BTreeSet::new(),
            }]
            .into(),
        }
    }
    fn union(&mut self, other: &Self) {
        self.refs.extend(other.refs.iter().cloned());
    }
    fn map(&self, mut f: impl FnMut(&mut Reference)) -> Self {
        Self {
            refs: self
                .refs
                .iter()
                .cloned()
                .map(|mut r| {
                    f(&mut r);
                    r
                })
                .collect(),
        }
    }
    fn readonly(&self, name: &str) -> Self {
        self.map(|r| {
            r.readonly.insert(name.into());
        })
    }
}
type Heap = BTreeMap<Path, Access>;
#[derive(Clone, PartialEq, Eq)]
struct Binding {
    mutable: bool,
    value: Access,
}
#[derive(Clone)]
struct Deferred<'a> {
    body: &'a Block,
    scopes: Vec<HashMap<String, usize>>,
}
#[derive(Clone)]
struct State<'a> {
    bindings: BTreeMap<usize, Binding>,
    scopes: Vec<HashMap<String, usize>>,
    globals: BTreeMap<String, Binding>,
    heap: Heap,
    allocations: BTreeSet<Root>,
    many: BTreeSet<Root>,
    defers: Vec<Deferred<'a>>,
    alive: bool,
}
impl Default for State<'_> {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            scopes: vec![HashMap::new()],
            globals: BTreeMap::new(),
            heap: BTreeMap::new(),
            allocations: BTreeSet::new(),
            many: BTreeSet::new(),
            defers: vec![],
            alive: true,
        }
    }
}
impl State<'_> {
    fn join(&mut self, other: &Self) {
        self.allocations.extend(other.allocations.iter().cloned());
        self.many.extend(other.many.iter().cloned());
        for (id, b) in &other.bindings {
            self.bindings
                .entry(*id)
                .and_modify(|v| v.value.union(&b.value))
                .or_insert_with(|| b.clone());
        }
        for (name, b) in &other.globals {
            self.globals
                .entry(name.clone())
                .and_modify(|v| v.value.union(&b.value))
                .or_insert_with(|| b.clone());
        }
        for (path, value) in &other.heap {
            // Missing symbolic cells are unknown, not empty. A branch which
            // does not replace a field retains its incoming alias capability.
            if !self.heap.contains_key(path) && !matches!(path.root, Root::Fresh(..)) {
                let old = symbolic(path.clone());
                self.heap.insert(path.clone(), old);
            }
            self.heap.entry(path.clone()).or_default().union(value);
        }
        for (path, value) in &mut self.heap {
            if !other.heap.contains_key(path) && !matches!(path.root, Root::Fresh(..)) {
                value.union(&symbolic(path.clone()));
            }
        }
    }
}
fn symbolic(path: Path) -> Access {
    Access {
        refs: [Reference {
            path,
            readonly: BTreeSet::new(),
            locals: BTreeSet::new(),
        }]
        .into(),
    }
}
#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    returns: bool,
    mutations: BTreeSet<Path>,
    returned: Access,
    heap: Heap,
    globals: BTreeMap<String, Access>,
    allocations: BTreeSet<Root>,
    many: BTreeSet<Root>,
}
impl Summary {
    fn join(&mut self, other: Self) {
        self.returns |= other.returns;
        self.mutations.extend(other.mutations);
        self.returned.union(&other.returned);
        self.allocations.extend(other.allocations);
        self.many.extend(other.many);
        for (path, value) in other.heap {
            self.heap.entry(path).or_default().union(&value);
        }
        for (name, value) in other.globals {
            self.globals.entry(name).or_default().union(&value);
        }
    }
}
struct Local {
    name: String,
    span: Option<SourceSpan>,
    valid: bool,
}
struct Loop<'a> {
    cleanup: usize,
    breaks: Vec<State<'a>>,
    continues: Vec<State<'a>>,
}
struct Pass<'a> {
    facts: &'a Facts,
    functions: HashMap<String, &'a FunctionDecl>,
    constructors: HashMap<String, &'a [StructField]>,
    summaries: &'a BTreeMap<String, Summary>,
    state: State<'a>,
    summary: Summary,
    locals: BTreeMap<usize, Local>,
    mutated: BTreeSet<usize>,
    attempted: BTreeSet<usize>,
    loops: Vec<Loop<'a>>,
    diagnostics: Vec<Diagnostic>,
    emit: bool,
    location: Option<SourceSpan>,
    persistent: BTreeSet<Root>,
    observations: Vec<State<'a>>,
}

pub(crate) fn check(program: &Program, facts: &Facts) -> Vec<Diagnostic> {
    let functions: HashMap<_, _> = program
        .items
        .iter()
        .flat_map(|item| match item {
            Item::Function(function) => vec![(function.name.clone(), function)],
            Item::Extend(extension) => extension
                .methods
                .iter()
                .map(|method| (method.function.name.clone(), &method.function))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    let mut constructors = HashMap::new();
    for item in &program.items {
        match item {
            Item::StructDecl { name, fields, .. } => {
                constructors.insert(name.clone(), fields.as_slice());
            }
            Item::EnumDecl { name, variants, .. } => {
                for v in variants {
                    constructors.insert(format!("{name}::{}", v.name), v.payload.as_slice());
                }
            }
            _ => {}
        }
    }
    let mut summaries: BTreeMap<_, _> = functions
        .keys()
        .map(|name| (name.clone(), Summary::default()))
        .collect();
    // Monotone fixed point, including mutually recursive helpers. Effects are
    // compiler data, not mandatory source annotations or runtime string lookup.
    loop {
        let mut next = summaries.clone();
        for function in functions.values() {
            let mut p = Pass::new(facts, &functions, &constructors, &summaries, false);
            p.globals(program);
            p.function(function);
            next.entry(function.name.clone())
                .or_default()
                .join(p.summary);
        }
        if next == summaries {
            break;
        }
        summaries = next;
    }
    let mut all_locals = BTreeMap::new();
    let mut mutated = BTreeSet::new();
    let mut attempted = BTreeSet::new();
    let mut diagnostics = vec![];
    for function in functions.values() {
        let mut p = Pass::new(facts, &functions, &constructors, &summaries, true);
        p.globals(program);
        p.function(function);
        diagnostics.extend(p.diagnostics);
        all_locals.extend(p.locals);
        mutated.extend(p.mutated);
        attempted.extend(p.attempted);
    }
    // Also check defaults never used by a call and global initializer effects.
    let mut p = Pass::new(facts, &functions, &constructors, &summaries, true);
    p.globals(program);
    for fields in constructors
        .values()
        .copied()
        .chain(functions.values().map(|f| f.params.as_slice()))
    {
        for field in fields {
            if let Some(expr) = &field.default {
                p.expr(expr);
                p.state.alive = true;
            }
        }
    }
    diagnostics.extend(p.diagnostics);
    for (id, local) in all_locals {
        if local.valid && !mutated.contains(&id) && !attempted.contains(&id) {
            diagnostics.push(Diagnostic::new(
                "E1006",
                format!(
                    "local var `{}` requires a reachable mutation; use const",
                    local.name
                ),
                local.span,
            ));
        }
    }
    diagnostics.sort_by_key(|d| {
        (
            d.span
                .as_ref()
                .map(|s| (s.source.name.clone(), s.range.start)),
            d.code,
            d.message.clone(),
        )
    });
    diagnostics.dedup();
    diagnostics
}

impl<'a> Pass<'a> {
    fn new(
        facts: &'a Facts,
        functions: &HashMap<String, &'a FunctionDecl>,
        constructors: &HashMap<String, &'a [StructField]>,
        summaries: &'a BTreeMap<String, Summary>,
        emit: bool,
    ) -> Self {
        Self {
            facts,
            functions: functions.clone(),
            constructors: constructors.clone(),
            summaries,
            state: State::default(),
            summary: Summary::default(),
            locals: BTreeMap::new(),
            mutated: BTreeSet::new(),
            attempted: BTreeSet::new(),
            loops: vec![],
            diagnostics: vec![],
            emit,
            location: None,
            persistent: BTreeSet::new(),
            observations: vec![],
        }
    }
    fn kind(&self, expr: &Expr) -> ValueKind {
        self.facts
            .values
            .get(&expr_id(expr))
            .copied()
            .unwrap_or(ValueKind::Reference)
    }
    fn define(&mut self, name: &str, id: usize, mutable: bool, value: Access) {
        self.state.bindings.insert(id, Binding { mutable, value });
        self.state
            .scopes
            .last_mut()
            .unwrap()
            .insert(name.into(), id);
    }
    fn local_id(&self, name: &str) -> Option<usize> {
        self.state
            .scopes
            .iter()
            .rev()
            .find_map(|s| s.get(name).copied())
    }
    fn globals(&mut self, program: &'a Program) {
        for item in &program.items {
            if let Item::GlobalConst { name, .. } | Item::GlobalVar { name, .. } = item {
                self.state.globals.insert(
                    name.clone(),
                    Binding {
                        mutable: matches!(item, Item::GlobalVar { .. }),
                        value: Access::root(Root::Global(name.clone())),
                    },
                );
            }
        }
        for item in &program.items {
            if let Item::GlobalConst { name, value, .. } | Item::GlobalVar { name, value, .. } =
                item
            {
                let access = self.expr(value);
                self.state.globals.get_mut(name).unwrap().value = access;
                self.state.alive = true;
            }
        }
        // Initialization is validated but is not part of a function's effects.
        self.persistent.extend(
            self.state
                .globals
                .values()
                .flat_map(|b| b.value.refs.iter().map(|r| r.path.root.clone())),
        );
        self.persistent
            .extend(self.state.heap.keys().map(|p| p.root.clone()));
        self.summary = Summary::default();
    }
    fn function(&mut self, f: &'a FunctionDecl) {
        self.state.scopes.push(HashMap::new());
        for (index, param) in f.params.iter().enumerate() {
            let value = if param.ty.as_ref().is_none_or(annotation_may_reference) {
                Access::root(Root::Parameter(index))
            } else {
                Access::default()
            };
            self.define(&param.name, param as *const Param as usize, true, value);
        }
        let value = self.block(&f.body);
        if self.state.alive {
            self.summary.returned.union(&value);
            self.summary.returns = true;
            self.save_heap();
        }
        // Heap cells are templates for reference-containing returned objects and
        // stores through parameters. Local identity never becomes an ABI feature.
    }
    fn save_heap(&mut self) {
        self.summary.many.extend(self.state.many.iter().cloned());
        for (path, access) in &self.state.heap {
            self.summary
                .heap
                .entry(path.clone())
                .or_default()
                .union(access);
        }
    }
    fn read(&self, expr: &Expr, name: &str) -> Access {
        if let Some(id) = self.local_id(name) {
            let b = &self.state.bindings[&id];
            let value = if b.mutable {
                b.value.clone()
            } else {
                b.value.readonly(name)
            };
            if self.locals.contains_key(&id) && b.mutable {
                value.map(|r| {
                    r.locals.insert(id);
                })
            } else {
                value
            }
        } else {
            let name = self
                .facts
                .globals
                .get(&expr_id(expr))
                .map_or(name, String::as_str);
            self.state
                .globals
                .get(name)
                .map_or_else(Access::default, |b| {
                    let mut value = b.value.clone();
                    // Keep the binding origin in effects even if the initializer is
                    // fresh: another function can reassign this global before use.
                    value.union(&Access::root(Root::Global(name.into())));
                    if b.mutable {
                        value
                    } else {
                        value.readonly(name)
                    }
                })
        }
    }
    fn project(&self, access: &Access, key: &Key) -> Access {
        if key == &Key::Descendant {
            let mut result = access.clone();
            let mut pending: Vec<_> = access.refs.iter().cloned().collect();
            let mut visited = BTreeSet::new();
            while let Some(reference) = pending.pop() {
                if !visited.insert(reference.path.clone()) {
                    continue;
                }
                for (path, value) in &self.state.heap {
                    if path.root == reference.path.root
                        && path.keys.starts_with(&reference.path.keys)
                    {
                        let value = value.map(|r| {
                            r.readonly.extend(reference.readonly.iter().cloned());
                            r.locals.extend(&reference.locals);
                        });
                        pending.extend(value.refs.iter().cloned());
                        result.union(&value);
                    }
                }
            }
            return result;
        }
        let mut result = Access::default();
        for reference in &access.refs {
            let child = reference.path.child(key.clone());
            let mut value = self
                .state
                .heap
                .get(&child)
                .cloned()
                .unwrap_or_else(|| symbolic(child.clone()));
            for (path, stored) in &self.state.heap {
                if path.root != child.root {
                    continue;
                }
                let same_parent = path.keys.len() == child.keys.len()
                    && path.keys[..path.keys.len().saturating_sub(1)]
                        == child.keys[..child.keys.len().saturating_sub(1)];
                if same_parent
                    && (key == &Key::AnyIndex || path.keys.last() == Some(&Key::AnyIndex))
                    && !matches!(path.keys.last(), Some(Key::Field(_)))
                {
                    value.union(stored);
                }
                if path.keys.last() == Some(&Key::Descendant)
                    && child.keys.starts_with(&path.keys[..path.keys.len() - 1])
                {
                    value.union(stored);
                }
            }
            result.union(&value.map(|r| {
                r.readonly.extend(reference.readonly.iter().cloned());
                r.locals.extend(&reference.locals);
            }));
        }
        result
    }
    fn store(&mut self, access: &Access, key: Key, value: Access, strong: bool) {
        let strong =
            strong && access.refs.len() == 1 && !matches!(key, Key::AnyIndex | Key::Descendant);
        for reference in &access.refs {
            let path = reference.path.child(key.clone());
            if strong && !self.state.many.contains(&reference.path.root) {
                self.state.heap.insert(path, value.clone());
            } else {
                self.state.heap.entry(path).or_default().union(&value);
            }
        }
    }
    fn allocate(&mut self, root: Root) -> Access {
        if !self.state.allocations.insert(root.clone()) {
            self.state.many.insert(root.clone());
        }
        if self.state.alive {
            self.summary.allocations.insert(root.clone());
        }
        Access::root(root)
    }
    fn mutate(&mut self, access: &Access, expr: &Expr) {
        if self.kind(expr) == ValueKind::Invalid {
            return;
        }
        let readonly: BTreeSet<_> = access
            .refs
            .iter()
            .flat_map(|r| r.readonly.iter().cloned())
            .collect();
        if !readonly.is_empty() && self.emit {
            self.diagnostics.push(Diagnostic::new(
                "E1003",
                format!(
                    "cannot mutate through const/read-only access from `{}`",
                    readonly.into_iter().collect::<Vec<_>>().join("`, `")
                ),
                expr.span().cloned().or_else(|| self.location.clone()),
            ));
        }
        if self.state.alive {
            for r in &access.refs {
                self.attempted.extend(&r.locals);
                if r.readonly.is_empty() {
                    self.mutated.extend(&r.locals);
                    self.summary.mutations.insert(r.path.clone());
                }
            }
        }
    }
    fn cleanup(&mut self, from: usize) {
        while self.state.defers.len() > from {
            let deferred = self.state.defers.pop().unwrap();
            let scopes = std::mem::replace(&mut self.state.scopes, deferred.scopes);
            let loops = std::mem::take(&mut self.loops);
            self.block(deferred.body);
            self.loops = loops;
            self.state.scopes = scopes;
        }
    }
    fn block(&mut self, block: &'a Block) -> Access {
        let base = self.state.defers.len();
        self.state.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        let value = block
            .tail
            .as_ref()
            .map_or_else(Access::default, |e| self.expr(e));
        if self.state.alive {
            self.cleanup(base);
        }
        self.state.defers.truncate(base);
        self.state.scopes.pop();
        value
    }
    fn stmt(&mut self, stmt: &'a Stmt) {
        let location = self.location.clone();
        self.location = stmt.span().cloned().or(location.clone());
        match stmt.node() {
            Stmt::VarDecl { name, value, .. }
            | Stmt::Const { name, value, .. }
            | Stmt::Comptime { name, value, .. } => {
                let reachable = self.state.alive;
                let access = self.expr(value);
                let id = stmt_id(stmt);
                let mutable = matches!(stmt.node(), Stmt::VarDecl { .. });
                if mutable && reachable {
                    self.locals.insert(
                        id,
                        Local {
                            name: name.clone(),
                            span: stmt.span().cloned(),
                            valid: self.kind(value) != ValueKind::Invalid
                                && !self.facts.invalid_bindings.contains(&id),
                        },
                    );
                }
                self.define(name, id, mutable, access);
            }
            Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
                let compound = matches!(stmt.node(), Stmt::CompoundAssign { .. });
                match target.node() {
                    Expr::Ident(name) => {
                        let left = self.read(target, name);
                        let right = self.expr(value);
                        let access = self.compound_value(stmt, left, right);
                        if let Some(id) = self.local_id(name) {
                            if self.state.alive && self.state.bindings[&id].mutable {
                                self.attempted.insert(id);
                            }
                            if self.state.bindings[&id].mutable
                                && self.kind(value) != ValueKind::Invalid
                            {
                                if self.state.alive {
                                    self.mutated.insert(id);
                                }
                                self.state.bindings.get_mut(&id).unwrap().value = access;
                            }
                        } else {
                            let name = self
                                .facts
                                .globals
                                .get(&expr_id(target))
                                .unwrap_or(name)
                                .clone();
                            if self.state.alive
                                && self.state.globals.get(&name).is_some_and(|b| b.mutable)
                            {
                                self.state.globals.get_mut(&name).unwrap().value = access.clone();
                                self.summary.globals.entry(name).or_default().union(&access);
                            }
                        }
                    }
                    Expr::Index(receiver, index) => {
                        let parent = self.expr(receiver);
                        self.expr(index);
                        let left = self.project(&parent, &index_key(index));
                        let right = self.expr(value);
                        let access = self.compound_value(stmt, left, right);
                        self.mutate(&parent, receiver);
                        self.store(&parent, index_key(index), access, !compound);
                    }
                    _ => {
                        self.expr(target);
                        self.expr(value);
                    }
                }
            }
            Stmt::Return(value) => {
                let value = value
                    .as_ref()
                    .map_or_else(Access::default, |e| self.expr(e));
                if self.state.alive {
                    self.summary.returned.union(&value);
                    self.summary.returns = true;
                    self.cleanup(0);
                    self.save_heap();
                }
                self.state.alive = false;
            }
            Stmt::Expr(expr) => {
                self.expr(expr);
            }
            Stmt::Crash { value, .. } => {
                self.expr(value);
                self.state.alive = false;
            }
            Stmt::Defer { body } => {
                // Check even dead code, but count effects at actual lexical exits,
                // with current alias capabilities rather than registration values.
                let saved = self.state.clone();
                self.state.alive = false;
                self.block(body);
                self.state = saved;
                if self.state.alive {
                    self.state.defers.push(Deferred {
                        body,
                        scopes: self.state.scopes.clone(),
                    });
                }
            }
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.try_blocks(body, catch_name, catch_block);
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                self.expr(cond);
                self.branches(
                    bool_literal(cond),
                    |p| p.block(then_block),
                    |p| {
                        else_block
                            .as_ref()
                            .map_or_else(Access::default, |b| p.block(b))
                    },
                );
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                let access = self.expr(value);
                self.branches(
                    None,
                    |p| {
                        p.state.scopes.push(HashMap::new());
                        p.define(name, stmt_id(stmt), true, access);
                        let result = p.block(then_block);
                        p.state.scopes.pop();
                        result
                    },
                    |p| {
                        else_block
                            .as_ref()
                            .map_or_else(Access::default, |b| p.block(b))
                    },
                );
            }
            Stmt::While { cond, body } => {
                self.loop_body(Some(cond), body, None);
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.expr(start);
                self.expr(end);
                if let Some(e) = step {
                    self.expr(e);
                }
                self.state.scopes.push(HashMap::new());
                self.define(var, stmt_id(stmt), true, Access::default());
                self.loop_body(None, body, range_empty(start, end, step.as_ref()));
                self.state.scopes.pop();
            }
            Stmt::ForEach {
                var,
                iterable,
                body,
            } => {
                let collection = self.expr(iterable);
                let item = self.project(&collection, &Key::AnyIndex);
                self.state.scopes.push(HashMap::new());
                self.define(var, stmt_id(stmt), true, item);
                self.loop_body(None, body, None);
                self.state.scopes.pop();
            }
            Stmt::Break => {
                if self.state.alive && !self.loops.is_empty() {
                    let base = self.loops.last().unwrap().cleanup;
                    self.cleanup(base);
                    let state = self.state.clone();
                    self.loops.last_mut().unwrap().breaks.push(state);
                }
                self.state.alive = false;
            }
            Stmt::Continue => {
                if self.state.alive && !self.loops.is_empty() {
                    let base = self.loops.last().unwrap().cleanup;
                    self.cleanup(base);
                    let state = self.state.clone();
                    self.loops.last_mut().unwrap().continues.push(state);
                }
                self.state.alive = false;
            }
            Stmt::Invalid { binding } => {
                if let Some(name) = binding {
                    self.define(name, stmt_id(stmt), true, Access::default());
                }
            }
            Stmt::Use { .. } => {}
            Stmt::Located(..) => unreachable!(),
        }
        self.location = location;
        self.observe();
    }
    fn observe(&mut self) {
        if self.state.alive {
            for entry in &mut self.observations {
                entry.join(&self.state);
            }
        }
    }
    fn compound_value(&mut self, stmt: &Stmt, left: Access, right: Access) -> Access {
        if matches!(stmt.node(), Stmt::CompoundAssign { op: BinOp::Add, .. })
            && (!left.refs.is_empty() || !right.refs.is_empty())
        {
            let result = self.allocate(Root::Fresh(stmt_id(stmt), 0));
            let mut children = self.project(&left, &Key::AnyIndex);
            children.union(&self.project(&right, &Key::AnyIndex));
            self.store(&result, Key::AnyIndex, children, false);
            result
        } else {
            right
        }
    }
    fn branches(
        &mut self,
        known: Option<bool>,
        then: impl FnOnce(&mut Self) -> Access,
        otherwise: impl FnOnce(&mut Self) -> Access,
    ) -> Access {
        let initial = self.state.clone();
        self.state.alive &= known != Some(false);
        let a = then(self);
        let first = self.state.clone();
        self.state = initial.clone();
        self.state.alive &= known != Some(true);
        let b = otherwise(self);
        let second = self.state.clone();
        let mut result = Access::default();
        if first.alive {
            result.union(&a);
        }
        if second.alive {
            result.union(&b);
        }
        self.state = if first.alive {
            first.clone()
        } else {
            second.clone()
        };
        if first.alive && second.alive {
            self.state.join(&second);
        }
        if !first.alive && !second.alive {
            self.state = initial;
            self.state.alive = false;
            self.state.join(&first);
            self.state.join(&second);
        }
        result
    }
    fn try_blocks(&mut self, body: &'a Block, name: &str, catch: &'a Block) -> Access {
        let initial = self.state.clone();
        self.observations.push(initial.clone());
        let success = self.block(body);
        let normal = self.state.clone();
        self.state = self.observations.pop().unwrap();
        self.state.scopes.push(HashMap::new());
        self.define(
            name,
            catch as *const Block as usize,
            false,
            Access::default(),
        );
        let failure = self.block(catch);
        self.state.scopes.pop();
        let exceptional = self.state.clone();
        if normal.alive {
            self.state = normal;
            if exceptional.alive {
                self.state.join(&exceptional);
            }
        }
        let mut value = success;
        if exceptional.alive {
            value.union(&failure);
        }
        value
    }
    fn loop_body(&mut self, cond: Option<&'a Expr>, body: &'a Block, empty: Option<bool>) {
        let initial = self.state.clone();
        let dead = empty == Some(true) || cond.and_then(bool_literal) == Some(false);
        self.loops.push(Loop {
            cleanup: self.state.defers.len(),
            breaks: vec![],
            continues: vec![],
        });
        loop {
            let before = self.state.clone();
            if let Some(c) = cond {
                self.expr(c);
            }
            let condition_returns = self.state.alive;
            self.state.alive &= !dead;
            self.block(body);
            let mut after = self.state.clone();
            let continues = std::mem::take(&mut self.loops.last_mut().unwrap().continues);
            for mut state in continues {
                state.scopes = before.scopes.clone();
                if after.alive {
                    after.join(&state);
                } else {
                    after = state;
                }
            }
            if !condition_returns || (cond.is_none() && empty == Some(false) && !after.alive) {
                break;
            }
            self.state = before.clone();
            if after.alive {
                self.state.join(&after);
            }
            if dead
                || !after.alive
                || (before.bindings == self.state.bindings
                    && before.allocations == self.state.allocations
                    && before.many == self.state.many
                    && before.heap == self.state.heap
                    && before.globals == self.state.globals)
            {
                break;
            }
        }
        let mut exits = self.loops.pop().unwrap().breaks;
        for exit in &mut exits {
            exit.scopes = initial.scopes.clone();
        }
        if cond.and_then(bool_literal) == Some(true) {
            self.state = initial;
            self.state.alive = false;
            for exit in exits {
                if !self.state.alive {
                    self.state = exit;
                } else {
                    self.state.join(&exit);
                }
            }
        } else {
            for exit in exits {
                if !self.state.alive {
                    self.state = exit;
                } else {
                    self.state.join(&exit);
                }
            }
        }
    }
    fn expr(&mut self, expr: &'a Expr) -> Access {
        let location = self.location.clone();
        self.location = expr.span().cloned().or(location.clone());
        let result = self.expr_inner(expr);
        let result = match self.kind(expr) {
            ValueKind::Scalar | ValueKind::Invalid => Access::default(),
            ValueKind::Never => {
                self.state.alive = false;
                Access::default()
            }
            ValueKind::Reference => result,
        };
        self.location = location;
        self.observe();
        result
    }
    fn expr_inner(&mut self, expr: &'a Expr) -> Access {
        match expr.node() {
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let StringPart::Expression(value) = part {
                        self.expr(value);
                    }
                }
                Access::default()
            }
            Expr::Ident(name) => self.read(expr, name),
            Expr::Array(items) | Expr::Tuple(items) => {
                let result = self.allocate(Root::Fresh(expr_id(expr), 0));
                for (i, item) in items.iter().enumerate() {
                    let value = self.expr(item);
                    self.store(&result, Key::Index(i as i64), value, true);
                }
                result
            }
            Expr::Record(fields) => {
                let result = self.allocate(Root::Fresh(expr_id(expr), 0));
                for (name, expr) in fields {
                    let value = self.expr(expr);
                    self.store(&result, Key::Field(name.clone()), value, true);
                }
                result
            }
            Expr::Index(value, index) => {
                let access = self.expr(value);
                self.expr(index);
                self.project(&access, &index_key(index))
            }
            Expr::Binary(op, left, right) => {
                let a = self.expr(left);
                if matches!(op, BinOp::And | BinOp::Or) {
                    let skip = bool_literal(left).map(|b| if *op == BinOp::And { !b } else { b });
                    self.branches(skip, |_| Access::default(), |p| p.expr(right));
                    return Access::default();
                }
                let b = self.expr(right);
                match op {
                    BinOp::Append => {
                        self.mutate(&a, left);
                        self.store(&a, Key::AnyIndex, b, false);
                        a
                    }
                    BinOp::Prepend => {
                        self.mutate(&b, right);
                        self.store(&b, Key::AnyIndex, a, false);
                        b
                    }
                    // Array addition allocates a new outer container, retaining
                    // capabilities of reference-like elements.
                    BinOp::Add => {
                        let result = self.allocate(Root::Fresh(expr_id(expr), 0));
                        let mut children = self.project(&a, &Key::AnyIndex);
                        children.union(&self.project(&b, &Key::AnyIndex));
                        self.store(&result, Key::AnyIndex, children, false);
                        result
                    }
                    _ => Access::default(),
                }
            }
            Expr::MethodCall(receiver, method, args) => {
                let value = self.expr(receiver);
                let mut arguments = vec![];
                for arg in args {
                    arguments.push(self.expr(arg));
                }
                if self.facts.structural_clones.contains(&expr_id(expr)) {
                    return self.allocate(Root::Fresh(expr_id(expr), 0));
                }
                let specialized = self.facts.specializations.get(&expr_id(expr));
                let pending_specialization =
                    specialized.is_some_and(|name| !self.summaries.contains_key(name));
                if pending_specialization {
                    for reference in &value.refs {
                        self.attempted.extend(&reference.locals);
                    }
                }
                let target = specialized
                    .filter(|name| self.summaries.contains_key(*name))
                    .or_else(|| self.facts.calls.get(&expr_id(expr)));
                if let Some(name) = target
                    && let Some(summary) = self.summaries.get(name).cloned()
                {
                    if self.facts.mutating_calls.contains(&expr_id(expr))
                        && !summary
                            .mutations
                            .iter()
                            .any(|path| path.root == Root::Parameter(0))
                    {
                        self.mutate(&value, receiver);
                    }
                    let mut call_arguments = Vec::with_capacity(arguments.len() + 1);
                    call_arguments.push(value);
                    call_arguments.extend(arguments);
                    return self.apply_summary(expr, summary, &call_arguments);
                }
                if self.facts.mutating_calls.contains(&expr_id(expr)) {
                    self.mutate(&value, receiver);
                }
                match method.as_str() {
                    "empty_like" => self.allocate(Root::Fresh(expr_id(expr), 0)),
                    "append" | "prepend" => {
                        self.mutate(&value, receiver);
                        if let Some(item) = arguments.first() {
                            self.store(&value, Key::AnyIndex, item.clone(), false);
                        }
                        value
                    }
                    "unwrap" | "expect" => value,
                    _ => Access::default(),
                }
            }
            Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                self.call(expr, name, args)
            }
            Expr::Block(block) => self.block(block),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond);
                self.branches(
                    bool_literal(cond),
                    |p| p.expr(then_branch),
                    |p| {
                        else_branch
                            .as_deref()
                            .map_or_else(Access::default, |b| p.expr(b))
                    },
                )
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => self.try_blocks(body, catch_name, catch_block),
            Expr::Match { value, arms, .. } => {
                let value = self.expr(value);
                let initial = self.state.clone();
                let mut joined: Option<State<'a>> = None;
                let mut result = Access::default();
                for arm in arms {
                    self.state = initial.clone();
                    self.state.scopes.push(HashMap::new());
                    if let MatchPattern::Variant { bindings, .. } = &arm.pattern {
                        for (i, name) in bindings.iter().enumerate() {
                            if name != "_" {
                                let access = self.project(&value, &Key::Index(i as i64));
                                self.define(name, name as *const String as usize, false, access);
                            }
                        }
                    }
                    if let Some(guard) = &arm.guard {
                        self.expr(guard);
                        self.state.alive &= bool_literal(guard) != Some(false);
                    }
                    let v = self.block(&arm.body);
                    self.state.scopes.pop();
                    if self.state.alive {
                        result.union(&v);
                        if let Some(joined) = &mut joined {
                            joined.join(&self.state);
                        } else {
                            joined = Some(self.state.clone());
                        }
                    }
                }
                if let Some(joined) = joined {
                    self.state = joined;
                } else {
                    self.state = initial;
                    self.state.alive = false;
                }
                result
            }
            Expr::Propagate(inner) => {
                let entered = self.state.alive;
                let result = self.expr(inner);
                // Propagation may exit here; inspect cleanup with this state,
                // while keeping the normal successor and its unchanged value.
                if self.state.alive {
                    let saved = self.state.clone();
                    self.cleanup(0);
                    self.save_heap();
                    self.state = saved;
                }
                self.summary.returns |= entered;
                result
            }
            Expr::Fallback {
                kind,
                value,
                fallback,
            } => {
                let access = self.expr(value);
                let known = if self.facts.fallback_possible.get(&expr_id(fallback)) == Some(&false)
                {
                    Some(true)
                } else {
                    match (kind, value.node()) {
                        (FallbackKind::Nil, Expr::Nil) => Some(false),
                        (
                            _,
                            Expr::Int(_)
                            | Expr::Float(_)
                            | Expr::Bool(_)
                            | Expr::Str(_)
                            | Expr::Array(_)
                            | Expr::Record(_),
                        ) => Some(true),
                        (FallbackKind::Error, Expr::Nil) => Some(true),
                        _ => None,
                    }
                };
                self.branches(known, |_| access, |p| p.expr(fallback))
            }
            Expr::Unary(_, value) | Expr::Cast(value, _) => {
                self.expr(value);
                Access::default()
            }
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Nil
            | Expr::TypeLiteral(_) => Access::default(),
            Expr::Bytes(_) => self.allocate(Root::Fresh(expr_id(expr), 0)),
            Expr::BracketApply(_, _) | Expr::Apply(_, _) => Access::default(),
            Expr::Located(..) => unreachable!(),
        }
    }
    fn substitute(&self, path: &Path, args: &[Access], site: usize) -> Access {
        let mut result = match &path.root {
            Root::Parameter(i) => args.get(*i).cloned().unwrap_or_default(),
            Root::Global(name) => self.state.globals.get(name).map_or_else(
                || Access::root(path.root.clone()),
                |b| {
                    if b.mutable {
                        b.value.clone()
                    } else {
                        b.value.readonly(name)
                    }
                },
            ),
            Root::Fresh(..) if self.persistent.contains(&path.root) => {
                Access::root(path.root.clone())
            }
            Root::Fresh(id, _) => Access::root(Root::Fresh(*id, site)),
        };
        for key in &path.keys {
            result = self.project(&result, key);
        }
        result
    }
    fn substitute_access(&self, value: &Access, args: &[Access], site: usize) -> Access {
        let mut result = Access::default();
        for reference in &value.refs {
            result.union(&self.substitute(&reference.path, args, site).map(|r| {
                r.readonly.extend(reference.readonly.iter().cloned());
                r.locals.extend(&reference.locals);
            }));
        }
        result
    }
    fn apply_heap(&mut self, summary: &Summary, args: &[Access], site: usize) {
        for (path, value) in &summary.heap {
            let Some((key, parents)) = path.keys.split_last() else {
                continue;
            };
            let access = self.substitute(
                &Path {
                    root: path.root.clone(),
                    keys: parents.into(),
                },
                args,
                site,
            );
            let value = self.substitute_access(value, args, site);
            self.store(
                &access,
                key.clone(),
                value,
                matches!(path.root, Root::Fresh(..)) && !self.persistent.contains(&path.root),
            );
        }
    }
    fn call(&mut self, expr: &'a Expr, name: &str, given: &'a [CallArgument]) -> Access {
        let name = self
            .facts
            .calls
            .get(&expr_id(expr))
            .map_or(name, String::as_str)
            .to_owned();
        let fields = self
            .functions
            .get(&name)
            .map(|f| f.params.as_slice())
            .or_else(|| self.constructors.get(&name).copied());
        let mut args = vec![Access::default(); fields.map_or(given.len(), <[StructField]>::len)];
        let bindings = fields.map(|fields| crate::arguments::bind(&name, fields, given));
        let mut seen = BTreeSet::new();
        for (i, argument) in given.iter().enumerate() {
            let index = bindings.as_ref().map_or(Some(i), |b| b.by_argument[i]);
            let access = self.expr(&argument.value);
            if let Some(index) = index
                && let Some(arg) = args.get_mut(index)
            {
                *arg = access;
                seen.insert(index);
            }
        }
        if let Some(fields) = fields {
            for (i, field) in fields.iter().enumerate() {
                if !seen.contains(&i)
                    && let Some(default) = &field.default
                {
                    let scopes = std::mem::replace(&mut self.state.scopes, vec![HashMap::new()]);
                    args[i] = self.expr(default);
                    self.state.scopes = scopes;
                }
            }
        }
        if self.kind(expr) == ValueKind::Invalid {
            return Access::default();
        }
        let specialized = self.facts.specializations.get(&expr_id(expr));
        let specialized_summary = specialized.and_then(|name| self.summaries.get(name));
        if specialized.is_some() && specialized_summary.is_none() {
            for argument in &args {
                for reference in &argument.refs {
                    self.attempted.extend(&reference.locals);
                }
            }
        }
        if self.constructors.contains_key(&name) {
            let result = self.allocate(Root::Fresh(expr_id(expr), 0));
            for (i, (field, value)) in fields.unwrap().iter().zip(args).enumerate() {
                self.store(&result, Key::Field(field.name.clone()), value.clone(), true);
                self.store(&result, Key::Index(i as i64), value, true);
            }
            return result;
        }
        if let Ok(import) = crate::runtime::NativeImport::named(&name) {
            for index in import.mutating_parameters() {
                if let Some(access) = args.get(*index) {
                    self.mutate(access, given.get(*index).map_or(expr, |arg| &arg.value));
                }
            }
            if name == "fmt::write" {
                return args.first().cloned().unwrap_or_default();
            }
            return self.allocate(Root::Fresh(expr_id(expr), 0));
        }
        if let Some(summary) = specialized_summary
            .or_else(|| self.summaries.get(&name))
            .cloned()
        {
            return self.apply_summary(expr, summary, &args);
        }
        Access::default()
    }

    fn apply_summary(&mut self, expr: &Expr, summary: Summary, args: &[Access]) -> Access {
        let site = expr_id(expr);
        for root in &summary.allocations {
            if let Root::Fresh(id, _) = root {
                self.allocate(Root::Fresh(*id, site));
            }
        }
        for root in &summary.many {
            let access = self.substitute(
                &Path {
                    root: root.clone(),
                    keys: vec![],
                },
                args,
                site,
            );
            self.state
                .many
                .extend(access.refs.into_iter().map(|r| r.path.root));
        }
        // Validate before and after writes because actual arguments may alias.
        for path in &summary.mutations {
            let access = self.substitute(path, args, site);
            self.mutate(&access, expr);
        }
        self.apply_heap(&summary, args, site);
        for path in &summary.mutations {
            let access = self.substitute(path, args, site);
            self.mutate(&access, expr);
        }
        for (name, value) in &summary.globals {
            let value = self.substitute_access(value, args, site);
            if let Some(binding) = self.state.globals.get_mut(name) {
                binding.value.union(&value);
            }
            self.summary
                .globals
                .entry(name.clone())
                .or_default()
                .union(&value);
        }
        let result = self.substitute_access(&summary.returned, args, site);
        if !summary.returns {
            self.state.alive = false;
        }
        result
    }
}
fn bool_literal(expr: &Expr) -> Option<bool> {
    match expr.node() {
        Expr::Bool(b) => Some(*b),
        _ => None,
    }
}
fn index_key(expr: &Expr) -> Key {
    match expr.node() {
        Expr::Int(i) => Key::Index(*i),
        Expr::Str(s) => Key::Field(s.clone()),
        _ => Key::AnyIndex,
    }
}
fn range_empty(start: &Expr, end: &Expr, step: Option<&Expr>) -> Option<bool> {
    match (start.node(), end.node(), step.map(Expr::node)) {
        (Expr::Int(a), Expr::Int(b), None) => Some(a >= b),
        (Expr::Int(a), Expr::Int(b), Some(Expr::Int(s))) => {
            Some(*s == 0 || if *s > 0 { a >= b } else { a <= b })
        }
        _ => None,
    }
}
