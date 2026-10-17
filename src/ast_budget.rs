//! Iterative accounting before cloning or analyzing generated syntax.
use crate::ast::*;

pub(crate) const MAX_COST: usize = 64 * 1024 * 1024;

enum Node<'a> {
    Item(&'a Item),
    Function(&'a FunctionDecl),
    Field(&'a StructField),
    Block(&'a Block),
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    Type(&'a TypeAnnotation, usize),
}

pub(crate) fn program(program: &Program) -> Result<usize, String> {
    measure(program.items.iter().map(Node::Item).collect())
}

pub(crate) fn item(item: &Item) -> Result<usize, String> {
    measure(vec![Node::Item(item)])
}
pub(crate) fn function(function: &FunctionDecl) -> Result<usize, String> {
    measure(vec![Node::Function(function)])
}
pub(crate) fn fields(fields: &[StructField]) -> Result<usize, String> {
    measure(fields.iter().map(Node::Field).collect())
}
pub(crate) fn annotation(ty: &TypeAnnotation) -> Result<usize, String> {
    measure(vec![Node::Type(ty, 0)])
}

fn measure(mut pending: Vec<Node<'_>>) -> Result<usize, String> {
    let mut cost = 0usize;
    while let Some(node) = pending.pop() {
        cost = cost.saturating_add(256);
        match node {
            Node::Item(item) => match item {
                Item::Function(function) => pending.push(Node::Function(function)),
                Item::GlobalConst { ty, value, .. }
                | Item::GlobalVar { ty, value, .. }
                | Item::Comptime { ty, value, .. } => {
                    pending.push(Node::Expr(value));
                    if let Some(ty) = ty {
                        pending.push(Node::Type(ty, 0));
                    }
                }
                Item::StructDecl {
                    name,
                    owner,
                    fields,
                    ..
                } => {
                    cost = cost.saturating_add(name.len());
                    if let Some(owner) = owner {
                        cost = cost.saturating_add(owner.len());
                    }
                    pending.extend(fields.iter().map(Node::Field));
                }
                Item::EnumDecl { variants, .. } => {
                    for variant in variants {
                        pending.extend(variant.payload.iter().map(Node::Field));
                    }
                }
                Item::Extend(extension) => {
                    pending.push(Node::Type(&extension.target, 0));
                    pending.extend(extension.trait_arguments.iter().map(|ty| Node::Type(ty, 0)));
                    pending.extend(
                        extension
                            .methods
                            .iter()
                            .map(|m| Node::Function(&m.function)),
                    );
                }
                Item::Trait(declaration) => {
                    for method in &declaration.methods {
                        pending.extend(method.params.iter().map(Node::Field));
                        if let Some(ty) = &method.result_type {
                            pending.push(Node::Type(ty, 0));
                        }
                    }
                }
                _ => {}
            },
            Node::Function(function) => {
                cost = cost.saturating_add(function.name.len());
                if let Some(owner) = &function.owner {
                    cost = cost.saturating_add(owner.len());
                }
                pending.push(Node::Block(&function.body));
                pending.extend(function.params.iter().map(Node::Field));
                if let Some(ty) = &function.result_type {
                    pending.push(Node::Type(ty, 0));
                }
                for parameter in &function.type_params {
                    for bound in &parameter.bounds {
                        cost = cost.saturating_add(bound.name.len());
                        pending.extend(bound.arguments.iter().map(|ty| Node::Type(ty, 0)));
                    }
                }
            }
            Node::Field(field) => {
                cost = cost.saturating_add(field.name.len());
                if let Some(ty) = &field.ty {
                    pending.push(Node::Type(ty, 0));
                }
                if let Some(value) = &field.default {
                    pending.push(Node::Expr(value));
                }
            }
            Node::Block(block) => {
                pending.extend(block.stmts.iter().map(Node::Stmt));
                if let Some(tail) = &block.tail {
                    pending.push(Node::Expr(tail));
                }
            }
            Node::Stmt(stmt) => match stmt {
                Stmt::Located(_, inner) => pending.push(Node::Stmt(inner)),
                Stmt::VarDecl { name, ty, value }
                | Stmt::Const { name, ty, value }
                | Stmt::Comptime { name, ty, value } => {
                    cost = cost.saturating_add(name.len());
                    if let Some(ty) = ty {
                        pending.push(Node::Type(ty, 0));
                    }
                    pending.push(Node::Expr(value));
                }
                Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
                    pending.push(Node::Expr(target));
                    pending.push(Node::Expr(value));
                }
                Stmt::Return(Some(value)) | Stmt::Expr(value) | Stmt::Crash { value, .. } => {
                    pending.push(Node::Expr(value))
                }
                Stmt::Defer { body } | Stmt::While { body, .. } => {
                    pending.push(Node::Block(body));
                    if let Stmt::While { cond, .. } = stmt {
                        pending.push(Node::Expr(cond));
                    }
                }
                Stmt::Try {
                    body, catch_block, ..
                } => {
                    pending.push(Node::Block(body));
                    pending.push(Node::Block(catch_block));
                }
                Stmt::If {
                    cond: value,
                    then_block,
                    else_block,
                }
                | Stmt::When {
                    value,
                    then_block,
                    else_block,
                    ..
                } => {
                    pending.push(Node::Expr(value));
                    pending.push(Node::Block(then_block));
                    if let Some(block) = else_block {
                        pending.push(Node::Block(block));
                    }
                }
                Stmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    pending.push(Node::Expr(start));
                    pending.push(Node::Expr(end));
                    pending.push(Node::Block(body));
                    if let Some(step) = step {
                        pending.push(Node::Expr(step));
                    }
                }
                Stmt::ForEach {
                    var,
                    iterable,
                    body,
                } => {
                    cost = cost.saturating_add(var.len());
                    pending.push(Node::Expr(iterable));
                    pending.push(Node::Block(body));
                }
                _ => {}
            },
            Node::Expr(expr) => match expr {
                Expr::Str(text) | Expr::Ident(text) => cost = cost.saturating_add(text.len()),
                Expr::Bytes(bytes) => cost = cost.saturating_add(bytes.len()),
                Expr::TypeLiteral(ty) => pending.push(Node::Type(ty, 0)),
                Expr::Located(_, inner)
                | Expr::Unary(_, inner)
                | Expr::Propagate(inner)
                | Expr::Cast(inner, _) => pending.push(Node::Expr(inner)),
                Expr::Index(a, b)
                | Expr::Binary(_, a, b)
                | Expr::Fallback {
                    value: a,
                    fallback: b,
                    ..
                } => {
                    pending.push(Node::Expr(a));
                    pending.push(Node::Expr(b));
                }
                Expr::Array(items) | Expr::Tuple(items) => {
                    pending.extend(items.iter().map(Node::Expr))
                }
                Expr::Record(fields) => {
                    for (name, value) in fields {
                        cost = cost.saturating_add(name.len());
                        pending.push(Node::Expr(value));
                    }
                }
                Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                    cost = cost.saturating_add(name.len());
                    pending.extend(args.iter().map(|a| Node::Expr(&a.value)));
                    if let Expr::SpecializedCall(_, types, _) = expr {
                        pending.extend(types.iter().map(|ty| Node::Type(ty, 0)));
                    }
                }
                Expr::BracketApply(receiver, args) | Expr::MethodCall(receiver, _, args) => {
                    pending.push(Node::Expr(receiver));
                    pending.extend(args.iter().map(Node::Expr));
                }
                Expr::Apply(receiver, args) => {
                    pending.push(Node::Expr(receiver));
                    pending.extend(args.iter().map(|a| Node::Expr(&a.value)));
                }
                Expr::Interpolated(parts) => {
                    for part in parts {
                        match part {
                            StringPart::Text(text) => cost = cost.saturating_add(text.len()),
                            StringPart::Expression(value) => pending.push(Node::Expr(value)),
                        }
                    }
                }
                Expr::Block(block) => pending.push(Node::Block(block)),
                Expr::Try {
                    body, catch_block, ..
                } => {
                    pending.push(Node::Block(body));
                    pending.push(Node::Block(catch_block));
                }
                Expr::Match { value, arms, .. } => {
                    pending.push(Node::Expr(value));
                    for arm in arms {
                        if let MatchPattern::StringLiteral(pattern) = &arm.pattern {
                            cost = cost.saturating_add(pattern.len());
                        }
                        pending.push(Node::Block(&arm.body));
                        if let Some(guard) = &arm.guard {
                            pending.push(Node::Expr(guard));
                        }
                    }
                }
                Expr::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    pending.push(Node::Expr(cond));
                    pending.push(Node::Expr(then_branch));
                    if let Some(other) = else_branch {
                        pending.push(Node::Expr(other));
                    }
                }
                _ => {}
            },
            Node::Type(ty, depth) => {
                if depth >= 64 {
                    return Err("compiler type depth limit exceeded".into());
                }
                match &ty.kind {
                    TypeKind::Named(name) | TypeKind::TypeParameter(name) => {
                        cost = cost.saturating_add(name.len())
                    }
                    TypeKind::ResolvedName { source, symbol } => {
                        cost = cost.saturating_add(source.len() + symbol.len())
                    }
                    TypeKind::Array(inner) => pending.push(Node::Type(inner, depth + 1)),
                    TypeKind::Tuple(items) => {
                        pending.extend(items.iter().map(|ty| Node::Type(ty, depth + 1)))
                    }
                    TypeKind::Applied { source, arguments } => {
                        cost = cost.saturating_add(source.len());
                        pending.extend(arguments.iter().map(|ty| Node::Type(ty, depth + 1)));
                    }
                    TypeKind::ResolvedApplied {
                        source,
                        symbol,
                        arguments,
                    } => {
                        cost = cost.saturating_add(source.len() + symbol.len());
                        pending.extend(arguments.iter().map(|ty| Node::Type(ty, depth + 1)));
                    }
                    _ => {}
                }
            }
        }
        if cost > MAX_COST {
            return Err("compiler syntax allocation budget (64 MiB) exceeded".into());
        }
    }
    Ok(cost)
}
