use crate::ast::{
    BinOp, Block, CallArgument, EnumVariant, Expr, FallbackKind, FunctionDecl, Item, MatchPattern,
    Param, Program, Stmt, StringPart, UnOp, VarType, Visibility,
};
use crate::backend::{CompiledProgram, Emitter, Label};
use crate::builtins::Builtin;
use crate::instruction::{Instruction, ValueType};
use crate::value::{RuntimeType, Value};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy)]
enum DeclaredCall {
    Function,
    Record,
    Variant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodegenError {
    pub message: String,
    pub diagnostics: Vec<crate::diagnostic::Diagnostic>,
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for diagnostic in &self.diagnostics {
            write!(f, "{diagnostic}")?;
        }
        Ok(())
    }
}

impl CodegenError {
    fn new(message: impl Into<String>) -> Self {
        Self::from_diagnostics(vec![crate::diagnostic::Diagnostic::new(
            "E3000", message, None,
        )])
    }

    fn from_diagnostics(diagnostics: Vec<crate::diagnostic::Diagnostic>) -> Self {
        let message = diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            message,
            diagnostics,
        }
    }

    fn at(mut self, span: Option<&crate::diagnostic::SourceSpan>) -> Self {
        for diagnostic in &mut self.diagnostics {
            if diagnostic.span.is_none() {
                diagnostic.span = span.cloned();
            }
        }
        self
    }
}

fn byte_literal_buffer(length: usize) -> Result<String, CodegenError> {
    let capacity = length
        .checked_mul(2)
        .ok_or_else(|| CodegenError::new("byte literal encoding size overflow"))?;
    let mut buffer = String::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| CodegenError::new("cannot allocate byte literal encoding"))?;
    Ok(buffer)
}

#[derive(Clone)]
struct Scopes {
    stack: Vec<HashMap<String, usize>>,
    imports: Vec<HashMap<String, String>>,
    modules: Vec<HashMap<String, String>>,
    // Binding properties use slot identity, so shadowing cannot leak constness or types.
    const_vars: std::collections::HashSet<usize>,
    next_slot: usize,
}

impl Scopes {
    fn new() -> Self {
        Scopes {
            stack: vec![HashMap::new()],
            imports: vec![HashMap::new()],
            modules: vec![HashMap::new()],
            const_vars: std::collections::HashSet::new(),
            next_slot: 0,
        }
    }

    fn push(&mut self) {
        self.stack.push(HashMap::new());
        self.imports.push(HashMap::new());
        self.modules.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.stack.pop();
        self.imports.pop();
        self.modules.pop();
    }

    fn mark_const(&mut self, name: &str) {
        self.const_vars
            .insert(self.resolve(name).expect("defined binding"));
    }

    fn is_const(&self, name: &str) -> bool {
        self.resolve(name)
            .is_some_and(|slot| self.const_vars.contains(&slot))
    }

    fn bind_module(&mut self, name: &str, target: &str) {
        self.modules
            .last_mut()
            .expect("scope stack is never empty")
            .insert(name.to_string(), target.to_string());
    }

    fn resolve_module(&self, name: &str) -> Option<String> {
        self.modules
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn bind_import(&mut self, name: &str, target: &str) {
        self.imports
            .last_mut()
            .expect("scope stack is never empty")
            .insert(name.to_string(), target.to_string());
    }

    fn resolve_import(&self, name: &str) -> Option<String> {
        self.imports
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn define(&mut self, name: &str) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.stack
            .last_mut()
            .expect("scope stack is never empty")
            .insert(name.to_string(), slot);
        slot
    }

    fn resolve(&self, name: &str) -> Option<usize> {
        self.stack.iter().rev().find_map(|s| s.get(name).copied())
    }
}

#[derive(Clone)]
struct FunctionSig {
    params: Vec<Param>,
    visibility: Visibility,
    module: Option<String>,
}

#[derive(Clone)]
struct GlobalSig {
    visibility: Visibility,
    module: Option<String>,
    mutable: bool,
}

#[derive(Clone)]
struct Deferred {
    id: Label,
    body: Block,
    scopes: Scopes,
}

#[derive(Clone)]
struct ErrorHandler {
    label: Label,
    slot: usize,
    stack_depth: usize,
    defer_baseline: usize,
    parent: Option<Box<ErrorHandler>>,
}

#[derive(Clone)]
struct CleanupBoundary {
    label: Label,
    primary_slot: usize,
    stack_depth: usize,
    defer_baseline: usize,
}

#[derive(Clone, Copy)]
struct LoopTransfer {
    label: Label,
    defer_baseline: usize,
    stack_depth: usize,
}

fn runtime_array_element_type(annotation: &crate::ast::TypeAnnotation) -> Option<RuntimeType> {
    if let crate::ast::TypeKind::Array(element) = &annotation.kind {
        runtime_type(element)
    } else {
        None
    }
}

fn runtime_type(annotation: &crate::ast::TypeAnnotation) -> Option<RuntimeType> {
    use crate::ast::{TypeKind, VarType};
    let mut ty = match &annotation.kind {
        TypeKind::Primitive(VarType::Int) => RuntimeType::Int,
        TypeKind::Primitive(VarType::Float) => RuntimeType::Float,
        TypeKind::Primitive(VarType::Bool) => RuntimeType::Bool,
        TypeKind::Primitive(VarType::Str) => RuntimeType::Str,
        TypeKind::Primitive(VarType::Nil) => RuntimeType::Nil,
        TypeKind::Array(element) => RuntimeType::Array(Box::new(runtime_type(element)?)),
        TypeKind::Named(name) => RuntimeType::Nominal(name.clone()),
        TypeKind::ResolvedName { symbol, .. } => RuntimeType::Nominal(symbol.clone()),
        TypeKind::Applied { .. } | TypeKind::ResolvedApplied { .. } => {
            RuntimeType::Nominal(annotation.canonical())
        }
        TypeKind::TypeParameter(_) | TypeKind::SelfType => return None,
        TypeKind::Tuple(items) => {
            RuntimeType::Tuple(items.iter().map(runtime_type).collect::<Option<Vec<_>>>()?)
        }
    };
    if annotation.nilable {
        ty = RuntimeType::Nullable(Box::new(ty));
    }
    Some(ty)
}

struct Codegen<'a> {
    functions: &'a HashMap<String, FunctionSig>,
    method_calls: &'a HashMap<crate::analysis::CallSite, String>,
    call_specializations: &'a HashMap<crate::analysis::CallSite, String>,
    struct_call_specializations: &'a HashMap<crate::analysis::CallSite, String>,
    current_function: Option<String>,
    current_module: Option<String>,
    struct_defs: HashMap<String, Vec<crate::ast::StructField>>,
    struct_visibility: HashMap<String, Visibility>,
    enum_defs: HashMap<String, EnumVariant>,
    enum_visibility: HashMap<String, Visibility>,
    globals: &'a HashMap<String, GlobalSig>,
    declaration_uses: &'a [crate::ast::UseDecl],
    global_indices: &'a HashMap<String, usize>,
    out: Emitter,
    loop_end_stack: Vec<LoopTransfer>,
    loop_continue_stack: Vec<LoopTransfer>,
    active_defers: Vec<Deferred>,
    error_handler: Option<ErrorHandler>,
    // Snapshot normal result or primary Error while executing exited cleanup.
    cleanup_primary: Option<usize>,
    cleanup_boundary: Option<CleanupBoundary>,
    function_result_type: Option<crate::ast::TypeAnnotation>,
    in_function: bool,
}

pub fn compile(program: &Program) -> Result<CompiledProgram, CodegenError> {
    compile_with_modules(program, &HashMap::new())
}

/// Same as `compile`, but additionally receives the file/module ownership
/// map supplied by callers. Functions are already renamed to their
/// fully-qualified `module::name` labels before codegen; the map also records
/// the module of `main` so private/public access can be checked correctly.
pub fn compile_with_modules(
    program: &Program,
    function_modules: &HashMap<String, String>,
) -> Result<CompiledProgram, CodegenError> {
    let (program, checked) = crate::generics::expand(program, function_modules)
        .map_err(CodegenError::from_diagnostics)?;
    let program = &program;
    if program
        .items
        .iter()
        .any(|item| matches!(item, Item::Comptime { .. }))
    {
        return Err(CodegenError::new(
            "compile-time declarations must be evaluated before code generation",
        ));
    }
    if !checked.diagnostics.is_empty() {
        return Err(CodegenError::from_diagnostics(checked.diagnostics));
    }
    let mut functions: HashMap<String, FunctionSig> = HashMap::new();
    let mut struct_defs: HashMap<String, Vec<crate::ast::StructField>> = HashMap::new();
    let mut struct_visibility: HashMap<String, Visibility> = HashMap::new();
    let mut enum_defs: HashMap<String, EnumVariant> = HashMap::new();
    let mut enum_visibility: HashMap<String, Visibility> = HashMap::new();
    let mut globals: HashMap<String, GlobalSig> = HashMap::new();
    let mut global_indices: HashMap<String, usize> = HashMap::new();
    let mut global_imports: HashMap<String, String> = HashMap::new();

    for item in &program.items {
        match item {
            Item::Function(f) => {
                if f.comptime || !f.type_params.is_empty() {
                    continue;
                }
                if functions
                    .insert(
                        f.name.clone(),
                        FunctionSig {
                            params: f.params.clone(),
                            visibility: f.visibility,
                            module: function_modules
                                .get(&f.name)
                                .cloned()
                                .or_else(|| f.name.rsplit_once("::").map(|(m, _)| m.to_string())),
                        },
                    )
                    .is_some()
                {
                    return Err(CodegenError::new(format!(
                        "function `{}` is defined more than once",
                        f.name
                    )));
                }
            }
            Item::Extend(extension) => {
                for method in &extension.methods {
                    let function = &method.function;
                    if function.comptime || !function.type_params.is_empty() {
                        continue;
                    }
                    functions.insert(
                        function.name.clone(),
                        FunctionSig {
                            params: function.params.clone(),
                            visibility: function.visibility,
                            module: function
                                .name
                                .rsplit_once("::")
                                .map(|(module, _)| module.to_owned()),
                        },
                    );
                }
            }
            Item::Trait(_) => {}
            Item::GlobalConst {
                name, visibility, ..
            } => {
                if globals.contains_key(name) {
                    return Err(CodegenError::new(format!(
                        "global `{name}` is defined more than once"
                    )));
                }
                let idx = global_indices.len();
                global_indices.insert(name.clone(), idx);
                globals.insert(
                    name.clone(),
                    GlobalSig {
                        visibility: *visibility,
                        module: function_modules
                            .get(name)
                            .cloned()
                            .or_else(|| name.rsplit_once("::").map(|(m, _)| m.to_string())),
                        mutable: false,
                    },
                );
            }
            Item::Comptime { .. } => unreachable!("checked above"),
            Item::GlobalVar {
                name, visibility, ..
            } => {
                if globals.contains_key(name) {
                    return Err(CodegenError::new(format!(
                        "global `{name}` is defined more than once"
                    )));
                }
                let idx = global_indices.len();
                global_indices.insert(name.clone(), idx);
                globals.insert(
                    name.clone(),
                    GlobalSig {
                        visibility: *visibility,
                        module: function_modules
                            .get(name)
                            .cloned()
                            .or_else(|| name.rsplit_once("::").map(|(m, _)| m.to_string())),
                        mutable: true,
                    },
                );
            }
            Item::StructDecl {
                name,
                visibility,
                type_params,
                fields,
                ..
            } => {
                if type_params.is_empty() {
                    struct_defs.insert(name.clone(), fields.clone());
                    struct_visibility.insert(name.clone(), *visibility);
                }
            }
            Item::EnumDecl {
                name,
                visibility,
                variants,
            } => {
                enum_visibility.insert(name.clone(), *visibility);
                for variant in variants {
                    let key = format!("{}::{}", name, variant.name);
                    enum_defs.insert(key, variant.clone());
                }
            }
            Item::Use(use_decl) => {
                for (name, alias) in use_decl.names.iter().zip(use_decl.aliases.iter()) {
                    let bind_name = alias.as_deref().unwrap_or(name.as_str());
                    let target = format!("{}::{}", use_decl.module, name);
                    if let Some(sig) = globals.get(&target)
                        && sig.visibility == Visibility::Private
                    {
                        return Err(CodegenError::new(format!(
                            "cannot import private global `{target}` from module `{}`",
                            use_decl.module
                        )));
                    }
                    global_imports.insert(bind_name.to_string(), target);
                }
            }
            Item::ComptimeInclude { .. } => {
                return Err(CodegenError::new(
                    "package inclusion must be resolved before code generation",
                ));
            }
            Item::Invalid { .. } => {
                return Err(CodegenError::new(
                    "internal error: invalid AST reached code generation",
                ));
            }
        }
    }

    let top_level_uses: Vec<crate::ast::UseDecl> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Use(decl) => Some(decl.clone()),
            _ => None,
        })
        .collect();

    if !functions.contains_key("main") {
        return Err(CodegenError::new(
            "no entry point: expected a function named `main`",
        ));
    }
    if !functions["main"].params.is_empty() {
        return Err(CodegenError::new("`main` must take no parameters"));
    }

    let mut cg = Codegen {
        functions: &functions,
        method_calls: &checked.method_calls,
        call_specializations: &checked.call_specializations,
        struct_call_specializations: &checked.struct_call_specializations,
        current_function: None,
        current_module: None,
        struct_defs,
        struct_visibility,
        enum_defs,
        enum_visibility,
        globals: &globals,
        declaration_uses: &top_level_uses,
        global_indices: &global_indices,
        out: Emitter::default(),
        loop_end_stack: Vec::new(),
        loop_continue_stack: Vec::new(),
        active_defers: Vec::new(),
        error_handler: None,
        cleanup_primary: None,
        cleanup_boundary: None,
        function_result_type: None,
        in_function: false,
    };

    // The resolver orders global declarations by initialization dependencies.
    for item in &program.items {
        match item {
            Item::GlobalConst {
                name, ty, value, ..
            }
            | Item::GlobalVar {
                name, ty, value, ..
            } => {
                let mut init_scopes = Scopes::new();
                for decl in &top_level_uses {
                    cg.apply_use_decl(decl, &mut init_scopes)?;
                }
                cg.current_module = globals.get(name).and_then(|g| g.module.clone());
                if let Some(ty) = ty.as_ref() {
                    cg.compile_expr_typed_array(value, ty, &mut init_scopes)?;
                } else {
                    cg.compile_expr(value, &mut init_scopes)?;
                }
                let idx = *global_indices.get(name).expect("global index");
                cg.out.emit(Instruction::StoreGlobal(idx));
            }
            _ => {}
        }
    }
    cg.out.call("main", 0);
    cg.out.emit(Instruction::Halt);
    for item in &program.items {
        match item {
            Item::Function(f) if !f.comptime && f.type_params.is_empty() => {
                cg.current_module = function_modules
                    .get(&f.name)
                    .cloned()
                    .or_else(|| functions.get(&f.name).and_then(|sig| sig.module.clone()));
                cg.compile_function(f, &global_imports, &top_level_uses)?;
            }
            Item::Extend(extension) => {
                for method in &extension.methods {
                    let f = &method.function;
                    if f.comptime || !f.type_params.is_empty() {
                        continue;
                    }
                    cg.current_module = functions.get(&f.name).and_then(|sig| sig.module.clone());
                    cg.compile_function(f, &global_imports, &top_level_uses)?;
                }
            }
            _ => {}
        }
    }

    cg.out.finish().map_err(CodegenError::new)
}

