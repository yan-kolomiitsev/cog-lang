//! Bounded monomorphization of generic functions after semantic inference.
use crate::analysis::{self, CheckResult};
use crate::ast::{
    Block, Expr, FunctionDecl, Item, Program, Stmt, StringPart, TypeAnnotation, TypeKind,
};
use crate::diagnostic::Diagnostic;
use std::collections::HashSet;

const MAX_SPECIALIZATIONS: usize = 256;

pub(crate) fn expand(
    source: &Program,
    function_modules: &std::collections::HashMap<String, String>,
) -> Result<(Program, CheckResult), Vec<Diagnostic>> {
    let mut allocation_cost = crate::ast_budget::program(source).map_err(budget_error)?;
    let mut program = source.clone();
    let mut generated = HashSet::new();
    for _ in 0..MAX_SPECIALIZATIONS {
        let checked = analysis::check_with_methods(&program, function_modules);
        if !checked.diagnostics.is_empty() {
            return Err(checked.diagnostics);
        }

        let mut additions = Vec::new();
        for specialization in &checked.specializations {
            if !generated.insert(specialization.name.clone()) {
                continue;
            }
            let Some(template) = function_template(&program, &specialization.template) else {
                return Err(vec![Diagnostic::new(
                    "E4003",
                    format!(
                        "generic function template `{}` is unavailable",
                        specialization.template
                    ),
                    None,
                )]);
            };
            check_allocation(
                allocation_cost,
                crate::ast_budget::function(template).map_err(budget_error)?,
            )?;
            let mut instance = template.clone();
            specialize_function(&mut instance, &specialization.arguments);
            instance.name.clone_from(&specialization.name);
            instance.type_params.clear();
            let instance = Item::Function(instance);
            allocation_cost = check_allocation(
                allocation_cost,
                crate::ast_budget::item(&instance).map_err(budget_error)?,
            )?;
            additions.push(instance);
            if generated.len() > MAX_SPECIALIZATIONS {
                return Err(vec![Diagnostic::new(
                    "E4003",
                    format!("generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"),
                    None,
                )]);
            }
        }
        for specialization in &checked.struct_specializations {
            if !generated.insert(specialization.name.clone()) {
                continue;
            }
            let Some(Item::StructDecl {
                visibility,
                fields,
                owner,
                ..
            }) = program.items.iter().find(|item| {
                matches!(item, Item::StructDecl { name, type_params, .. }
                    if name == &specialization.template && !type_params.is_empty())
            })
            else {
                return Err(vec![Diagnostic::new(
                    "E4003",
                    format!(
                        "generic struct template `{}` is unavailable",
                        specialization.template
                    ),
                    None,
                )]);
            };
            check_allocation(
                allocation_cost,
                crate::ast_budget::fields(fields).map_err(budget_error)?,
            )?;
            let mut instance_fields = fields.clone();
            for field in &mut instance_fields {
                if let Some(ty) = &mut field.ty {
                    substitute_type(ty, &specialization.arguments);
                }
                if let Some(default) = &mut field.default {
                    substitute_expr(default, &specialization.arguments);
                }
            }
            let instance = Item::StructDecl {
                name: specialization.name.clone(),
                visibility: *visibility,
                type_params: Vec::new(),
                fields: instance_fields,
                owner: owner.clone(),
            };
            allocation_cost = check_allocation(
                allocation_cost,
                crate::ast_budget::item(&instance).map_err(budget_error)?,
            )?;
            additions.push(instance);
            if generated.len() > MAX_SPECIALIZATIONS {
                return Err(vec![Diagnostic::new(
                    "E4003",
                    format!("generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"),
                    None,
                )]);
            }
        }
        if additions.is_empty() {
            return Ok((program, checked));
        }
        program.items.extend(additions);
    }
    Err(vec![Diagnostic::new(
        "E4003",
        format!("generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"),
        None,
    )])
}

fn function_template<'a>(program: &'a Program, name: &str) -> Option<&'a FunctionDecl> {
    program.items.iter().find_map(|item| match item {
        Item::Function(function) if function.name == name => Some(function),
        Item::Extend(extension) => extension
            .methods
            .iter()
            .find(|method| method.function.name == name)
            .map(|method| &method.function),
        _ => None,
    })
}

fn budget_error(message: String) -> Vec<Diagnostic> {
    vec![Diagnostic::new("E4003", message, None)]
}

fn check_allocation(current: usize, additional: usize) -> Result<usize, Vec<Diagnostic>> {
    current
        .checked_add(additional)
        .filter(|cost| *cost <= crate::ast_budget::MAX_COST)
        .ok_or_else(|| budget_error("compiler specialization allocation budget exceeded".into()))
}

fn specialize_function(
    function: &mut FunctionDecl,
    arguments: &std::collections::HashMap<String, TypeAnnotation>,
) {
    for parameter in &mut function.params {
        if let Some(ty) = &mut parameter.ty {
            substitute_type(ty, arguments);
        }
        if let Some(default) = &mut parameter.default {
            substitute_expr(default, arguments);
        }
    }
    if let Some(result) = &mut function.result_type {
        substitute_type(result, arguments);
    }
    substitute_block(&mut function.body, arguments);
}

