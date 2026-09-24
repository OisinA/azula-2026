//! Which variables closures use from their surroundings. Closures capture
//! variables by reference, so the typechecker keeps mutable variables that a
//! closure uses in heap cells.

use std::collections::{BTreeSet, HashSet};

use azula_ast::prelude::*;

#[derive(Default)]
struct Names {
    /// Identifiers read or written
    used: BTreeSet<String>,
    /// Names declared (variables, loop variables, pattern bindings, parameters)
    declared: HashSet<String>,
    /// Names used by closures that they don't declare themselves
    closure_free: HashSet<String>,
}

/// Names that `body` (of a closure with `params`) uses without declaring,
/// in a deterministic order
pub fn closure_free_names(params: &[String], body: &[Statement]) -> Vec<String> {
    let mut names = Names::default();
    for p in params {
        names.declared.insert(p.clone());
    }
    for s in body {
        walk_stmt(s, &mut names);
    }
    names.used.into_iter().filter(|n| !names.declared.contains(n)).collect()
}

/// Names used from outside by the closures in `body`: variables with these
/// names may be captured
pub fn captured_names(body: &[Statement]) -> HashSet<String> {
    let mut names = Names::default();
    for s in body {
        walk_stmt(s, &mut names);
    }
    names.closure_free
}

fn walk_body(body: &[Statement], names: &mut Names) {
    for s in body {
        walk_stmt(s, names);
    }
}

fn walk_stmt(stmt: &Statement, names: &mut Names) {
    match stmt {
        Statement::Root(body) | Statement::Block(body) | Statement::Group(body) => walk_body(body, names),
        Statement::Function { body, .. } => walk_stmt(body, names),
        Statement::Return(value, _) => {
            if let Some(v) = value {
                walk_expr(v, names);
            }
        }
        Statement::Assign(_, name, _, value, _) => {
            names.declared.insert(name.clone());
            walk_expr(value, names);
        }
        Statement::ExpressionStatement(e, _) => walk_expr(e, names),
        Statement::If(cond, body, else_branch, _) => {
            walk_expr(cond, names);
            walk_body(body, names);
            if let Some(e) = else_branch {
                walk_stmt(e, names);
            }
        }
        Statement::Reassign(target, value, _) | Statement::CompoundAssign(target, _, value, _) => {
            walk_expr(target, names);
            walk_expr(value, names);
        }
        Statement::While(cond, body, _) => {
            walk_expr(cond, names);
            walk_body(body, names);
        }
        Statement::For(cond, body, _) => {
            if let Some(c) = cond {
                walk_expr(c, names);
            }
            walk_body(body, names);
        }
        Statement::ForIn(name, iterable, end, _, body, _) => {
            names.declared.insert(name.clone());
            walk_expr(iterable, names);
            if let Some(e) = end {
                walk_expr(e, names);
            }
            walk_body(body, names);
        }
        Statement::Destructure(_, bound, value, _) => {
            for n in bound {
                names.declared.insert(n.clone());
            }
            walk_expr(value, names);
        }
        _ => {}
    }
}

fn declare_pattern(pattern: &MatchPattern, names: &mut Names) {
    match pattern {
        MatchPattern::Destructure(_, _, bindings) => {
            for b in bindings.iter().flatten() {
                names.declared.insert(b.to_string());
            }
        }
        MatchPattern::Binding(b) => {
            names.declared.insert(b.to_string());
        }
        MatchPattern::Tuple(items) => {
            for item in items {
                declare_pattern(item, names);
            }
        }
        _ => {}
    }
}

fn walk_expr(expr: &ExpressionNode, names: &mut Names) {
    match &expr.expression {
        Expression::Identifier(name) => {
            names.used.insert(name.clone());
        }
        Expression::Infix(l, _, r) | Expression::ArrayAccess(l, r) => {
            walk_expr(l, names);
            walk_expr(r, names);
        }
        Expression::FunctionCall { function, args } => {
            walk_expr(function, names);
            for a in args {
                walk_expr(a, names);
            }
        }
        Expression::Not(e)
        | Expression::BitNot(e)
        | Expression::Negate(e)
        | Expression::Pointer(e)
        | Expression::Deref(e)
        | Expression::Cast(e, _)
        | Expression::Alloc(e) => walk_expr(e, names),
        Expression::Array(items) | Expression::Tuple(items) | Expression::Interpolation(items) => {
            for i in items {
                walk_expr(i, names);
            }
        }
        Expression::StructInitialisation(_, fields) => {
            for (_, v) in fields {
                walk_expr(v, names);
            }
        }
        // The member is a field or method name, not a variable
        Expression::StructAccess(obj, _) => walk_expr(obj, names),
        Expression::Match(scrutinee, arms) => {
            walk_expr(scrutinee, names);
            for (pattern, body) in arms {
                declare_pattern(pattern, names);
                walk_expr(body, names);
            }
        }
        Expression::Block(stmts, last) => {
            walk_body(stmts, names);
            if let Some(l) = last {
                walk_expr(l, names);
            }
        }
        Expression::Closure(params, _, body) => {
            let param_names: Vec<String> = params.iter().map(|(_, n)| n.clone()).collect();
            let mut inner = Names::default();
            for p in &param_names {
                inner.declared.insert(p.clone());
            }
            walk_body(body, &mut inner);
            let free: Vec<String> = inner.used.iter().filter(|n| !inner.declared.contains(*n)).cloned().collect();
            for n in free {
                names.used.insert(n.clone());
                names.closure_free.insert(n);
            }
            names.closure_free.extend(inner.closure_free);
        }
        _ => {}
    }
}