impl<'a> Codegen<'a> {
    fn fresh_label(&mut self, _prefix: &str) -> Label {
        self.out.label()
    }

    fn compile_function(
        &mut self,
        f: &FunctionDecl,
        global_imports: &HashMap<String, String>,
        top_level_uses: &[crate::ast::UseDecl],
    ) -> Result<(), CodegenError> {
        self.current_function = Some(f.name.clone());
        let mut scopes = Scopes::new();
        scopes
            .imports
            .last_mut()
            .expect("scope stack is never empty")
            .extend(global_imports.iter().map(|(k, v)| (k.clone(), v.clone())));
        for decl in top_level_uses {
            self.apply_use_decl(decl, &mut scopes)?;
        }
        for p in &f.params {
            scopes.define(&p.name);
        }

        self.out.function_with_types(
            &f.name,
            f.params
                .iter()
                .map(|parameter| parameter.ty.as_ref().and_then(runtime_type))
                .collect(),
            f.result_type.as_ref().and_then(runtime_type),
        );
        self.in_function = true;
        for i in (0..f.params.len()).rev() {
            self.out.emit(Instruction::Store(i));
        }

        self.function_result_type = f.result_type.clone();
        self.compile_block_result(
            &f.body,
            &mut scopes,
            self.active_defers.clone(),
            true,
            f.result_type.as_ref(),
        )?;
        self.out.emit(Instruction::Return);
        self.function_result_type = None;
        self.in_function = false;
        Ok(())
    }

    fn compile_block_with_deferred(
        &mut self,
        block: &Block,
        scopes: &mut Scopes,
        inherited: Vec<Deferred>,
    ) -> Result<(), CodegenError> {
        self.compile_block_result(block, scopes, inherited, false, None)
    }

    fn compile_block_result(
        &mut self,
        block: &Block,
        scopes: &mut Scopes,
        inherited: Vec<Deferred>,
        value: bool,
        expected_result: Option<&crate::ast::TypeAnnotation>,
    ) -> Result<(), CodegenError> {
        scopes.push();
        let saved_active = self.active_defers.clone();
        let inherited_count = inherited.len();
        let mut deferred = inherited;
        let mut local_deferred = Vec::new();
        // Both `return` and `break` are terminal within this block: they already
        // flush whatever defers they're responsible for on their own (see their
        // respective compile_stmt arms), and nothing after them is reachable. If we
        // let the loop continue past either, the trailing "flush local_deferred at
        // natural block end" below would run a second time and double-execute the
        // same defer body.
        let mut terminated_early = false;

        for stmt in &block.stmts {
            match stmt.node() {
                Stmt::Defer { body } => {
                    let cleanup = Deferred {
                        id: self.fresh_label("defer"),
                        body: body.clone(),
                        scopes: scopes.clone(),
                    };
                    local_deferred.push(cleanup.clone());
                    deferred.push(cleanup);
                }
                Stmt::Return(_) => {
                    self.compile_stmt(stmt, scopes, &mut deferred)?;
                    terminated_early = true;
                    break;
                }
                Stmt::Break | Stmt::Continue => {
                    self.compile_stmt(stmt, scopes, &mut deferred)?;
                    terminated_early = true;
                    break;
                }
                _ => self.compile_stmt(stmt, scopes, &mut deferred)?,
            }
        }

        if !terminated_early {
            self.active_defers = deferred;
            if let Some(tail) = &block.tail {
                if value {
                    if let Some(expected) = expected_result {
                        self.compile_expr_typed_array(tail, expected, scopes)?;
                    } else {
                        self.compile_expr(tail, scopes)?;
                    }
                } else {
                    self.compile_discarded_expr(tail, scopes)?;
                }
            } else if value {
                self.out.emit(Instruction::Push(Value::Nil));
            }
            // Snapshot the result value, not the referenced object, before any
            // local cleanup. Cleanup may reassign bindings or use the stack.
            let snapshot = if !local_deferred.is_empty() {
                if !value {
                    self.out.emit(Instruction::Push(Value::Nil));
                }
                let slot = scopes.next_slot;
                scopes.next_slot += 1;
                self.out.emit(Instruction::Store(slot));
                Some(slot)
            } else {
                None
            };
            let saved_primary = self.cleanup_primary;
            if let Some(slot) = snapshot {
                self.cleanup_primary = Some(slot);
            }
            for cleanup in local_deferred.iter().rev() {
                self.compile_cleanup(cleanup, scopes)?;
            }
            self.cleanup_primary = saved_primary;
            if let Some(slot) = snapshot {
                self.out.emit(Instruction::Load(slot));
                if !value {
                    // Completed local defers must not run again if a cleanup
                    // failure exits the remaining scopes/function.
                    self.active_defers.truncate(inherited_count);
                    self.propagate_control_error(scopes)?;
                    self.out.emit(Instruction::Pop);
                }
            }
        }

        self.active_defers = saved_active;
        scopes.pop();
        Ok(())
    }

    fn compile_cleanup(
        &mut self,
        cleanup: &Deferred,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        let index = self
            .active_defers
            .iter()
            .position(|entry| entry.id == cleanup.id)
            .ok_or_else(|| {
                CodegenError::new("internal error: cleanup is outside its lexical scope")
            })?;
        let pending = self.active_defers[..index].to_vec();
        let saved_handler = self.error_handler.clone();
        while self
            .error_handler
            .as_ref()
            .is_some_and(|handler| handler.defer_baseline > pending.len())
        {
            self.error_handler = self
                .error_handler
                .take()
                .and_then(|handler| handler.parent.map(|parent| *parent));
        }
        let saved = std::mem::replace(scopes, cleanup.scopes.clone());
        scopes.next_slot = saved.next_slot.max(scopes.next_slot);
        let saved_boundary = self.cleanup_boundary.clone();
        let boundary = if let Some(primary_slot) = self.cleanup_primary {
            let boundary = CleanupBoundary {
                label: self.fresh_label("cleanup_failed"),
                primary_slot,
                stack_depth: self.out.stack_depth(),
                defer_baseline: pending.len(),
            };
            self.cleanup_boundary = Some(boundary.clone());
            Some(boundary)
        } else {
            None
        };
        let result = self.compile_block_with_deferred(&cleanup.body, scopes, pending);
        if let Some(boundary) = boundary {
            self.out.bind(boundary.label);
        }
        self.cleanup_boundary = saved_boundary;
        let allocated = scopes.next_slot;
        *scopes = saved;
        scopes.next_slot = scopes.next_slot.max(allocated);
        self.error_handler = saved_handler;
        result
    }

    fn compile_stmt(
        &mut self,
        stmt: &Stmt,
        scopes: &mut Scopes,
        deferred: &mut Vec<Deferred>,
    ) -> Result<(), CodegenError> {
        let saved_source = self.out.enter_source(stmt.span());
        let previous = std::mem::replace(&mut self.active_defers, deferred.clone());
        let result = self
            .compile_stmt_inner(stmt, scopes, deferred)
            .map_err(|e| e.at(stmt.span()));
        self.active_defers = previous;
        self.out.leave_source(saved_source);
        result
    }