fn substitute_type(
    ty: &mut TypeAnnotation,
    arguments: &std::collections::HashMap<String, TypeAnnotation>,
) {
    if let TypeKind::TypeParameter(name) = &ty.kind {
        if let Some(replacement) = arguments.get(name) {
            let nilable = ty.nilable || replacement.nilable;
            *ty = replacement.clone();
            ty.nilable = nilable;
        }
        return;
    }
    match &mut ty.kind {
        TypeKind::Array(element) => substitute_type(element, arguments),
        TypeKind::Applied {
            arguments: types, ..
        }
        | TypeKind::ResolvedApplied {
            arguments: types, ..
        } => {
            for argument in types {
                substitute_type(argument, arguments);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                substitute_type(element, arguments);
            }
        }
        TypeKind::Primitive(_)
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => {}
    }
}

fn substitute_block(
    block: &mut Block,
    arguments: &std::collections::HashMap<String, TypeAnnotation>,
) {
    for statement in &mut block.stmts {
        substitute_stmt(statement, arguments);
    }
    if let Some(tail) = &mut block.tail {
        substitute_expr(tail, arguments);
    }
}

fn substitute_stmt(
    statement: &mut Stmt,
    arguments: &std::collections::HashMap<String, TypeAnnotation>,
) {
    match statement {
        Stmt::Located(_, inner) => substitute_stmt(inner, arguments),
        Stmt::VarDecl { ty, value, .. }
        | Stmt::Const { ty, value, .. }
        | Stmt::Comptime { ty, value, .. } => {
            if let Some(ty) = ty {
                substitute_type(ty, arguments);
            }
            substitute_expr(value, arguments);
        }
        Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
            substitute_expr(target, arguments);
            substitute_expr(value, arguments);
        }
        Stmt::Return(value) => {
            if let Some(value) = value {
                substitute_expr(value, arguments);
            }
        }
        Stmt::Defer { body } | Stmt::While { body, .. } => substitute_block(body, arguments),
        Stmt::Try {
            body, catch_block, ..
        } => {
            substitute_block(body, arguments);
            substitute_block(catch_block, arguments);
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
        }
        | Stmt::When {
            value: cond,
            then_block,
            else_block,
            ..
        } => {
            substitute_expr(cond, arguments);
            substitute_block(then_block, arguments);
            if let Some(else_block) = else_block {
                substitute_block(else_block, arguments);
            }
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            substitute_expr(start, arguments);
            substitute_expr(end, arguments);
            if let Some(step) = step {
                substitute_expr(step, arguments);
            }
            substitute_block(body, arguments);
        }
        Stmt::ForEach { iterable, body, .. } => {
            substitute_expr(iterable, arguments);
            substitute_block(body, arguments);
        }
        Stmt::Crash { value, .. } | Stmt::Expr(value) => substitute_expr(value, arguments),
        Stmt::Invalid { .. } | Stmt::Use { .. } | Stmt::Break | Stmt::Continue => {}
    }
}

fn substitute_expr(expr: &mut Expr, arguments: &std::collections::HashMap<String, TypeAnnotation>) {
    match expr {
        Expr::Located(_, inner)
        | Expr::Propagate(inner)
        | Expr::Unary(_, inner)
        | Expr::Cast(inner, _) => substitute_expr(inner, arguments),
        Expr::TypeLiteral(ty) => substitute_type(ty, arguments),
        Expr::Array(items) | Expr::Tuple(items) => {
            for item in items {
                substitute_expr(item, arguments);
            }
        }
        Expr::Record(fields) => {
            for (_, value) in fields {
                substitute_expr(value, arguments);
            }
        }
        Expr::SpecializedCall(_, types, call_args) => {
            for ty in types {
                substitute_type(ty, arguments);
            }
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments);
            }
        }
        Expr::BracketApply(receiver, call_args) => {
            substitute_expr(receiver, arguments);
            for argument in call_args {
                substitute_expr(argument, arguments);
            }
        }
        Expr::Apply(receiver, call_args) => {
            substitute_expr(receiver, arguments);
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments);
            }
        }
        Expr::Call(_, call_args) => {
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments);
            }
        }
        Expr::MethodCall(receiver, _, call_args) => {
            substitute_expr(receiver, arguments);
            for argument in call_args {
                substitute_expr(argument, arguments);
            }
        }
        Expr::Index(receiver, index) | Expr::Binary(_, receiver, index) => {
            substitute_expr(receiver, arguments);
            substitute_expr(index, arguments);
        }
        Expr::Interpolated(parts) => {
            for part in parts {
                if let StringPart::Expression(value) = part {
                    substitute_expr(value, arguments);
                }
            }
        }
        Expr::Fallback {
            value, fallback, ..
        } => {
            substitute_expr(value, arguments);
            substitute_expr(fallback, arguments);
        }
        Expr::Block(block) => substitute_block(block, arguments),
        Expr::Match { value, arms, .. } => {
            substitute_expr(value, arguments);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    substitute_expr(guard, arguments);
                }
                substitute_block(&mut arm.body, arguments);
            }
        }
        Expr::Try {
            body, catch_block, ..
        } => {
            substitute_block(body, arguments);
            substitute_block(catch_block, arguments);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            substitute_expr(cond, arguments);
            substitute_expr(then_branch, arguments);
            if let Some(else_branch) = else_branch {
                substitute_expr(else_branch, arguments);
            }
        }
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bytes(_)
        | Expr::Bool(_)
        | Expr::Nil
        | Expr::Ident(_) => {}
    }
}
