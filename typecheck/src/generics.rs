//! Helpers for monomorphising generic definitions: substituting type
//! parameters throughout an AST, and inferring type arguments by unification.

use std::{collections::HashMap, rc::Rc};

use azula_ast::prelude::*;
use azula_type::prelude::AzulaType;

pub type Substitution<'a> = HashMap<String, AzulaType<'a>>;

pub fn subst_type<'a>(typ: &AzulaType<'a>, map: &Substitution<'a>) -> AzulaType<'a> {
    match typ {
        AzulaType::Named(name) => map.get(name).cloned().unwrap_or_else(|| typ.clone()),
        AzulaType::Pointer(inner) => AzulaType::Pointer(Rc::new(subst_type(inner, map))),
        AzulaType::Array(inner, size) => AzulaType::Array(Rc::new(subst_type(inner, map)), *size),
        AzulaType::Generic(name, args) => {
            AzulaType::Generic(name.clone(), args.iter().map(|a| subst_type(a, map)).collect())
        }
        _ => typ.clone(),
    }
}

fn subst_body<'a>(body: &[Statement<'a>], map: &Substitution<'a>) -> Vec<Statement<'a>> {
    body.iter().map(|s| subst_stmt(s, map)).collect()
}

pub fn subst_stmt<'a>(stmt: &Statement<'a>, map: &Substitution<'a>) -> Statement<'a> {
    match stmt {
        Statement::Root(body) => Statement::Root(subst_body(body, map)),
        Statement::Block(body) => Statement::Block(subst_body(body, map)),
        Statement::Function {
            name,
            args,
            returns,
            body,
            span,
        } => Statement::Function {
            name,
            args: args.iter().map(|(t, n)| (subst_type(t, map), *n)).collect(),
            returns: subst_type(returns, map),
            body: Rc::new(subst_stmt(body, map)),
            span: span.clone(),
        },
        Statement::Return(value, span) => {
            Statement::Return(value.as_ref().map(|v| subst_expr(v, map)), span.clone())
        }
        Statement::Assign(mutable, name, annotation, value, span) => Statement::Assign(
            *mutable,
            name.clone(),
            annotation.as_ref().map(|t| subst_type(t, map)),
            subst_expr(value, map),
            span.clone(),
        ),
        Statement::ExpressionStatement(expr, span) => {
            Statement::ExpressionStatement(subst_expr(expr, map), span.clone())
        }
        Statement::If(cond, body, else_branch, span) => Statement::If(
            subst_expr(cond, map),
            subst_body(body, map),
            else_branch.as_ref().map(|e| Rc::new(subst_stmt(e, map))),
            span.clone(),
        ),
        Statement::Reassign(target, value, span) => {
            Statement::Reassign(subst_expr(target, map), subst_expr(value, map), span.clone())
        }
        Statement::While(cond, body, span) => {
            Statement::While(subst_expr(cond, map), subst_body(body, map), span.clone())
        }
        Statement::For(cond, body, span) => Statement::For(
            cond.as_ref().map(|c| subst_expr(c, map)),
            subst_body(body, map),
            span.clone(),
        ),
        Statement::ForIn(name, iterable, end, inclusive, body, span) => Statement::ForIn(
            name.clone(),
            subst_expr(iterable, map),
            end.as_ref().map(|e| subst_expr(e, map)),
            *inclusive,
            subst_body(body, map),
            span.clone(),
        ),
        Statement::CompoundAssign(target, op, value, span) => {
            Statement::CompoundAssign(subst_expr(target, map), op.clone(), subst_expr(value, map), span.clone())
        }
        _ => stmt.clone(),
    }
}

pub fn subst_expr<'a>(expr: &ExpressionNode<'a>, map: &Substitution<'a>) -> ExpressionNode<'a> {
    let sub = |e: &Rc<ExpressionNode<'a>>| Rc::new(subst_expr(e, map));
    let expression = match &expr.expression {
        Expression::Infix(l, op, r) => Expression::Infix(sub(l), op.clone(), sub(r)),
        Expression::FunctionCall { function, args } => Expression::FunctionCall {
            function: sub(function),
            args: args.iter().map(|a| subst_expr(a, map)).collect(),
        },
        Expression::Not(e) => Expression::Not(sub(e)),
        Expression::BitNot(e) => Expression::BitNot(sub(e)),
        Expression::SizeOf(t) => Expression::SizeOf(subst_type(t, map)),
        Expression::Negate(e) => Expression::Negate(sub(e)),
        Expression::Pointer(e) => Expression::Pointer(sub(e)),
        Expression::Deref(e) => Expression::Deref(sub(e)),
        Expression::Array(items) => Expression::Array(items.iter().map(|a| subst_expr(a, map)).collect()),
        Expression::Interpolation(parts) => {
            Expression::Interpolation(parts.iter().map(|a| subst_expr(a, map)).collect())
        }
        Expression::ArrayAccess(a, i) => Expression::ArrayAccess(sub(a), sub(i)),
        Expression::StructInitialisation(s, fields) => Expression::StructInitialisation(
            sub(s),
            fields.iter().map(|(n, e)| (*n, subst_expr(e, map))).collect(),
        ),
        Expression::StructAccess(s, m) => Expression::StructAccess(sub(s), sub(m)),
        Expression::NamespaceAccess(n, m) => Expression::NamespaceAccess(sub(n), sub(m)),
        Expression::Match(scrutinee, arms) => Expression::Match(
            sub(scrutinee),
            arms.iter().map(|(p, e)| (p.clone(), subst_expr(e, map))).collect(),
        ),
        Expression::Cast(e, t) => Expression::Cast(sub(e), subst_type(t, map)),
        Expression::Alloc(e) => Expression::Alloc(sub(e)),
        Expression::Turbofish(name, args) => {
            Expression::Turbofish(name.clone(), args.iter().map(|a| subst_type(a, map)).collect())
        }
        Expression::Block(stmts, last) => {
            Expression::Block(subst_body(stmts, map), last.as_ref().map(sub))
        }
        other => other.clone(),
    };
    ExpressionNode {
        expression,
        typed: subst_type(&expr.typed, map),
        span: expr.span.clone(),
    }
}

/// Infer type parameters by matching a (generic) `pattern` type against a
/// concrete type. `instances` maps instance names such as `Vec<int>` back to
/// the generic they came from and its arguments.
pub fn unify<'a>(
    pattern: &AzulaType<'a>,
    concrete: &AzulaType<'a>,
    params: &[String],
    instances: &HashMap<String, (String, Vec<AzulaType<'a>>)>,
    bindings: &mut Substitution<'a>,
) {
    match (pattern, concrete) {
        (AzulaType::Named(name), _) if params.contains(name) => {
            bindings.entry(name.clone()).or_insert_with(|| concrete.clone());
        }
        (AzulaType::Pointer(p), AzulaType::Pointer(c)) => unify(p, c, params, instances, bindings),
        (AzulaType::Array(p, _), AzulaType::Array(c, _)) => unify(p, c, params, instances, bindings),
        (AzulaType::Generic(name, pargs), AzulaType::Named(instance)) => {
            if let Some((generic, cargs)) = instances.get(instance) {
                if generic == name {
                    for (p, c) in pargs.iter().zip(cargs) {
                        unify(p, c, params, instances, bindings);
                    }
                }
            }
        }
        _ => {}
    }
}
