//! Small, conservative source warnings that never affect compilation success.
use crate::ast::{
    BinOp, Block, Expr, FunctionDecl, Item, MatchPattern, Program, Stmt, StringPart, TypeKind,
    UnOp, VarType,
};
use crate::diagnostic::{Diagnostic, SourceSpan};
use std::collections::HashMap;

#[derive(Default)]
struct Local {
    read: bool,
    report_unused: bool,
    boolean: bool,
    span: Option<SourceSpan>,
}

#[derive(Default)]
struct Linter {
    scopes: Vec<HashMap<String, Local>>,
    warnings: Vec<Diagnostic>,
}

pub(crate) fn check(program: &Program) -> Vec<Diagnostic> {
    let mut linter = Linter::default();
    for item in &program.items {
        match item {
            Item::Function(function) => linter.function(function),
            Item::Extend(extension) => {
                for method in &extension.methods {
                    linter.function(&method.function);
                }
            }
            Item::GlobalConst { value, .. }
            | Item::GlobalVar { value, .. }
            | Item::Comptime { value, .. } => linter.expr(value, true),
            Item::StructDecl { fields, .. } => {
                for field in fields {
                    if let Some(default) = &field.default {
                        linter.expr(default, true);
                    }
                }
            }
            Item::Invalid { .. }
            | Item::Use(_)
            | Item::ComptimeInclude { .. }
            | Item::Trait(_)
            | Item::EnumDecl { .. } => {}
        }
    }
    linter.warnings.sort_by_key(diagnostic_position);
    linter.warnings
}

fn diagnostic_position(diagnostic: &Diagnostic) -> (String, usize) {
    diagnostic.span.as_ref().map_or_else(
        || (String::new(), 0),
        |span| (span.source.name.clone(), span.range.start),
    )
}

impl Linter {
    fn function(&mut self, function: &FunctionDecl) {
        self.scopes.push(HashMap::new());
        for parameter in &function.params {
            let boolean = parameter
                .ty
                .as_ref()
                .is_some_and(|ty| matches!(ty.kind, TypeKind::Primitive(VarType::Bool)));
            self.define_with_type(&parameter.name, false, None, boolean);
        }
        self.block(&function.body, true);
        self.pop_scope();
    }

    fn block(&mut self, block: &Block, report: bool) {
        self.scopes.push(HashMap::new());
        let mut reachable = report;
        let mut reported_unreachable = false;
        for statement in &block.stmts {
            if !reachable && report && !reported_unreachable {
                if let Some(span) = statement.span() {
                    self.warnings.push(Diagnostic::warning(
                        "W0002",
                        "statement is unreachable",
                        Some(span.clone()),
                    ));
                }
                reported_unreachable = true;
            }
            let statement_is_reachable = reachable;
            let terminates = self.stmt(statement, statement_is_reachable);
            if reachable && terminates {
                reachable = false;
            }
        }
        if reachable && let Some(tail) = &block.tail {
            self.expr(tail, report);
        }
        self.pop_scope();
    }