    fn compile_stmt_inner(
        &mut self,
        stmt: &Stmt,
        scopes: &mut Scopes,
        deferred: &mut Vec<Deferred>,
    ) -> Result<(), CodegenError> {
        match stmt.node() {
            Stmt::Located(_, _) => unreachable!("node removes locations"),
            Stmt::Invalid { .. } => {
                return Err(CodegenError::new(
                    "internal error: invalid AST reached code generation",
                ));
            }
            Stmt::VarDecl { name, ty, value } => {
                if let Some(ty) = ty {
                    self.compile_expr_typed_array(value, ty, scopes)?;
                } else {
                    self.compile_expr(value, scopes)?;
                }
                let slot = scopes.define(name);
                self.out.emit(Instruction::Store(slot));
            }
            Stmt::Const { name, ty, value } => {
                if let Some(ty) = ty {
                    self.compile_expr_typed_array(value, ty, scopes)?;
                } else {
                    self.compile_expr(value, scopes)?;
                }
                let slot = scopes.define(name);
                scopes.mark_const(name);
                self.out.emit(Instruction::Store(slot));
            }
            Stmt::Comptime { .. } => {
                return Err(CodegenError::new(
                    "compile-time declarations must be evaluated before code generation",
                ));
            }
            Stmt::Use {
                module,
                names,
                aliases,
                namespace,
                wildcard,
                ..
            } => {
                self.validate_module(module)?;
                if *namespace {
                    let bind_name = aliases.first().and_then(|a| a.as_deref()).unwrap_or(module);
                    scopes.bind_module(bind_name, module);
                } else if *wildcard {
                    self.bind_wildcard_import(module, scopes)?;
                } else {
                    for (name, alias) in names.iter().zip(aliases.iter()) {
                        let target = format!("{module}::{name}");
                        self.validate_import_target(module, name)?;
                        let bind_name = alias.as_deref().unwrap_or(name.as_str());
                        scopes.bind_import(bind_name, &target);
                    }
                }
            }
            Stmt::Assign { target, value } => {
                self.compile_assign(target, value, scopes)?;
            }
            Stmt::CompoundAssign { target, op, value } => {
                self.compile_compound_assign(target, *op, value, scopes)?;
            }
            Stmt::Return(Some(e)) => {
                if let Some(expected) = self.function_result_type.clone() {
                    self.compile_expr_typed_array(e, &expected, scopes)?;
                } else {
                    self.compile_expr(e, scopes)?;
                }
                self.compile_return(scopes, deferred)?;
            }
            Stmt::Return(None) => {
                self.out.emit(Instruction::Push(Value::Nil));
                self.compile_return(scopes, deferred)?;
            }
            Stmt::Defer { body } => {
                deferred.push(Deferred {
                    id: self.fresh_label("defer"),
                    body: body.clone(),
                    scopes: scopes.clone(),
                });
            }
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.compile_try(body, catch_name, catch_block, scopes, false)?;
            }

            Stmt::Expr(e) => self.compile_discarded_expr(e, scopes)?,
            Stmt::Break => {
                if let Some(transfer) = self.loop_end_stack.last().copied() {
                    self.compile_loop_transfer(transfer, scopes, deferred)?;
                } else {
                    return Err(CodegenError::new("`break` used outside of a loop"));
                }
            }
            Stmt::Continue => {
                if let Some(transfer) = self.loop_continue_stack.last().copied() {
                    self.compile_loop_transfer(transfer, scopes, deferred)?;
                } else {
                    return Err(CodegenError::new("`continue` used outside of a loop"));
                }
            }
            Stmt::Crash { value, context } => {
                self.compile_expr(value, scopes)?;
                match context {
                    Some(s) => {
                        self.out
                            .emit(Instruction::Crash(Some(Rc::from(s.as_str()))));
                    }
                    None => self.out.emit(Instruction::Crash(None)),
                }
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                let else_label = self.fresh_label("if_else");
                let end_label = self.fresh_label("if_end");
                self.compile_expr(cond, scopes)?;
                self.validate_condition(scopes, None)?;
                self.out.jump(Instruction::JumpIfFalse(0), else_label);
                self.compile_block_with_deferred(then_block, scopes, deferred.clone())?;
                self.out.jump(Instruction::Jump(0), end_label);
                self.out.bind(else_label);
                if let Some(b) = else_block {
                    self.compile_block_with_deferred(b, scopes, deferred.clone())?;
                }
                self.out.bind(end_label);
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                let temp_slot = scopes.next_slot;
                scopes.next_slot += 1;
                let else_label = self.fresh_label("iflet_else");
                let end_label = self.fresh_label("iflet_end");

                self.compile_handled_expr(value, scopes)?;
                self.out.emit(Instruction::Store(temp_slot));
                self.out.emit(Instruction::Load(temp_slot));
                self.out.emit(Instruction::IsType(ValueType::Error));
                self.out.emit(Instruction::Not);
                self.out.jump(Instruction::JumpIfFalse(0), else_label);

                scopes.push();
                let bound_slot = scopes.define(name);
                self.out.emit(Instruction::Load(temp_slot));
                self.out.emit(Instruction::Store(bound_slot));
                self.compile_block_with_deferred(then_block, scopes, deferred.clone())?;
                scopes.pop();
                self.out.jump(Instruction::Jump(0), end_label);
                self.out.bind(else_label);
                if let Some(b) = else_block {
                    self.compile_block_with_deferred(b, scopes, deferred.clone())?;
                }
                self.out.bind(end_label);
            }
            Stmt::While { cond, body } => {
                let cond_label = self.fresh_label("while_cond");
                let end_label = self.fresh_label("while_end");
                self.out.bind(cond_label);
                self.compile_expr(cond, scopes)?;
                self.validate_condition(scopes, None)?;
                self.out.jump(Instruction::JumpIfFalse(0), end_label);
                // push loop end label so `break` can compile to jump here, along with
                // how many defers were already registered before this loop (see
                // loop_end_stack's doc comment)
                let transfer = LoopTransfer {
                    label: end_label,
                    defer_baseline: deferred.len(),
                    stack_depth: self.out.stack_depth(),
                };
                self.loop_end_stack.push(transfer);
                self.loop_continue_stack.push(LoopTransfer {
                    label: cond_label,
                    ..transfer
                });
                self.compile_block_with_deferred(body, scopes, deferred.clone())?;
                self.loop_continue_stack.pop();
                self.loop_end_stack.pop();
                self.out.jump(Instruction::Jump(0), cond_label);
                self.out.bind(end_label);
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let cond_label = self.fresh_label("for_cond");
                let end_label = self.fresh_label("for_end");
                let next_label = self.fresh_label("for_next");
                let desc_label = self.fresh_label("for_desc");
                let cmp_done_label = self.fresh_label("for_cmp_done");
                scopes.push();
                self.compile_expr(start, scopes)?;
                self.out
                    .extend([Instruction::Push(Value::Int(0)), Instruction::Sub]);
                self.propagate_control_error(scopes)?;
                let var_slot = scopes.define(var);
                self.out.emit(Instruction::Store(var_slot));
                self.compile_expr(end, scopes)?;
                self.out
                    .extend([Instruction::Push(Value::Int(0)), Instruction::Sub]);
                self.propagate_control_error(scopes)?;
                let end_slot = scopes.next_slot;
                scopes.next_slot += 1;
                self.out.emit(Instruction::Store(end_slot));
                // Step defaults to literal 1 (old behavior, unchanged) when
                // no `step` clause is present.
                let step_slot = scopes.next_slot;
                scopes.next_slot += 1;
                match step {
                    Some(step_expr) => self.compile_expr(step_expr, scopes)?,
                    None => self.out.emit(Instruction::Push(Value::Int(1))),
                }
                self.out
                    .extend([Instruction::Push(Value::Int(0)), Instruction::Sub]);
                self.propagate_control_error(scopes)?;
                self.out.emit(Instruction::Store(step_slot));

                let valid_step = self.fresh_label("nonzero_step");
                self.out.extend([
                    Instruction::Load(step_slot),
                    Instruction::Push(Value::Int(0)),
                    Instruction::Eq,
                ]);
                self.out.jump(Instruction::JumpIfFalse(0), valid_step);
                self.out.extend([
                    Instruction::Push(Value::TypeTag(Rc::from(format!(
                        "{}::InvalidRangeStep",
                        crate::error::ERROR_TYPE
                    )))),
                    Instruction::Push(Value::Nil),
                    Instruction::MakeRecord(1),
                ]);
                self.propagate_control_error(scopes)?;
                self.out.emit(Instruction::Pop);
                self.out.bind(valid_step);

                self.out.bind(cond_label);
                // Direction is re-checked every iteration from step_slot
                // rather than assumed once, so a step computed from a
                // variable (not just a literal) still picks the right
                // comparison each time the sign could matter.
                self.out.emit(Instruction::Load(step_slot));
                self.out.emit(Instruction::Push(Value::Int(0)));
                self.out.emit(Instruction::Lt);
                self.propagate_control_error(scopes)?;
                self.out.jump(Instruction::JumpIfTrue(0), desc_label);
                self.out.emit(Instruction::Load(var_slot));
                self.out.emit(Instruction::Load(end_slot));
                self.out.emit(Instruction::Lt);
                self.out.jump(Instruction::Jump(0), cmp_done_label);
                self.out.bind(desc_label);
                self.out.emit(Instruction::Load(var_slot));
                self.out.emit(Instruction::Load(end_slot));
                self.out.emit(Instruction::Gt);
                self.out.bind(cmp_done_label);
                self.propagate_control_error(scopes)?;
                self.out.jump(Instruction::JumpIfFalse(0), end_label);
                // push loop end label so `break` can compile to jump here, along with
                // how many defers were already registered before this loop (see
                // loop_end_stack's doc comment)
                let transfer = LoopTransfer {
                    label: end_label,
                    defer_baseline: deferred.len(),
                    stack_depth: self.out.stack_depth(),
                };
                self.loop_end_stack.push(transfer);
                self.loop_continue_stack.push(LoopTransfer {
                    label: next_label,
                    ..transfer
                });
                self.compile_block_with_deferred(body, scopes, deferred.clone())?;
                self.loop_continue_stack.pop();
                self.loop_end_stack.pop();
                self.out.bind(next_label);
                self.out.emit(Instruction::Load(var_slot));
                self.out.emit(Instruction::Load(step_slot));
                self.out.emit(Instruction::Add);
                self.propagate_control_error(scopes)?;
                self.out.emit(Instruction::Store(var_slot));
                self.out.jump(Instruction::Jump(0), cond_label);
                self.out.bind(end_label);
                scopes.pop();
            }
            Stmt::ForEach {
                var,
                iterable,
                body,
            } => {
                let condition_label = self.fresh_label("foreach_condition");
                let end_label = self.fresh_label("foreach_end");
                scopes.push();
                let iterator_slot = scopes.next_slot;
                scopes.next_slot += 1;
                self.compile_expr(iterable, scopes)?;
                self.out.call_native(crate::runtime::iterator_new_import());
                self.propagate_control_error(scopes)?;
                self.out.emit(Instruction::Store(iterator_slot));
                let value_slot = scopes.define(var);

                self.out.bind(condition_label);
                self.out.emit(Instruction::Load(iterator_slot));
                self.out.call_native(crate::runtime::iterator_next_import());
                self.propagate_control_error(scopes)?;
                self.out.jump(Instruction::JumpIfFalse(0), end_label);
                self.out.emit(Instruction::Load(iterator_slot));
                self.out
                    .call_native(crate::runtime::iterator_current_import());
                self.out.emit(Instruction::Store(value_slot));

                let transfer = LoopTransfer {
                    label: end_label,
                    defer_baseline: deferred.len(),
                    stack_depth: self.out.stack_depth(),
                };
                self.loop_end_stack.push(transfer);
                self.loop_continue_stack.push(LoopTransfer {
                    label: condition_label,
                    ..transfer
                });
                self.compile_block_with_deferred(body, scopes, deferred.clone())?;
                self.loop_continue_stack.pop();
                self.loop_end_stack.pop();
                self.out.jump(Instruction::Jump(0), condition_label);
                self.out.bind(end_label);
                self.out.extend([
                    Instruction::Push(Value::Nil),
                    Instruction::Store(iterator_slot),
                ]);
                scopes.pop();
            }
        }
        Ok(())
    }

    fn compile_loop_transfer(
        &mut self,
        transfer: LoopTransfer,
        scopes: &mut Scopes,
        deferred: &[Deferred],
    ) -> Result<(), CodegenError> {
        let LoopTransfer {
            label,
            defer_baseline: baseline,
            stack_depth,
        } = transfer;
        // Discard partially evaluated operands before leaving the iteration,
        // including builders and aggregate prefixes around break/continue.
        self.out.emit(Instruction::StackTruncate(stack_depth));
        if baseline == deferred.len() {
            self.out.jump(Instruction::Jump(0), label);
            return Ok(());
        }

        let slot = scopes.next_slot;
        scopes.next_slot += 1;
        self.out
            .extend([Instruction::Push(Value::Nil), Instruction::Store(slot)]);
        let primary = self.cleanup_primary.replace(slot);
        for cleanup in deferred[baseline..].iter().rev() {
            self.compile_cleanup(cleanup, scopes)?;
        }
        self.cleanup_primary = primary;
        self.out.emit(Instruction::Load(slot));
        let active = self.active_defers.clone();
        self.active_defers.truncate(baseline);
        // A cleanup outside a protected scope may have already left that scope.
        // Do not re-enter its catch now.
        let handler = self.error_handler.take();
        self.propagate_control_error(scopes)?;
        self.error_handler = handler;
        self.active_defers = active;
        self.out.emit(Instruction::Pop);
        self.out.jump(Instruction::Jump(0), label);
        Ok(())
    }

    fn compile_assign(
        &mut self,
        target: &Expr,
        value: &Expr,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        match target.node() {
            Expr::Ident(name) => {
                if scopes.is_const(name) {
                    return Err(CodegenError::new(format!(
                        "cannot assign to const `{name}`"
                    )));
                }
                if let Some(slot) = scopes.resolve(name) {
                    self.compile_expr(value, scopes)?;
                    self.out.emit(Instruction::Load(slot));
                    self.out.emit(Instruction::ConstrainArrayFrom);
                    self.out.emit(Instruction::Store(slot));
                    return Ok(());
                }
                let resolved = scopes
                    .resolve_import(name)
                    .unwrap_or_else(|| name.to_string());
                let candidate = self.resolve_global_name(&resolved, Some(scopes))?;
                if let Some(sig) = self.globals.get(&candidate) {
                    if !sig.mutable {
                        return Err(CodegenError::new(format!(
                            "cannot assign to const `{name}`"
                        )));
                    }
                    self.compile_expr(value, scopes)?;
                    let idx = self.global_indices[&candidate];
                    self.out.emit(Instruction::LoadGlobal(idx));
                    self.out.emit(Instruction::ConstrainArrayFrom);
                    self.out.emit(Instruction::StoreGlobal(idx));
                    return Ok(());
                }
                Err(CodegenError::new(format!(
                    "cannot assign to undefined variable `{name}` (declare it first with `var`)"
                )))
            }
            Expr::Index(arr, idx) => {
                self.compile_expr(arr, scopes)?;
                self.compile_expr(idx, scopes)?;
                self.compile_expr(value, scopes)?;
                self.out.emit(Instruction::IndexSet);
                self.guard_error(scopes)?;
                self.out.emit(Instruction::Pop);
                Ok(())
            }
            _ => Err(CodegenError::new(
                "invalid assignment target: expected a variable or an indexed expression, e.g. `x = 1;` or `arr[0] = 1;`",
            )),
        }
    }

    fn compile_compound_assign(
        &mut self,
        target: &Expr,
        op: BinOp,
        value: &Expr,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        scopes.push();
        let target = match target.node() {
            Expr::Ident(name) => {
                if scopes.is_const(name) {
                    return Err(CodegenError::new(format!(
                        "cannot assign to const `{name}`"
                    )));
                }
                if scopes.resolve(name).is_none() {
                    let resolved = scopes
                        .resolve_import(name)
                        .unwrap_or_else(|| name.to_string());
                    let candidate = self.resolve_global_name(&resolved, Some(scopes))?;
                    if let Some(signature) = self.globals.get(&candidate) {
                        if !signature.mutable {
                            return Err(CodegenError::new(format!(
                                "cannot assign to const `{name}`"
                            )));
                        }
                    } else {
                        return Err(CodegenError::new(format!(
                            "cannot assign to undefined variable `{name}` (declare it first with `var`)"
                        )));
                    }
                }
                Expr::Ident(name.clone())
            }
            Expr::Index(receiver, index) => {
                self.compile_expr(receiver, scopes)?;
                let receiver_name = format!("$receiver{}", scopes.next_slot);
                let receiver_slot = scopes.define(&receiver_name);
                self.out.emit(Instruction::Store(receiver_slot));

                self.compile_expr(index, scopes)?;
                let index_name = format!("$index{}", scopes.next_slot);
                let index_slot = scopes.define(&index_name);
                self.out.emit(Instruction::Store(index_slot));

                Expr::Index(
                    Box::new(Expr::Ident(receiver_name)),
                    Box::new(Expr::Ident(index_name)),
                )
            }
            _ => {
                return Err(CodegenError::new(
                    "invalid assignment target: expected a variable or an indexed expression, e.g. `x += 1;` or `arr[0] += 1;`",
                ));
            }
        };

        // Compile the original RHS node so semantic call-site facts remain
        // available for generic extension methods such as Map.get().
        self.compile_expr(&target, scopes)?;
        self.compile_binary_rhs(op, value, scopes)?;

        let result_name = format!("$compound_result{}", scopes.next_slot);
        let result_slot = scopes.define(&result_name);
        self.out.emit(Instruction::Store(result_slot));
        self.compile_assign(&target, &Expr::Ident(result_name), scopes)?;
        scopes.pop();
        Ok(())
    }

    /// The left operand has already been emitted onto the operand stack.
    fn compile_binary_rhs(
        &mut self,
        op: BinOp,
        right: &Expr,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        if matches!(op, BinOp::And | BinOp::Or) {
            let error_label = self.fresh_label("logical_error");
            let end_label = self.fresh_label("logical_end");
            self.out.extend([
                Instruction::Dup,
                Instruction::Not,
                Instruction::Dup,
                Instruction::IsType(ValueType::Error),
            ]);
            self.out.jump(Instruction::JumpIfTrue(0), error_label);
            self.out.jump(
                if op == BinOp::And {
                    Instruction::JumpIfTrue(0)
                } else {
                    Instruction::JumpIfFalse(0)
                },
                end_label,
            );
            self.out.emit(Instruction::Pop);
            self.compile_expr(right, scopes)?;
            self.out.extend([Instruction::Not, Instruction::Not]);
            self.out.jump(Instruction::Jump(0), end_label);
            self.out.bind(error_label);
            self.out.extend([Instruction::Swap, Instruction::Pop]);
            self.out.bind(end_label);
        } else {
            self.compile_expr(right, scopes)?;
            self.out.emit(binop_instruction(op));
        }
        self.guard_error(scopes)
    }

    fn compile_declared_call(
        &mut self,
        name: &str,
        fields: &[crate::ast::StructField],
        args: &[CallArgument],
        module: Option<String>,
        target: DeclaredCall,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        let bindings = crate::arguments::bind(name, fields, args);
        if let Some(error) = bindings.errors.first() {
            return Err(CodegenError::new(error.message.clone()));
        }
        // A supplied parameter prefix needs no temporaries. Otherwise snapshot
        // all supplied expressions in written order before evaluating defaults.
        let prefix = args
            .iter()
            .enumerate()
            .all(|(i, _)| bindings.by_parameter[i] == Some(i));
        let mut slots = Vec::new();
        if !prefix {
            for (argument_index, arg) in args.iter().enumerate() {
                let expected = bindings.by_argument[argument_index]
                    .and_then(|parameter| fields[parameter].ty.as_ref());
                self.compile_call_argument(&arg.value, expected, scopes)?;
                let slot = scopes.next_slot;
                scopes.next_slot += 1;
                self.out.emit(Instruction::Store(slot));
                slots.push(slot);
            }
        }
        if matches!(target, DeclaredCall::Variant | DeclaredCall::Record) {
            let tag = if matches!(target, DeclaredCall::Record) {
                format!("@struct::{name}")
            } else {
                name.to_string()
            };
            self.out.extend([
                Instruction::Push(Value::TypeTag(Rc::from(tag))),
                Instruction::Push(Value::Nil),
            ]);
        }
        for (index, field) in fields.iter().enumerate() {
            if !matches!(target, DeclaredCall::Function) {
                self.out
                    .emit(Instruction::Push(Value::Str(Rc::from(field.name.as_str()))));
            }
            if let Some(argument) = bindings.by_parameter[index] {
                if prefix {
                    self.compile_call_argument(&args[argument].value, field.ty.as_ref(), scopes)?;
                } else {
                    let slot = slots[argument];
                    self.out.emit(Instruction::Load(slot));
                    // Keep the stack argument, release the temporary reference.
                    self.out
                        .extend([Instruction::Push(Value::Nil), Instruction::Store(slot)]);
                }
            } else if let Some(default) = &field.default {
                self.compile_default(default, module.clone(), scopes)?;
            } else {
                return Err(CodegenError::new(format!(
                    "`{name}` expects argument `{}`",
                    field.name
                )));
            }
        }
        match target {
            DeclaredCall::Function => self.out.call(name, fields.len()),
            DeclaredCall::Record => self.out.emit(Instruction::MakeRecord(fields.len() + 1)),
            DeclaredCall::Variant => self.out.emit(Instruction::MakeRecord(fields.len() + 1)),
        }
        Ok(())
    }

    fn compile_declared_method_call(
        &mut self,
        name: &str,
        params: &[crate::ast::Param],
        receiver: &Expr,
        args: &[Expr],
        module: Option<String>,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        let Some(receiver_param) = params.first() else {
            return Err(CodegenError::new(format!(
                "extension method `{name}` has no receiver"
            )));
        };
        let maximum_arguments = params.len() - 1;
        if args.len() > maximum_arguments {
            return Err(CodegenError::new(format!(
                "method `{name}` expects at most {maximum_arguments} argument(s)"
            )));
        }
        self.compile_call_argument(receiver, receiver_param.ty.as_ref(), scopes)?;
        for (index, parameter) in params.iter().enumerate().skip(1) {
            if let Some(argument) = args.get(index - 1) {
                self.compile_call_argument(argument, parameter.ty.as_ref(), scopes)?;
            } else if let Some(default) = &parameter.default {
                self.compile_default(default, module.clone(), scopes)?;
            } else {
                return Err(CodegenError::new(format!(
                    "`{name}` expects argument `{}`",
                    parameter.name
                )));
            }
        }
        self.out.call(name, params.len());
        Ok(())
    }

    fn compile_default(
        &mut self,
        value: &Expr,
        module: Option<String>,
        caller: &mut Scopes,
    ) -> Result<(), CodegenError> {
        let previous = std::mem::replace(&mut self.current_module, module);
        let mut context = Scopes::new();
        for decl in self.declaration_uses {
            self.apply_use_decl(decl, &mut context)?;
        }
        context.next_slot = caller.next_slot;
        let result = self.compile_expr(value, &mut context);
        caller.next_slot = context.next_slot;
        self.current_module = previous;
        result
    }

    fn compile_builtin_call(
        &mut self,
        builtin: Builtin,
        args: &[CallArgument],
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        if args.len() != builtin.arity() {
            return Err(CodegenError::new(format!(
                "`{}` expects {} argument(s), got {}",
                format!("{:?}", builtin.instruction()).to_lowercase(),
                builtin.arity(),
                args.len()
            )));
        }
        for arg in args {
            if matches!(builtin, Builtin::ErrorKind) {
                self.compile_handled_expr(&arg.value, scopes)?;
            } else {
                self.compile_expr(&arg.value, scopes)?;
            }
        }
        match builtin {
            Builtin::Native(import) => self.out.call_native(import),
            Builtin::BuildInput => {
                return Err(CodegenError::new(
                    "build input calls must be evaluated at comptime",
                ));
            }
            _ => self
                .out
                .emit(builtin.instruction().expect("VM builtin instruction")),
        }
        Ok(())
    }

    fn compile_interpolation(
        &mut self,
        parts: &[StringPart],
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        if parts.iter().all(|p| matches!(p, StringPart::Text(_))) {
            let text: String = parts
                .iter()
                .filter_map(|p| match p {
                    StringPart::Text(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            self.out.emit(Instruction::Push(Value::Str(text.into())));
            return Ok(());
        }
        if let [StringPart::Expression(value)] = parts {
            self.compile_expr(value, scopes)?;
            self.out.call_native(
                crate::runtime::NativeImport::named("str::to_str")
                    .expect("string conversion import"),
            );
            return Ok(());
        }
        self.out.call_native(
            crate::runtime::NativeImport::named("str::builder").expect("string builder import"),
        );
        self.guard_error(scopes)?;
        for part in parts {
            match part {
                StringPart::Text(text) if !text.contains("{}") => {
                    // A literal template without placeholders appends directly,
                    // avoiding a temporary rendered copy of this text chunk.
                    self.out
                        .emit(Instruction::Push(Value::Str(Rc::from(text.as_str()))));
                    self.out.emit(Instruction::Push(Value::Nil));
                }
                StringPart::Text(text) => {
                    self.out.emit(Instruction::Push(Value::Str(Rc::from("{}"))));
                    self.out
                        .emit(Instruction::Push(Value::Str(Rc::from(text.as_str()))));
                }
                StringPart::Expression(value) => {
                    self.out.emit(Instruction::Push(Value::Str(Rc::from("{}"))));
                    self.compile_expr(value, scopes)?;
                }
            }
            self.out.call_native(
                crate::runtime::NativeImport::named("fmt::write").expect("formatting import"),
            );
            // Rendering itself may fail (limits/cycles), so the surrounding
            // lexical boundary must observe it before evaluating later holes.
            self.guard_error(scopes)?;
        }
        self.out.call_native(
            crate::runtime::NativeImport::named("str::build").expect("string snapshot import"),
        );
        Ok(())
    }

    fn compile_method_call(
        &mut self,
        receiver: &Expr,
        method: &str,
        args: &[Expr],
        extension: Option<&str>,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        if let Some(name) = extension {
            if name == "$vm::structural_clone" {
                if !args.is_empty() {
                    return Err(CodegenError::new("method `clone()` expects 0 arguments"));
                }
                self.compile_expr(receiver, scopes)?;
                self.out
                    .call_native(crate::runtime::structural_clone_import());
                return Ok(());
            }
            let sig = self.functions.get(name).cloned().ok_or_else(|| {
                CodegenError::new(format!("unknown extension method `{method}()`"))
            })?;
            return self.compile_declared_method_call(
                name,
                &sig.params,
                receiver,
                args,
                sig.module,
                scopes,
            );
        }
        // The receiver is evaluated first. Conversions share the native ABI
        // with casts and explicit library calls.
        match method {
            "empty_like" => {
                if !args.is_empty() {
                    return Err(CodegenError::new(
                        "method `empty_like()` expects 0 arguments",
                    ));
                }
                self.compile_expr(receiver, scopes)?;
                self.out.emit(Instruction::ArrayEmptyLike);
            }
            "append" | "prepend" => {
                if args.len() != 1 {
                    return Err(CodegenError::new(format!(
                        "method `{method}()` expects 1 argument"
                    )));
                }
                self.compile_expr(receiver, scopes)?;
                self.compile_expr(&args[0], scopes)?;
                if method == "prepend" {
                    self.out.emit(Instruction::Swap);
                }
                self.out.emit(if method == "append" {
                    Instruction::ArrayPush
                } else {
                    Instruction::ArrayPrepend
                });
            }
            "len" => {
                if !args.is_empty() {
                    return Err(CodegenError::new("method `len()` expects 0 arguments"));
                }
                self.compile_expr(receiver, scopes)?;
                self.out.emit(Instruction::Len);
            }
            "to_int" | "to_float" | "to_bool" | "to_str" => {
                if !args.is_empty() {
                    return Err(CodegenError::new(format!(
                        "method `{method}()` expects 0 arguments"
                    )));
                }
                self.compile_expr(receiver, scopes)?;
                let name = match method {
                    "to_int" => "str::to_int",
                    "to_float" => "str::to_float",
                    "to_bool" => "str::to_bool",
                    _ => "str::to_str",
                };
                self.out.call_native(
                    crate::runtime::NativeImport::named(name).expect("conversion import"),
                );
            }
            "error_kind" => {
                if !args.is_empty() {
                    return Err(CodegenError::new(
                        "method `error_kind()` expects 0 arguments",
                    ));
                }
                self.compile_handled_expr(receiver, scopes)?;
                self.out.emit(Instruction::ErrorKind);
            }
            "unwrap" => {
                if !args.is_empty() {
                    return Err(CodegenError::new("method `unwrap()` expects 0 arguments"));
                }
                self.compile_handled_expr(receiver, scopes)?;
                let ok_label = self.fresh_label("unwrap_ok");
                self.out
                    .extend([Instruction::Dup, Instruction::IsType(ValueType::Error)]);
                self.out.jump(Instruction::JumpIfFalse(0), ok_label);
                // On the error path the original error is on top; crash
                // consumes it. On the success path the original value remains.
                self.out.emit(Instruction::Crash(Some(Rc::from(
                    "unwrap called on an error value",
                ))));
                self.out.bind(ok_label);
            }
            "expect" => {
                if args.len() != 1 {
                    return Err(CodegenError::new("method `expect()` expects 1 argument"));
                }
                self.compile_handled_expr(receiver, scopes)?;
                self.out
                    .extend([Instruction::Dup, Instruction::IsType(ValueType::Error)]);
                let ok_label = self.fresh_label("expect_ok");
                self.out.jump(Instruction::JumpIfFalse(0), ok_label);
                // The error value stays below the message. Crash consumes the
                // message, so the error value is not leaked into the runtime.
                self.compile_expr(&args[0], scopes)?;
                self.out.emit(Instruction::Crash(None));
                self.out.bind(ok_label);
            }
            _ => {
                return Err(CodegenError::new(format!("unknown method `{method}()`")));
            }
        }
        Ok(())
    }

    fn resolve_global_name(
        &self,
        name: &str,
        scopes: Option<&Scopes>,
    ) -> Result<String, CodegenError> {
        let candidate = if let Some((qualifier, _item)) = name.split_once("::") {
            if self.current_module.as_deref() == Some(qualifier)
                || scopes.and_then(|s| s.resolve_module(qualifier)).is_some()
                || scopes.and_then(|s| s.resolve_import(name)).is_some()
            {
                name.to_string()
            } else {
                return Err(CodegenError::new(format!(
                    "undeclared module `{qualifier}`"
                )));
            }
        } else if let Some(module) = &self.current_module {
            let local = format!("{module}::{name}");
            if self.globals.contains_key(&local) {
                local
            } else {
                name.to_string()
            }
        } else {
            name.to_string()
        };
        if let Some(sig) = self.globals.get(&candidate) {
            if sig.visibility == Visibility::Private
                && sig.module.as_deref() != self.current_module.as_deref()
            {
                return Err(CodegenError::new(format!(
                    "global `{candidate}` is private to module `{}`",
                    sig.module.as_deref().unwrap_or("<local>")
                )));
            }
            return Ok(candidate);
        }
        Ok(candidate)
    }

    fn known_module(&self, module: &str) -> bool {
        crate::builtins::native_module(module)
            || crate::core_modules::SOURCES
                .iter()
                .any(|(name, _)| *name == module)
            || self.functions.keys().any(|name| {
                name.split_once("::")
                    .map(|(m, _)| m == module)
                    .unwrap_or(false)
            })
            || self.globals.keys().any(|name| {
                name.split_once("::")
                    .map(|(m, _)| m == module)
                    .unwrap_or(false)
            })
            || self.struct_defs.keys().any(|name| {
                name.split_once("::")
                    .map(|(m, _)| m == module)
                    .unwrap_or(false)
            })
            || self.enum_defs.keys().any(|name| {
                name.split_once("::")
                    .map(|(m, _)| m == module)
                    .unwrap_or(false)
            })
    }

    fn validate_module(&self, module: &str) -> Result<(), CodegenError> {
        if self.known_module(module) {
            Ok(())
        } else {
            Err(CodegenError::new(format!("unknown module `{module}`")))
        }
    }

    fn validate_import_target(&self, module: &str, name: &str) -> Result<(), CodegenError> {
        let target = format!("{module}::{name}");
        if self.functions.contains_key(&target)
            || self.globals.contains_key(&target)
            || self.struct_defs.contains_key(&target)
            || self.enum_defs.contains_key(&target)
            || Builtin::from_name(&target).is_some()
        {
            return Ok(());
        }
        Err(CodegenError::new(format!(
            "unknown item `{name}` in module `{module}`"
        )))
    }

    fn bind_wildcard_import(&self, module: &str, scopes: &mut Scopes) -> Result<(), CodegenError> {
        let prefix = format!("{module}::");
        for (name, sig) in self.functions.iter() {
            if let Some(short) = name.strip_prefix(&prefix)
                && sig.visibility == Visibility::Public
            {
                scopes.bind_import(short, name);
            }
        }
        for (name, sig) in self.globals.iter() {
            if let Some(short) = name.strip_prefix(&prefix)
                && sig.visibility == Visibility::Public
            {
                scopes.bind_import(short, name);
            }
        }
        for name in self.struct_defs.keys() {
            if let Some(short) = name.strip_prefix(&prefix)
                && self.struct_visibility.get(name) == Some(&Visibility::Public)
            {
                scopes.bind_import(short, name);
            }
        }
        // Builtins are resolved by name rather than stored in the function map.
        for builtin in ["err::kind".to_owned()]
            .into_iter()
            .chain(crate::runtime::names())
        {
            if builtin.starts_with(&prefix) {
                let short = builtin.strip_prefix(&prefix).unwrap();
                scopes.bind_import(short, &builtin);
            }
        }
        Ok(())
    }

    fn apply_use_decl(
        &self,
        decl: &crate::ast::UseDecl,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        self.validate_module(&decl.module)?;
        if decl.namespace {
            let bind_name = decl
                .aliases
                .first()
                .and_then(|a| a.as_deref())
                .unwrap_or(&decl.module);
            scopes.bind_module(bind_name, &decl.module);
        } else if decl.wildcard {
            self.bind_wildcard_import(&decl.module, scopes)?;
        } else {
            for (name, alias) in decl.names.iter().zip(decl.aliases.iter()) {
                self.validate_import_target(&decl.module, name)?;
                let target = format!("{}::{}", decl.module, name);
                scopes.bind_import(alias.as_deref().unwrap_or(name), &target);
            }
        }
        Ok(())
    }

    fn resolve_function_name(&self, name: &str, scopes: &Scopes) -> Result<String, CodegenError> {
        let resolved = scopes
            .resolve_import(name)
            .unwrap_or_else(|| name.to_string());

        let candidate = if self.enum_defs.contains_key(&resolved)
            || self.enum_visibility.contains_key(&resolved)
        {
            resolved
        } else if let Some((qualifier, item)) = resolved.split_once("::") {
            let module = if self.current_module.as_deref() == Some(qualifier) {
                qualifier.to_string()
            } else if let Some(imported) = scopes.resolve_module(qualifier) {
                imported
            } else if scopes.resolve_import(name).is_some() {
                qualifier.to_string()
            } else {
                return Err(CodegenError::new(format!(
                    "undeclared module `{qualifier}`"
                )));
            };
            format!("{module}::{item}")
        } else if resolved.contains("::") {
            resolved
        } else if let Some(module) = &self.current_module {
            let local = format!("{module}::{resolved}");
            if self.functions.contains_key(&local)
                || self.struct_defs.contains_key(&local)
                || self.enum_defs.contains_key(&local)
                || self.enum_visibility.contains_key(&local)
            {
                local
            } else {
                resolved
            }
        } else {
            resolved
        };

        if let Some(sig) = self.functions.get(&candidate) {
            if sig.visibility == Visibility::Private {
                let same_module = self.current_module.as_deref() == sig.module.as_deref();
                if !same_module {
                    return Err(CodegenError::new(format!(
                        "function `{candidate}` is private to module `{}`",
                        sig.module.as_deref().unwrap_or("<local>")
                    )));
                }
            }
            return Ok(candidate);
        }

        if let Some(visibility) = self.struct_visibility.get(&candidate) {
            if *visibility == Visibility::Private {
                let module = candidate.rsplit_once("::").map(|(m, _)| m);
                if module != self.current_module.as_deref() {
                    return Err(CodegenError::new(format!(
                        "struct `{candidate}` is private to module `{}`",
                        module.unwrap_or("<local>")
                    )));
                }
            }
            return Ok(candidate);
        }

        if let Some((enum_name, _)) = candidate.rsplit_once("::")
            && let Some(visibility) = self.enum_visibility.get(enum_name)
        {
            if *visibility == Visibility::Private {
                let module = enum_name.rsplit_once("::").map(|(m, _)| m);
                if module != self.current_module.as_deref() {
                    return Err(CodegenError::new(format!(
                        "enum `{enum_name}` is private to module `{}`",
                        module.unwrap_or("<local>")
                    )));
                }
            }
            return Ok(candidate);
        }

        Ok(candidate)
    }

    fn type_test(&self, name: &str, scopes: &Scopes) -> Result<ValueType, CodegenError> {
        if let Some(primitive) = ValueType::primitive(name) {
            return Ok(primitive);
        }
        let name = self.resolve_function_name(name, scopes)?;
        if name == crate::error::ERROR_TYPE {
            return Ok(ValueType::Error);
        }
        if self.enum_defs.contains_key(&name) {
            return Ok(ValueType::Variant(name.into()));
        }
        if self.enum_visibility.contains_key(&name) {
            return Ok(ValueType::Enum(name.into()));
        }
        Err(CodegenError::new(format!("unsupported type test `{name}`")))
    }

    fn compile_expr_typed_array(
        &mut self,
        expr: &Expr,
        expected: &crate::ast::TypeAnnotation,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        match (expr.node(), &expected.kind) {
            (Expr::Array(items), crate::ast::TypeKind::Array(element)) => {
                for item in items {
                    self.compile_expr_typed_array(item, element, scopes)?;
                }
                if let Some(element_type) = runtime_array_element_type(expected) {
                    self.out
                        .emit(Instruction::MakeTypedArray(items.len(), element_type));
                } else {
                    self.out.emit(Instruction::MakeArray(items.len()));
                }
                Ok(())
            }
            (Expr::Tuple(items), crate::ast::TypeKind::Tuple(types))
                if items.len() == types.len() =>
            {
                for (item, ty) in items.iter().zip(types) {
                    self.compile_expr_typed_array(item, ty, scopes)?;
                }
                self.out.emit(Instruction::MakeTuple(items.len()));
                Ok(())
            }
            _ => {
                self.compile_expr(expr, scopes)?;
                if let Some(element_type) = runtime_array_element_type(expected) {
                    self.out.emit(Instruction::ConstrainArrayType(element_type));
                }
                Ok(())
            }
        }
    }

    fn compile_call_argument(
        &mut self,
        expr: &Expr,
        expected: Option<&crate::ast::TypeAnnotation>,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        if let Some(expected) = expected {
            self.compile_expr_typed_array(expr, expected, scopes)
        } else {
            self.compile_expr(expr, scopes)
        }
    }

    fn compile_expr(&mut self, expr: &Expr, scopes: &mut Scopes) -> Result<(), CodegenError> {
        let saved_source = self.out.enter_source(expr.span());
        let result = (|| {
            self.compile_expr_inner(expr, scopes)
                .map_err(|e| e.at(expr.span()))?;
            if matches!(
                expr.node(),
                Expr::Int(_)
                    | Expr::Float(_)
                    | Expr::Str(_)
                    | Expr::Bool(_)
                    | Expr::Nil
                    | Expr::Propagate(_)
            ) {
                return Ok(());
            }
            self.guard_error(scopes)
        })();
        self.out.leave_source(saved_source);
        result
    }

    fn compile_handled_expr(
        &mut self,
        expr: &Expr,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        let saved = self.error_handler.take();
        let boundary = self.cleanup_boundary.take();
        let result = self.compile_expr(expr, scopes);
        self.error_handler = saved;
        self.cleanup_boundary = boundary;
        result
    }

    fn guard_error(&mut self, scopes: &mut Scopes) -> Result<(), CodegenError> {
        let (slot, stack_depth, defer_baseline, label, merge) =
            if let Some(handler) = &self.error_handler {
                (
                    handler.slot,
                    handler.stack_depth,
                    handler.defer_baseline,
                    handler.label,
                    false,
                )
            } else if let Some(boundary) = &self.cleanup_boundary {
                (
                    boundary.primary_slot,
                    boundary.stack_depth,
                    boundary.defer_baseline,
                    boundary.label,
                    true,
                )
            } else {
                return Ok(());
            };
        let ok = self.fresh_label("error_checked");
        self.out
            .extend([Instruction::Dup, Instruction::IsType(ValueType::Error)]);
        self.out.jump(Instruction::JumpIfFalse(0), ok);
        if merge {
            self.merge_cleanup_error(slot);
        } else {
            self.out.emit(Instruction::Store(slot));
        }
        self.out.emit(Instruction::StackTruncate(stack_depth));
        let saved = self.error_handler.take();
        let primary = self.cleanup_primary.replace(slot);
        let cleanup: Vec<_> = self.active_defers[defer_baseline..].to_vec();
        for cleanup in cleanup.iter().rev() {
            self.compile_cleanup(cleanup, scopes)?;
        }
        self.cleanup_primary = primary;
        self.error_handler = saved;
        self.out.jump(Instruction::Jump(0), label);
        self.out.bind(ok);
        Ok(())
    }

    // A failed statement condition/range cannot choose a meaningful branch.
    // Return its error to the caller (or the surrounding try); ordinary value
    // expressions outside try still produce errors without early propagation.
    fn propagate_control_error(&mut self, scopes: &mut Scopes) -> Result<(), CodegenError> {
        if self.error_handler.is_some() || self.cleanup_boundary.is_some() {
            return self.guard_error(scopes);
        }
        self.propagate_error(scopes)
    }

    // Explicit ReturnError edge. Local handlers/boundaries are disabled only
    // on this edge; nested try inside the operand/cleanup still works normally.
    fn propagate_error(&mut self, scopes: &mut Scopes) -> Result<(), CodegenError> {
        let ok = self.fresh_label("control_checked");
        self.out
            .extend([Instruction::Dup, Instruction::IsType(ValueType::Error)]);
        self.out.jump(Instruction::JumpIfFalse(0), ok);
        let result_slot = if let Some(slot) = self.cleanup_primary {
            self.merge_cleanup_error(slot);
            slot
        } else {
            let slot = scopes.next_slot;
            scopes.next_slot += 1;
            self.out.emit(Instruction::Store(slot));
            slot
        };
        self.out.emit(Instruction::StackTruncate(0));
        let handler = self.error_handler.take();
        let boundary = self.cleanup_boundary.take();
        let primary = self.cleanup_primary.replace(result_slot);
        for cleanup in self.active_defers.clone().iter().rev() {
            self.compile_cleanup(cleanup, scopes)?;
        }
        self.cleanup_primary = primary;
        self.cleanup_boundary = boundary;
        self.error_handler = handler;
        self.out.emit(Instruction::Load(result_slot));
        self.out.emit(if self.in_function {
            Instruction::Return
        } else {
            Instruction::Halt
        });
        self.out.bind(ok);
        Ok(())
    }

    fn merge_cleanup_error(&mut self, primary_slot: usize) {
        self.out
            .extend([Instruction::Load(primary_slot), Instruction::Swap]);
        self.out.call_native(crate::runtime::cleanup_merge_import());
        self.out.emit(Instruction::Store(primary_slot));
    }

    fn compile_return(
        &mut self,
        scopes: &mut Scopes,
        deferred: &[Deferred],
    ) -> Result<(), CodegenError> {
        if !deferred.is_empty() {
            let result_slot = scopes.next_slot;
            scopes.next_slot += 1;
            self.out.emit(Instruction::Store(result_slot));
            let primary = self.cleanup_primary.replace(result_slot);
            for cleanup in deferred.iter().rev() {
                self.compile_cleanup(cleanup, scopes)?;
            }
            self.cleanup_primary = primary;
            self.out.emit(Instruction::Load(result_slot));
        }
        self.out.emit(Instruction::Return);
        Ok(())
    }

    fn validate_condition(
        &mut self,
        scopes: &mut Scopes,
        value_end: Option<Label>,
    ) -> Result<(), CodegenError> {
        self.out.extend([Instruction::Not, Instruction::Not]);
        if let Some(end) = value_end {
            self.guard_error(scopes)?;
            self.out
                .extend([Instruction::Dup, Instruction::IsType(ValueType::Error)]);
            self.out.jump(Instruction::JumpIfTrue(0), end);
        } else {
            self.propagate_control_error(scopes)?;
        }
        Ok(())
    }

    fn compile_try(
        &mut self,
        body: &Block,
        name: &str,
        catch: &Block,
        scopes: &mut Scopes,
        value: bool,
    ) -> Result<(), CodegenError> {
        let catch_label = self.fresh_label("try_catch");
        let end_label = self.fresh_label("try_end");
        scopes.push();
        let slot = scopes.next_slot;
        scopes.next_slot += 1;
        let saved = self.error_handler.replace(ErrorHandler {
            label: catch_label,
            slot,
            stack_depth: self.out.stack_depth(),
            defer_baseline: self.active_defers.len(),
            parent: self.error_handler.clone().map(Box::new),
        });
        self.compile_block_as_expr(body, scopes)?;
        if !value {
            self.out.emit(Instruction::Pop);
        }
        self.out.jump(Instruction::Jump(0), end_label);
        self.error_handler = saved;
        self.out.bind(catch_label);
        let bound = scopes.define(name);
        scopes.mark_const(name);
        self.out
            .extend([Instruction::Load(slot), Instruction::Store(bound)]);
        if value {
            self.compile_block_as_expr(catch, scopes)?;
        } else {
            self.compile_block_with_deferred(catch, scopes, self.active_defers.clone())?;
        }
        self.out.bind(end_label);
        scopes.pop();
        Ok(())
    }

    fn compile_expr_inner(&mut self, expr: &Expr, scopes: &mut Scopes) -> Result<(), CodegenError> {
        match expr.node() {
            Expr::BracketApply(_, _) | Expr::Apply(_, _) => {
                return Err(CodegenError::new(
                    "unresolved bracket or callable expression",
                ));
            }
            Expr::Located(_, _) => unreachable!("node removes locations"),
            Expr::Int(n) => self.out.emit(Instruction::Push(Value::Int(*n))),
            Expr::Float(n) => self.out.emit(Instruction::Push(Value::Float(*n))),
            Expr::Bytes(bytes) => {
                let mut hex = byte_literal_buffer(bytes.len())?;
                use std::fmt::Write;
                for byte in bytes {
                    write!(hex, "{byte:02x}").expect("writing to String");
                }
                self.out.emit(Instruction::Push(Value::Str(hex.into())));
                self.out.call_native(crate::runtime::bytes_literal_import());
            }
            Expr::Str(s) => self
                .out
                .emit(Instruction::Push(Value::Str(Rc::from(s.as_str())))),
            Expr::Interpolated(parts) => self.compile_interpolation(parts, scopes)?,
            Expr::Bool(b) => self.out.emit(Instruction::Push(Value::Bool(*b))),
            Expr::Nil => self.out.emit(Instruction::Push(Value::Nil)),
            Expr::TypeLiteral(_) => {
                return Err(CodegenError::new(
                    "compiler-only type values cannot be emitted at runtime",
                ));
            }
            Expr::Propagate(operand) => {
                self.compile_handled_expr(operand, scopes)?;
                self.propagate_error(scopes)?;
            }
            Expr::Fallback {
                kind,
                value,
                fallback,
            } => {
                let end = self.fresh_label("fallback_end");
                match kind {
                    FallbackKind::Nil => self.compile_expr(value, scopes)?,
                    FallbackKind::Error => self.compile_handled_expr(value, scopes)?,
                }
                let tested = match kind {
                    FallbackKind::Nil => ValueType::Nil,
                    FallbackKind::Error => ValueType::Error,
                };
                self.out
                    .extend([Instruction::Dup, Instruction::IsType(tested)]);
                self.out.jump(Instruction::JumpIfFalse(0), end);
                self.out.emit(Instruction::Pop);
                self.compile_expr(fallback, scopes)?;
                self.out.bind(end);
            }
            Expr::Ident(name) => match scopes.resolve(name) {
                Some(slot) => self.out.emit(Instruction::Load(slot)),
                None => {
                    let resolved = scopes
                        .resolve_import(name)
                        .unwrap_or_else(|| name.to_string());
                    let candidate = self.resolve_global_name(&resolved, Some(scopes))?;
                    if let Some(idx) = self.global_indices.get(&candidate) {
                        self.out.emit(Instruction::LoadGlobal(*idx));
                    } else {
                        return Err(CodegenError::new(format!("undefined variable `{name}`")));
                    }
                }
            },
            Expr::Array(items) | Expr::Tuple(items) => {
                for item in items {
                    self.compile_expr(item, scopes)?;
                }
                self.out.emit(if matches!(expr.node(), Expr::Tuple(_)) {
                    Instruction::MakeTuple(items.len())
                } else {
                    Instruction::MakeArray(items.len())
                });
            }
            Expr::Record(fields) => {
                for (name, value) in fields {
                    self.out
                        .emit(Instruction::Push(Value::Str(Rc::from(name.as_str()))));
                    self.compile_expr(value, scopes)?;
                }
                self.out.emit(Instruction::MakeRecord(fields.len()));
            }
            Expr::MethodCall(receiver, method, args) => {
                let site = crate::analysis::expression_site(self.current_function.as_deref(), expr);
                let extension = self
                    .call_specializations
                    .get(&site)
                    .or_else(|| self.method_calls.get(&site))
                    .map(String::as_str);
                self.compile_method_call(receiver, method, args, extension, scopes)?;
            }
            Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                let site = crate::analysis::expression_site(self.current_function.as_deref(), expr);
                let function_specialization = self.call_specializations.get(&site);
                let struct_specialization = self.struct_call_specializations.get(&site);
                let resolved = if let Some(specialization) = struct_specialization {
                    specialization.clone()
                } else if let Some(specialization) = function_specialization {
                    specialization.clone()
                } else {
                    self.resolve_function_name(name, scopes)?
                };
                let module = resolved.rsplit_once("::").map(|(m, _)| m.to_owned());
                if let Some(fields) = self.struct_defs.get(&resolved).cloned() {
                    self.compile_declared_call(
                        &resolved,
                        &fields,
                        args,
                        module,
                        DeclaredCall::Record,
                        scopes,
                    )?;
                } else if let Some(variant) = self.enum_defs.get(&resolved).cloned() {
                    self.compile_declared_call(
                        &resolved,
                        &variant.payload,
                        args,
                        module,
                        DeclaredCall::Variant,
                        scopes,
                    )?;
                } else if let Some(sig) = self.functions.get(&resolved).cloned() {
                    self.compile_declared_call(
                        &resolved,
                        &sig.params,
                        args,
                        sig.module,
                        DeclaredCall::Function,
                        scopes,
                    )?;
                } else if let Some(builtin) = Builtin::from_name(&resolved) {
                    self.compile_builtin_call(builtin, args, scopes)?;
                } else {
                    return Err(CodegenError::new(format!("unknown function `{name}`")));
                }
            }
            Expr::Index(arr, idx) => {
                self.compile_expr(arr, scopes)?;
                self.compile_expr(idx, scopes)?;
                self.out.emit(Instruction::Index);
            }
            Expr::Binary(op @ (BinOp::And | BinOp::Or), l, r) => {
                let error_label = self.fresh_label("logical_error");
                let end_label = self.fresh_label("logical_end");
                self.compile_expr(l, scopes)?;
                // Validate the left operand without converting strings/numbers
                // to booleans. Not propagates Error and reports invalid types.
                self.out.extend([
                    Instruction::Dup,
                    Instruction::Not,
                    Instruction::Dup,
                    Instruction::IsType(ValueType::Error),
                ]);
                self.out.jump(Instruction::JumpIfTrue(0), error_label);
                self.out.jump(
                    if *op == BinOp::And {
                        Instruction::JumpIfTrue(0)
                    } else {
                        Instruction::JumpIfFalse(0)
                    },
                    end_label,
                );
                self.out.emit(Instruction::Pop);
                self.compile_expr(r, scopes)?;
                self.out.extend([Instruction::Not, Instruction::Not]);
                self.out.jump(Instruction::Jump(0), end_label);
                self.out.bind(error_label);
                self.out.extend([Instruction::Swap, Instruction::Pop]);
                self.out.bind(end_label);
            }
            Expr::Binary(op, l, r) => {
                self.compile_expr(l, scopes)?;
                self.compile_expr(r, scopes)?;
                self.out.emit(binop_instruction(*op));
            }
            Expr::Unary(UnOp::Not, e) => {
                self.compile_expr(e, scopes)?;
                self.out.emit(Instruction::Not);
            }
            Expr::Unary(UnOp::Neg, e) => {
                self.out.emit(Instruction::Push(Value::Int(0)));
                self.compile_expr(e, scopes)?;
                self.out.emit(Instruction::Sub);
            }
            Expr::Cast(inner, ty) => {
                self.compile_expr(inner, scopes)?;
                let name = match ty {
                    VarType::Int => "str::to_int",
                    VarType::Float => "str::to_float",
                    VarType::Bool => "str::to_bool",
                    VarType::Str => "str::to_str",
                    VarType::Nil => {
                        return Err(CodegenError::new("casts to nil are not supported"));
                    }
                };
                self.out.call_native(
                    crate::runtime::NativeImport::named(name).expect("conversion import"),
                );
            }
            Expr::Block(block) => {
                self.compile_block_as_expr(block, scopes)?;
            }
            Expr::Match { value, arms, .. } => {
                scopes.push();
                self.compile_handled_expr(value, scopes)?;
                let value_slot = scopes.next_slot;
                scopes.next_slot += 1;
                self.out.emit(Instruction::Store(value_slot));
                let end = self.fresh_label("match_end");
                for arm in arms {
                    let next = self.fresh_label("match_next");
                    scopes.push();
                    if let MatchPattern::StringLiteral(value) = &arm.pattern {
                        self.out.extend([
                            Instruction::Load(value_slot),
                            Instruction::IsType(ValueType::Str),
                        ]);
                        self.out.jump(Instruction::JumpIfFalse(0), next);
                        self.out.extend([
                            Instruction::Load(value_slot),
                            Instruction::Push(Value::Str(Rc::from(value.as_str()))),
                            Instruction::Eq,
                        ]);
                        self.out.jump(Instruction::JumpIfFalse(0), next);
                    }
                    if let MatchPattern::Type(name) = &arm.pattern {
                        let ty = self.type_test(name, scopes)?;
                        self.out
                            .extend([Instruction::Load(value_slot), Instruction::IsType(ty)]);
                        self.out.jump(Instruction::JumpIfFalse(0), next);
                    }
                    if let MatchPattern::Variant { name, bindings } = &arm.pattern {
                        let name = self.resolve_function_name(name, scopes)?;
                        let variant = self.enum_defs.get(&name).cloned().ok_or_else(|| {
                            CodegenError::new(format!("unknown enum variant `{name}`"))
                        })?;
                        self.out.extend([
                            Instruction::Load(value_slot),
                            Instruction::IsType(ValueType::Variant(Rc::from(name.as_str()))),
                        ]);
                        self.out.jump(Instruction::JumpIfFalse(0), next);
                        for (binding, field) in bindings.iter().zip(&variant.payload) {
                            if binding != "_" {
                                let slot = scopes.define(binding);
                                scopes.mark_const(binding);
                                self.out.extend([
                                    Instruction::Load(value_slot),
                                    Instruction::Push(Value::Str(Rc::from(field.name.as_str()))),
                                    Instruction::Index,
                                    Instruction::Store(slot),
                                ]);
                            }
                        }
                    }
                    if let Some(guard) = &arm.guard {
                        self.compile_expr(guard, scopes)?;
                        self.validate_condition(scopes, Some(end))?;
                        self.out.jump(Instruction::JumpIfFalse(0), next);
                    }
                    self.compile_block_as_expr(&arm.body, scopes)?;
                    self.out.jump(Instruction::Jump(0), end);
                    self.out.bind(next);
                    scopes.pop();
                }
                self.out.emit(Instruction::Push(Value::Nil));
                self.out.bind(end);
                scopes.pop();
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.compile_try(body, catch_name, catch_block, scopes, true)?;
            }

            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let else_label = self.fresh_label("if_else");
                let end_label = self.fresh_label("if_end");
                self.compile_expr(cond, scopes)?;
                self.validate_condition(scopes, Some(end_label))?;
                self.out.jump(Instruction::JumpIfFalse(0), else_label);
                self.compile_expr(then_branch, scopes)?;
                self.out.jump(Instruction::Jump(0), end_label);
                self.out.bind(else_label);
                if let Some(branch) = else_branch {
                    self.compile_expr(branch, scopes)?;
                } else {
                    self.out.emit(Instruction::Push(Value::Nil));
                }
                self.out.bind(end_label);
            }
        }
        Ok(())
    }

    fn compile_discarded_expr(
        &mut self,
        expr: &Expr,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        match expr.node() {
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let otherwise = self.fresh_label("if_else");
                let end = self.fresh_label("if_end");
                self.compile_expr(cond, scopes)?;
                self.validate_condition(scopes, None)?;
                self.out.jump(Instruction::JumpIfFalse(0), otherwise);
                self.compile_discarded_expr(then_branch, scopes)?;
                self.out.jump(Instruction::Jump(0), end);
                self.out.bind(otherwise);
                if let Some(branch) = else_branch {
                    self.compile_discarded_expr(branch, scopes)?;
                }
                self.out.bind(end);
            }
            Expr::Block(block) => {
                self.compile_block_with_deferred(block, scopes, self.active_defers.clone())?
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => self.compile_try(body, catch_name, catch_block, scopes, false)?,
            _ => {
                self.compile_expr(expr, scopes)?;
                self.out.emit(Instruction::Pop);
            }
        }
        Ok(())
    }

    fn compile_block_as_expr(
        &mut self,
        block: &Block,
        scopes: &mut Scopes,
    ) -> Result<(), CodegenError> {
        self.compile_block_result(block, scopes, self.active_defers.clone(), true, None)
    }
}

// resolved: `<<`/`>>` now compile to their own arrpush/arrprepend mnemonics
// (see interpreter.rs) instead of reusing `add`, so array mutation is actually
// visible through aliases and a bare `arr << x;` has an effect.
fn binop_instruction(op: BinOp) -> Instruction {
    match op {
        BinOp::Add => Instruction::Add,
        // `arr << x` / `x >> arr`: these mutate the array in place (see
        // Instruction::ArrayPush/ArrayPrepend in interpreter.rs) rather than
        // reusing `add`'s allocate-a-new-array behavior — a bare `arr << x;`
        // statement needs the original `arr` to actually change, not just the
        // (immediately discarded) expression result.
        BinOp::Append => Instruction::ArrayPush,
        BinOp::Prepend => Instruction::ArrayPrepend,
        BinOp::Sub => Instruction::Sub,
        BinOp::Mul => Instruction::Mul,
        BinOp::Div => Instruction::Div,
        BinOp::Mod => Instruction::Mod,
        BinOp::Eq => Instruction::Eq,
        BinOp::NotEq => Instruction::Ne,
        BinOp::Lt => Instruction::Lt,
        BinOp::LtEq => Instruction::Le,
        BinOp::Gt => Instruction::Gt,
        BinOp::GtEq => Instruction::Ge,
        BinOp::And => Instruction::And,
        BinOp::Or => Instruction::Or,
        BinOp::Xor => Instruction::Xor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::TypeAnnotation;

    #[test]
    fn byte_literal_reservation_reports_size_and_allocation_failures() {
        assert!(
            byte_literal_buffer(usize::MAX)
                .unwrap_err()
                .message
                .contains("size overflow")
        );
        assert!(
            byte_literal_buffer(usize::MAX / 2)
                .unwrap_err()
                .message
                .contains("cannot allocate")
        );
    }

    #[test]
    fn byte_literals_round_trip_and_preserve_all_byte_values() {
        for data in [Vec::new(), (0..=255).collect::<Vec<u8>>()] {
            // Bytes are materialized by build-input evaluation, not source syntax.
            let mut parsed = crate::frontend::parse("bytes.cog", "fn main() { nil }").program;
            let Item::Function(main) = &mut parsed.items[0] else {
                unreachable!()
            };
            main.body.tail = Some(Box::new(Expr::Bytes(data.clone())));
            let program = super::compile(&parsed).unwrap();
            let expected: String = data.iter().map(|byte| format!("{byte:02x}")).collect();
            assert!(program.instructions.iter().any(|instruction| matches!(instruction, Instruction::Push(Value::Str(text)) if text.as_ref() == expected)));
            let decoded =
                crate::bytecode::decode(&crate::bytecode::encode(&program).unwrap()).unwrap();
            crate::interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
        }
    }

    fn compile(program: &Program) -> Result<String, CodegenError> {
        super::compile(program).map(|p| p.disassemble())
    }
    fn compile_with_modules(
        program: &Program,
        modules: &HashMap<String, String>,
    ) -> Result<String, CodegenError> {
        super::compile_with_modules(program, modules).map(|p| p.disassemble())
    }

    #[test]
    fn duplicate_function_names_rejected() {
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![],
                    },
                }),
            ],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("defined more than once"));
    }

    #[test]
    fn nil_literal_compiles_to_push_nil() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::Return(Some(Expr::Nil))],
                },
            })],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("push nil\nret\n"));
        assert_eq!(out.matches("ret\n").count(), 1);
    }

    #[test]
    fn const_assignment_rejected() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![
                        Stmt::Const {
                            name: "MAX".to_string(),
                            ty: None,
                            value: Expr::Int(10),
                        },
                        Stmt::Assign {
                            target: Expr::Ident("MAX".to_string()),
                            value: Expr::Int(20),
                        },
                        Stmt::Return(Some(Expr::Ident("MAX".to_string()))),
                    ],
                },
            })],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("cannot assign to const `MAX`"));
    }

    #[test]
    fn reassignment_stores_to_existing_slot() {
        // fn main() { let x = 1; x = 2; return x; }
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![
                        Stmt::VarDecl {
                            ty: None,
                            name: "x".to_string(),
                            value: Expr::Int(1),
                        },
                        Stmt::Assign {
                            target: Expr::Ident("x".to_string()),
                            value: Expr::Int(2),
                        },
                        Stmt::Return(Some(Expr::Ident("x".to_string()))),
                    ],
                },
            })],
        };
        let out = compile(&program).expect("should compile");
        // let x = 1; and x = 2; both store in one slot
        assert_eq!(out.matches("store 0\n").count(), 2);
        assert!(!out.contains("store 1\n"));
    }

    #[test]
    fn assigning_undefined_variable_rejected() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::Assign {
                        target: Expr::Ident("x".to_string()),
                        value: Expr::Int(2),
                    }],
                },
            })],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("undefined variable"));
    }

    #[test]
    fn assigning_to_non_lvalue_rejected() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::Assign {
                        target: Expr::Int(1),
                        value: Expr::Int(2),
                    }],
                },
            })],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("invalid assignment target"));
    }

    #[test]
    fn indexed_assignment_uses_indexset() {
        // fn main() { let arr = [1]; arr[0] = 9; return arr; }
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![
                        Stmt::VarDecl {
                            ty: None,
                            name: "arr".to_string(),
                            value: Expr::Array(vec![Expr::Int(1)]),
                        },
                        Stmt::Assign {
                            target: Expr::Index(
                                Box::new(Expr::Ident("arr".to_string())),
                                Box::new(Expr::Int(0)),
                            ),
                            value: Expr::Int(9),
                        },
                        Stmt::Return(Some(Expr::Ident("arr".to_string()))),
                    ],
                },
            })],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("indexset\n"));
    }

    #[test]
    fn conversion_call_produces_one_result() {
        // fn main() { return to_str(1); }
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "str".into(),
                    from: None,
                    names: vec!["to_str".into()],
                    aliases: vec![None],
                    namespace: false,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "to_str".to_string(),
                            vec![CallArgument::positional(Expr::Int(1))],
                        )))],
                    },
                }),
            ],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("push 1\ncall_native 0\nret\n"));
    }

    #[test]
    fn sleep_call_compiles_without_extra_push() {
        // fn main() { time::sleep(1); return; }
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "time".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![
                            Stmt::Expr(Expr::Call(
                                "time::sleep".to_string(),
                                vec![CallArgument::positional(Expr::Int(1))],
                            )),
                            Stmt::Return(None),
                        ],
                    },
                }),
            ],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("push 1\ncall_native 0\npop\n"));
    }

    #[test]
    fn explicit_module_path_resolves_with_use() {
        // A linked program calls `random::next()` through an explicitly
        // imported namespace.
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "random".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "random::next".to_string(),
                            vec![],
                        )))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "random::next".to_string(),
                    visibility: Visibility::Public,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Int(4)))],
                    },
                }),
            ],
        };

        let mut function_modules = HashMap::new();
        function_modules.insert("random::next".to_string(), "random".to_string());
        let out = compile_with_modules(&program, &function_modules).expect("should compile");
        // The qualified module path is also the bytecode label.
        assert!(out.contains("call random::next 0\n"));
        assert!(out.contains("random::next:\n"));
    }

    #[test]
    fn builtin_wrong_arity_rejected() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "str".into(),
                    from: None,
                    names: vec!["to_str".into()],
                    aliases: vec![None],
                    namespace: false,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Expr(Expr::Call("to_str".to_string(), vec![]))],
                    },
                }),
            ],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("`to_str` expects 1 argument"));
    }

    #[test]
    fn defer_block_runs_before_return() {
        let program = crate::project::compile_source("cleanup.cog",
            "include std; use io; fn work() { defer { io::println(1); } return 2; } fn main() { io::println(work()); }")
            .expect("should compile");
        let program = crate::bytecode::decode(&crate::bytecode::encode(&program).unwrap()).unwrap();
        let mut output = Vec::new();
        crate::interpreter::run_with(&program, &mut output).unwrap();
        assert_eq!(output, b"1\n2\n");
    }

    #[test]
    fn defer_block_runs_at_scope_end() {
        let compiled = crate::project::compile_source(
            "defer.cog",
            "include std; use io; fn main() { defer { io::println(9); } io::println(7); }",
        )
        .unwrap();
        let decoded =
            crate::bytecode::decode(&crate::bytecode::encode(&compiled).unwrap()).unwrap();
        let mut output = Vec::new();
        crate::interpreter::run_with(&decoded, &mut output).unwrap();
        assert_eq!(output, b"7\n9\n");
    }

    #[test]
    fn try_catch_guards_error_results() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "str".into(),
                    from: None,
                    names: vec!["to_str".into()],
                    aliases: vec![None],
                    namespace: false,
                    wildcard: false,
                }),
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "str".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Try {
                            body: Block {
                                tail: None,
                                stmts: vec![Stmt::Expr(Expr::Call(
                                    "str::to_int".to_string(),
                                    vec![CallArgument::positional(Expr::Str("abc".to_string()))],
                                ))],
                            },
                            catch_name: "e".to_string(),
                            catch_block: Block {
                                tail: None,
                                stmts: vec![Stmt::Expr(Expr::Call(
                                    "to_str".to_string(),
                                    vec![CallArgument::positional(Expr::Ident("e".to_string()))],
                                ))],
                            },
                        }],
                    },
                }),
            ],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("is_type error\n"));
        assert!(out.contains("store "));
    }

    #[test]
    fn append_and_prepend_compile_to_dedicated_mnemonics() {
        // arr << 1; 2 >> arr;
        // must NOT compile to `add` (see interpreter.rs's ArrayPush/ArrayPrepend
        // for why: `add` allocates a new array and leaves the original untouched,
        // so a bare `arr << x;` statement would silently do nothing).
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![
                        Stmt::VarDecl {
                            ty: None,
                            name: "arr".to_string(),
                            value: Expr::Array(vec![]),
                        },
                        Stmt::Expr(Expr::Binary(
                            BinOp::Append,
                            Box::new(Expr::Ident("arr".to_string())),
                            Box::new(Expr::Int(1)),
                        )),
                        Stmt::Expr(Expr::Binary(
                            BinOp::Prepend,
                            Box::new(Expr::Int(2)),
                            Box::new(Expr::Ident("arr".to_string())),
                        )),
                    ],
                },
            })],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("arrpush\n"));
        assert!(out.contains("arrprepend\n"));
    }

    #[test]
    fn named_call_requires_nullable_param_without_default() {
        // Nullable permits nil as a value; it does not make an argument optional.
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "add".to_string(),
                    visibility: Visibility::Private,
                    params: vec![
                        Param {
                            visibility: Visibility::Public,
                            name: "a".to_string(),
                            ty: Some(TypeAnnotation {
                                kind: crate::ast::TypeKind::Primitive(VarType::Int),
                                nilable: true,
                            }),
                            default: None,
                        },
                        Param {
                            visibility: Visibility::Public,
                            name: "b".to_string(),
                            ty: Some(TypeAnnotation {
                                kind: crate::ast::TypeKind::Primitive(VarType::Int),
                                nilable: true,
                            }),
                            default: None,
                        },
                    ],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("a".to_string())))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "add".to_string(),
                            vec![CallArgument::named("b".to_string(), Expr::Int(2))],
                        )))],
                    },
                }),
            ],
        };
        let error = compile(&program).expect_err("missing required nullable argument");
        assert!(error.message.contains("expects argument `a`"));
    }

    #[test]
    fn positional_call_may_omit_trailing_defaulted_params() {
        // fn add(a: int, b: int = 5) { return a; }
        // fn main() { return add(1); }  -- `b` omitted positionally, uses its default
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "add".to_string(),
                    visibility: Visibility::Private,
                    params: vec![
                        Param {
                            visibility: Visibility::Public,
                            name: "a".to_string(),
                            ty: None,
                            default: None,
                        },
                        Param {
                            visibility: Visibility::Public,
                            name: "b".to_string(),
                            ty: None,
                            default: Some(Expr::Int(5)),
                        },
                    ],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("a".to_string())))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "add".to_string(),
                            vec![CallArgument::positional(Expr::Int(1))],
                        )))],
                    },
                }),
            ],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("push 1\npush 5\ncall add 2\n"));
    }

    #[test]
    fn named_call_with_unknown_parameter_name_is_rejected() {
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "add".to_string(),
                    visibility: Visibility::Private,
                    params: vec![Param {
                        visibility: Visibility::Public,
                        name: "a".to_string(),
                        ty: None,
                        default: None,
                    }],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("a".to_string())))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "add".to_string(),
                            vec![CallArgument::named("nope".to_string(), Expr::Int(1))],
                        )))],
                    },
                }),
            ],
        };
        let err = compile(&program).expect_err("unknown named argument must be rejected");
        assert!(err.message.contains("nope"));
    }

    #[test]
    fn named_call_with_duplicate_name_is_rejected() {
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "add".to_string(),
                    visibility: Visibility::Private,
                    params: vec![
                        Param {
                            visibility: Visibility::Public,
                            name: "a".to_string(),
                            ty: None,
                            default: None,
                        },
                        Param {
                            visibility: Visibility::Public,
                            name: "b".to_string(),
                            ty: None,
                            default: None,
                        },
                    ],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("a".to_string())))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "add".to_string(),
                            vec![
                                CallArgument::named("a".to_string(), Expr::Int(1)),
                                CallArgument::named("a".to_string(), Expr::Int(2)),
                            ],
                        )))],
                    },
                }),
            ],
        };
        let err = compile(&program).expect_err("duplicate named argument must be rejected");
        assert!(err.message.contains("duplicate"));
    }

    #[test]
    fn error_pattern_compiles_to_a_generic_runtime_predicate() {
        let compiled = crate::project::compile_source(
            "types.cog",
            "include std; use str; use err::Error; fn main() { when str::to_int(\"abc\") is Error {} else { crash(\"expected Error\"); } }",
        )
        .unwrap();
        assert!(
            compiled
                .instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::IsType(ValueType::Error)))
        );
        crate::interpreter::run_with(&compiled, &mut Vec::new()).unwrap();
    }

    #[test]
    fn if_expression_uses_tail_and_returns_value() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::Return(Some(Expr::If {
                        cond: Box::new(Expr::Bool(true)),
                        then_branch: Box::new(Expr::Block(Block {
                            tail: Some(Box::new(Expr::Int(7))),
                            stmts: vec![],
                        })),
                        else_branch: Some(Box::new(Expr::Block(Block {
                            tail: Some(Box::new(Expr::Int(9))),
                            stmts: vec![],
                        }))),
                    }))],
                },
            })],
        };
        let out = compile(&program).expect("should compile");
        assert!(out.contains("push true\nnot\nnot\n"));
        assert!(out.contains("push 7\n"));
    }

    #[test]
    fn user_function_shadows_builtin() {
        // fn to_str(x) { return x; }  fn main() { return to_str(5); }
        let program = Program {
            items: vec![
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "to_str".to_string(),
                    visibility: Visibility::Private,
                    params: vec![Param {
                        visibility: Visibility::Public,
                        name: "x".to_string(),
                        ty: None,
                        default: None,
                    }],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("x".to_string())))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "to_str".to_string(),
                            vec![CallArgument::positional(Expr::Int(5))],
                        )))],
                    },
                }),
            ],
        };
        let out = compile(&program).expect("should compile");
        // A user function with this short name is an ordinary call.
        assert!(out.contains("call to_str 1\n"));
    }

    #[test]
    fn missing_main_rejected() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "helper".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![],
                },
            })],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("main"));
    }

    #[test]
    fn compile_break_inside_loop_ok() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::While {
                        cond: Expr::Bool(true),
                        body: Block {
                            tail: None,
                            stmts: vec![Stmt::Break],
                        },
                    }],
                },
            })],
        };
        let out = compile(&program);
        assert!(out.is_ok());
    }

    #[test]
    fn break_runs_defer_registered_inside_the_loop() {
        let program = crate::project::compile_source(
            "cleanup.cog",
            "include std; use io; fn main() { while true { defer { io::println(1); } break; } io::println(2); }",
        )
        .expect("should compile");
        let program = crate::bytecode::decode(&crate::bytecode::encode(&program).unwrap()).unwrap();
        let mut output = Vec::new();
        crate::interpreter::run_with(&program, &mut output).unwrap();
        assert_eq!(output, b"1\n2\n");
    }

    #[test]
    fn break_does_not_run_defer_registered_before_the_loop() {
        // fn main() { defer { to_str(1); } while true { break; } return nil; }
        let compiled = crate::project::compile_source(
            "cleanup.cog",
            "include std; use io; fn main() { defer { io::print(1); } while true { break; } return nil; }",
        )
        .unwrap();
        let mut output = Vec::new();
        crate::interpreter::run_with(&compiled, &mut output).unwrap();
        assert_eq!(output, b"1");
    }

    #[test]
    fn compile_break_outside_loop_rejected() {
        let program = Program {
            items: vec![Item::Function(FunctionDecl {
                owner: None,
                comptime: false,
                type_params: vec![],
                result_type: None,
                result_span: None,
                name: "main".to_string(),
                visibility: Visibility::Private,
                params: vec![],
                body: Block {
                    tail: None,
                    stmts: vec![Stmt::Break],
                },
            })],
        };
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("break"));
    }
    #[test]
    fn private_function_cannot_be_called_from_another_module() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "hidden".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "hidden::secret".to_string(),
                            vec![],
                        )))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "hidden::secret".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Int(1)))],
                    },
                }),
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".to_string(), "app".to_string());
        modules.insert("hidden::secret".to_string(), "hidden".to_string());
        let err = compile_with_modules(&program, &modules).unwrap_err();
        assert!(err.message.contains("private"));
    }

    #[test]
    fn public_function_can_be_called_by_qualified_name() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "math".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "math::answer".to_string(),
                            vec![],
                        )))],
                    },
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "math::answer".to_string(),
                    visibility: Visibility::Public,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Int(42)))],
                    },
                }),
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".to_string(), "app".to_string());
        modules.insert("math::answer".to_string(), "math".to_string());
        let out = compile_with_modules(&program, &modules).expect("public call should compile");
        assert!(out.contains("call math::answer 0\n"));
    }

    #[test]
    fn compiles_len_as_a_receiver_method() {
        let tokens = crate::lexer::tokenize("fn main() { const n = [1, 2, 3].len(); return n; }")
            .expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let out = compile(&program).expect("method should compile");
        assert!(out.contains("len\n"));
    }

    #[test]
    fn parses_public_struct_and_enum() {
        let tokens = crate::lexer::tokenize(
            "public struct User { name: str } public enum State { Ready, Failed(code: int) }",
        )
        .expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        assert!(matches!(
            &program.items[0],
            Item::StructDecl { name, visibility: Visibility::Public, .. } if name == "User"
        ));
        assert!(matches!(
            &program.items[1],
            Item::EnumDecl { name, visibility: Visibility::Public, .. } if name == "State"
        ));
    }

    #[test]
    fn private_struct_cannot_be_constructed_from_another_module() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "hidden".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "hidden::Secret".to_string(),
                            vec![],
                        )))],
                    },
                }),
                Item::StructDecl {
                    name: "hidden::Secret".to_string(),
                    visibility: Visibility::Private,
                    type_params: vec![],
                    fields: vec![],
                    owner: None,
                },
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".to_string(), "app".to_string());
        let err = compile_with_modules(&program, &modules).unwrap_err();
        assert!(err.message.contains("struct `hidden::Secret` is private"));
    }

    #[test]
    fn public_struct_can_be_constructed_from_another_module() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "hidden".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".to_string(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Call(
                            "hidden::PublicData".to_string(),
                            vec![CallArgument::positional(Expr::Int(7))],
                        )))],
                    },
                }),
                Item::StructDecl {
                    name: "hidden::PublicData".to_string(),
                    visibility: Visibility::Public,
                    type_params: vec![],
                    fields: vec![crate::ast::StructField {
                        visibility: Visibility::Public,
                        name: "value".to_string(),
                        ty: Some(crate::ast::TypeAnnotation {
                            kind: crate::ast::TypeKind::Primitive(VarType::Int),
                            nilable: false,
                        }),
                        default: None,
                    }],
                    owner: None,
                },
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".to_string(), "app".to_string());
        let out = compile_with_modules(&program, &modules).expect("public struct should compile");
        assert!(out.contains("record 2\n"));
    }

    #[test]
    fn compiles_unwrap_without_an_err_wrapper_function() {
        let tokens = crate::lexer::tokenize(
            "use str; fn main() { const n = str::to_int(\"42\").unwrap(); return n; }",
        )
        .expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let out = compile(&program).expect("method should compile");
        assert!(out.contains("dup\nis_type error\n"));
        assert!(out.contains("crash \"unwrap called on an error value\"\n"));
        assert!(!out.contains("call err::unwrap\n"));
    }

    #[test]
    fn compiles_global_const_and_var() {
        let tokens = crate::lexer::tokenize(
            r#"
            const answer: int = 42;
            var counter: int = 0;
            fn main() {
                counter = answer;
                return counter;
            }
        "#,
        )
        .expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let out = compile(&program).expect("global declarations should compile");
        assert!(out.contains("store_global 0\n"));
        assert!(out.contains("store_global 1\n"));
        assert!(out.contains("load_global 0\n"));
        assert!(out.contains("store_global 1\n"));
    }

    #[test]
    fn global_const_cannot_be_assigned() {
        let tokens = crate::lexer::tokenize(
            r#"
            const answer = 42;
            fn main() { answer = 7; }
        "#,
        )
        .expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("cannot assign to const `answer`"));
    }

    #[test]
    fn public_global_can_be_read_from_another_module() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "hidden".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".into(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("hidden::VERSION".into())))],
                    },
                }),
                Item::GlobalConst {
                    name: "hidden::VERSION".into(),
                    visibility: Visibility::Public,
                    ty: None,
                    value: Expr::Int(1),
                },
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".into(), "app".into());
        let out = compile_with_modules(&program, &modules).expect("public global should compile");
        assert!(out.contains("load_global 0\n"));
    }

    #[test]
    fn private_global_is_rejected_from_another_module() {
        let program = Program {
            items: vec![
                Item::Use(crate::ast::UseDecl {
                    span: None,
                    module: "hidden".into(),
                    from: None,
                    names: vec![],
                    aliases: vec![None],
                    namespace: true,
                    wildcard: false,
                }),
                Item::Function(FunctionDecl {
                    owner: None,
                    comptime: false,
                    type_params: vec![],
                    result_type: None,
                    result_span: None,
                    name: "main".into(),
                    visibility: Visibility::Private,
                    params: vec![],
                    body: Block {
                        tail: None,
                        stmts: vec![Stmt::Return(Some(Expr::Ident("hidden::SECRET".into())))],
                    },
                }),
                Item::GlobalConst {
                    name: "hidden::SECRET".into(),
                    visibility: Visibility::Private,
                    ty: None,
                    value: Expr::Int(1),
                },
            ],
        };
        let mut modules = HashMap::new();
        modules.insert("main".into(), "app".into());
        let err = compile_with_modules(&program, &modules).unwrap_err();
        assert!(err.message.contains("global `hidden::SECRET` is private"));
    }

    #[test]
    fn qualified_builtin_requires_declared_namespace() {
        let tokens = crate::lexer::tokenize("fn main() { io::stdout(); }").expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("undeclared module `io`"));
    }

    #[test]
    fn namespace_import_allows_qualified_builtin() {
        let tokens = crate::lexer::tokenize("use io; fn main() { io::stdout(); }").expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let out = compile(&program).expect("declared namespace should allow qualified builtin");
        assert!(out.contains("call_native 0\n"));
    }

    #[test]
    fn unknown_bare_function_is_reported_as_unknown_function() {
        let tokens = crate::lexer::tokenize("fn main() { does_not_exist(); }").expect("lex");
        let program = crate::hlparser::parse_program(&tokens).expect("parse");
        let err = compile(&program).unwrap_err();
        assert!(err.message.contains("unknown function `does_not_exist`"));
    }
}