    fn stmt(&mut self, statement: &Stmt, report: bool) -> bool {
        match statement.node() {
            Stmt::Located(_, _) => unreachable!("node removes source locations"),
            Stmt::Invalid { .. } | Stmt::Use { .. } => {}
            Stmt::VarDecl { name, ty, value }
            | Stmt::Const { name, ty, value }
            | Stmt::Comptime { name, ty, value } => {
                self.expr(value, report);
                let boolean = ty
                    .as_ref()
                    .is_some_and(|ty| matches!(ty.kind, TypeKind::Primitive(VarType::Bool)))
                    || self.expr_is_boolean(value);
                self.define_with_type(name, report, statement.span(), boolean);
            }
            Stmt::Assign { target, value } => {
                self.lvalue(target, report);
                self.expr(value, report);
            }
            Stmt::CompoundAssign { target, value, .. } => {
                self.expr(target, report);
                self.expr(value, report);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, report);
                }
            }
            Stmt::Defer { body } => self.block(body, report),
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, report);
                self.scopes.push(HashMap::new());
                self.define(catch_name, report, statement.span());
                self.block(catch_block, report);
                self.pop_scope();
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                self.expr(cond, report);
                if report && let Expr::Bool(value) = cond.node() {
                    self.warnings.push(Diagnostic::warning(
                        "W0003",
                        if *value {
                            "condition is always true; this `if` can be simplified to its `then` branch"
                        } else if else_block.is_some() {
                            "condition is always false; this `if` can be simplified to its `else` branch"
                        } else {
                            "condition is always false; this `if` body can be removed"
                        },
                        statement.span().cloned(),
                    ));
                }
                let then_reachable = report && !matches!(cond.node(), Expr::Bool(false));
                let else_reachable = report && !matches!(cond.node(), Expr::Bool(true));
                self.block(then_block, then_reachable);
                if let Some(block) = else_block {
                    self.block(block, else_reachable);
                }
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                self.expr(value, report);
                self.scopes.push(HashMap::new());
                self.define(name, report, statement.span());
                self.block(then_block, report);
                self.pop_scope();
                if let Some(block) = else_block {
                    self.block(block, report);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond, report);
                self.block(body, report);
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.expr(start, report);
                self.expr(end, report);
                if let Some(step) = step {
                    self.expr(step, report);
                }
                self.scopes.push(HashMap::new());
                self.define(var, report, statement.span());
                self.block(body, report);
                self.pop_scope();
            }
            Stmt::ForEach {
                var,
                iterable,
                body,
            } => {
                self.expr(iterable, report);
                self.scopes.push(HashMap::new());
                self.define(var, report, statement.span());
                self.block(body, report);
                self.pop_scope();
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Crash { value, .. } => self.expr(value, report),
            Stmt::Expr(expr) => self.expr(expr, report),
        }
        self.terminates(statement)
    }

    fn expr(&mut self, expression: &Expr, report: bool) {
        match expression.node() {
            Expr::Located(_, _) => unreachable!("node removes source locations"),
            Expr::Ident(name) => {
                if report {
                    for scope in self.scopes.iter_mut().rev() {
                        if let Some(local) = scope.get_mut(name) {
                            local.read = true;
                            break;
                        }
                    }
                }
            }
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let StringPart::Expression(expression) = part {
                        self.expr(expression, report);
                    }
                }
            }
            Expr::Array(items) | Expr::Tuple(items) => {
                for item in items {
                    self.expr(item, report);
                }
            }
            Expr::Record(fields) => {
                for (_, value) in fields {
                    self.expr(value, report);
                }
            }
            Expr::Call(_, args) | Expr::Apply(_, args) => {
                if let Expr::Apply(callee, _) = expression.node() {
                    self.expr(callee, report);
                }
                for argument in args {
                    self.expr(&argument.value, report);
                }
            }
            Expr::BracketApply(target, args) => {
                self.expr(target, report);
                for argument in args {
                    self.expr(argument, report);
                }
            }
            Expr::SpecializedCall(_, _, args) => {
                for argument in args {
                    self.expr(&argument.value, report);
                }
            }
            Expr::MethodCall(receiver, _, args) => {
                self.expr(receiver, report);
                for argument in args {
                    self.expr(argument, report);
                }
            }
            Expr::Index(target, index) | Expr::Binary(_, target, index) => {
                self.expr(target, report);
                self.expr(index, report);
            }
            Expr::Unary(_, value) | Expr::Propagate(value) | Expr::Cast(value, _) => {
                self.expr(value, report)
            }
            Expr::Fallback {
                value, fallback, ..
            } => {
                self.expr(value, report);
                self.expr(fallback, report);
            }
            Expr::Block(block) => self.block(block, report),
            Expr::Match { value, arms, .. } => {
                self.expr(value, report);
                for arm in arms {
                    self.scopes.push(HashMap::new());
                    if let MatchPattern::Variant { bindings, .. } = &arm.pattern {
                        for binding in bindings {
                            self.define(
                                binding,
                                report,
                                arm.body.stmts.first().and_then(Stmt::span),
                            );
                        }
                    }
                    if let Some(guard) = &arm.guard {
                        self.expr(guard, report);
                    }
                    self.block(&arm.body, report);
                    self.pop_scope();
                }
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, report);
                self.scopes.push(HashMap::new());
                self.define(catch_name, report, expression.span());
                self.block(catch_block, report);
                self.pop_scope();
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, report);
                if report && let Expr::Bool(value) = cond.node() {
                    self.warnings.push(Diagnostic::warning(
                        "W0003",
                        if *value {
                            "condition is always true; this conditional can be simplified to its `then` branch"
                        } else if else_branch.is_some() {
                            "condition is always false; this conditional can be simplified to its `else` branch"
                        } else {
                            "condition is always false; this conditional body can be removed"
                        },
                        expression.span().cloned(),
                    ));
                } else if report
                    && self.expr_is_boolean(cond)
                    && branch_bool(then_branch) == Some(true)
                    && else_branch.as_deref().and_then(branch_bool) == Some(false)
                {
                    self.warnings.push(Diagnostic::warning(
                        "W0004",
                        "this conditional returns its boolean condition; use the condition directly",
                        expression.span().cloned(),
                    ));
                }
                let then_reachable = report && !matches!(cond.node(), Expr::Bool(false));
                let else_reachable = report && !matches!(cond.node(), Expr::Bool(true));
                self.expr(then_branch, then_reachable);
                if let Some(branch) = else_branch {
                    self.expr(branch, else_reachable);
                }
            }
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Bytes(_)
            | Expr::Bool(_)
            | Expr::Nil
            | Expr::TypeLiteral(_) => {}
        }
    }

    fn lvalue(&mut self, target: &Expr, report: bool) {
        match target.node() {
            Expr::Ident(_) => {}
            Expr::Index(receiver, index) => {
                self.expr(receiver, report);
                self.expr(index, report);
            }
            _ => self.expr(target, report),
        }
    }

    fn define(&mut self, name: &str, report_unused: bool, span: Option<&SourceSpan>) {
        self.define_with_type(name, report_unused, span, false);
    }

    fn define_with_type(
        &mut self,
        name: &str,
        report_unused: bool,
        span: Option<&SourceSpan>,
        boolean: bool,
    ) {
        if name.starts_with('_') {
            return;
        }
        self.scopes
            .last_mut()
            .expect("a function or block scope is active")
            .insert(
                name.to_owned(),
                Local {
                    read: false,
                    report_unused,
                    boolean,
                    span: span.cloned(),
                },
            );
    }

    fn pop_scope(&mut self) {
        let Some(scope) = self.scopes.pop() else {
            return;
        };
        for (name, local) in scope {
            if local.report_unused && !local.read {
                self.warnings.push(
                    Diagnostic::warning(
                        "W0001",
                        format!("local binding `{name}` is never read"),
                        local.span,
                    )
                    .note("prefix the name with `_` if the unused binding is intentional"),
                );
            }
        }
    }

    fn terminates(&self, statement: &Stmt) -> bool {
        match statement.node() {
            Stmt::Return(_) | Stmt::Break | Stmt::Continue | Stmt::Crash { .. } => true,
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => match cond.node() {
                Expr::Bool(true) => self.block_terminates(then_block),
                Expr::Bool(false) => else_block
                    .as_ref()
                    .is_some_and(|block| self.block_terminates(block)),
                _ => {
                    self.block_terminates(then_block)
                        && else_block
                            .as_ref()
                            .is_some_and(|block| self.block_terminates(block))
                }
            },
            _ => false,
        }
    }

    fn block_terminates(&self, block: &Block) -> bool {
        for statement in &block.stmts {
            if self.terminates(statement) {
                return true;
            }
        }
        false
    }

    fn expr_is_boolean(&self, expression: &Expr) -> bool {
        match expression.node() {
            Expr::Bool(_) => true,
            Expr::Ident(name) => self
                .scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
                .is_some_and(|local| local.boolean),
            Expr::Binary(
                BinOp::Eq
                | BinOp::NotEq
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
                | BinOp::And
                | BinOp::Or
                | BinOp::Xor,
                _,
                _,
            )
            | Expr::Unary(UnOp::Not, _)
            | Expr::Cast(_, VarType::Bool) => true,
            Expr::If {
                then_branch,
                else_branch: Some(else_branch),
                ..
            } => branch_bool(then_branch).is_some() && branch_bool(else_branch).is_some(),
            _ => false,
        }
    }
}

fn branch_bool(expression: &Expr) -> Option<bool> {
    match expression.node() {
        Expr::Bool(value) => Some(*value),
        Expr::Block(block) if block.stmts.is_empty() => block.tail.as_deref().and_then(branch_bool),
        _ => None,
    }
}
