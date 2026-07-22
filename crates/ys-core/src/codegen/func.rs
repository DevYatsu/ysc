//! Function, closure, and async function code generation.
//!
//! These are free functions that take `&mut Codegen` so they can be defined in
//! a child module without creating a circular dependency.

use super::{Codegen, VarInfo};
use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;
use rustc_hash::FxHashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Saved state from [`begin_function`] — must be passed to [`end_function`].
struct FuncFrame {
    locals: FxHashMap<String, VarInfo>,
    next_reg: usize,
    is_in_function: bool,
    var_mask: u64,
    freed_regs: Vec<usize>,
    saved_instrs: Vec<Instruction>,
}

/// Save the parent context and set up registers for function compilation.
/// Must be paired with [`end_function`].
fn begin_function(cg: &mut Codegen, params: &[String]) -> FuncFrame {
    let frame = FuncFrame {
        locals: std::mem::take(&mut cg.locals),
        next_reg: cg.next_reg,
        is_in_function: cg.is_in_function,
        var_mask: cg.var_mask,
        freed_regs: std::mem::take(&mut cg.freed_regs),
        saved_instrs: std::mem::take(&mut cg.instructions),
    };
    // Clear per-function state — the outer scope's register space must
    // not leak into the function's own allocation.
    cg.freed_regs.clear();
    cg.var_mask = 0;
    cg.is_in_function = true;
    cg.next_reg = 0;
    for (i, p) in params.iter().enumerate() {
        cg.locals.insert(
            p.clone(),
            VarInfo {
                idx: i,
                is_global: false,
            },
        );
        cg.var_mask |= 1 << i;
        cg.next_reg = i + 1;
    }
    frame
}

/// Ensure the compiled body ends with a Return, then push the function into
/// the program's function list and restore the parent compilation context.
fn end_function(
    cg: &mut Codegen,
    name: &str,
    params_count: usize,
    frame: FuncFrame,
    loc: Loc,
    rest_at: Option<usize>,
    kwargs_at: Option<usize>,
) {
    if !matches!(cg.instructions.last(), Some(Instruction::Return { .. })) {
        cg.emit(Instruction::Return { value: None, loc });
    }
    let name_id = cg.intern(name);
    let func_body = std::mem::replace(&mut cg.instructions, frame.saved_instrs);
    let locals_count = cg.next_reg;

    cg.locals = frame.locals;
    cg.next_reg = frame.next_reg;
    cg.is_in_function = frame.is_in_function;
    cg.var_mask = frame.var_mask;
    cg.freed_regs = frame.freed_regs;

    let idx = cg.functions.len();
    cg.functions.push(UserFunction {
        name_id,
        params_count,
        locals_count,
        instructions: Arc::from(func_body),
        rest_at,
        kwargs_at,
    });
    cg.function_map.insert(name.to_string(), idx);
}

// ---------------------------------------------------------------------------
// Function declaration
// ---------------------------------------------------------------------------

pub(super) fn compile_func(
    cg: &mut Codegen,
    name: &str,
    params: &[String],
    body: &[AstNode],
    loc: Loc,
    rest_at: Option<usize>,
    kwargs_at: Option<usize>,
) {
    let frame = begin_function(cg, params);
    if cg.compile_block(body).is_err() {
        cg.emit(Instruction::Return { value: None, loc });
    }
    end_function(cg, name, params.len(), frame, loc, rest_at, kwargs_at);
}

// ---------------------------------------------------------------------------
// Async function
// ---------------------------------------------------------------------------

/// Compile an async function — creates a pending return promise at the
/// start so callers immediately get a Promise, even if the body suspends
/// on an internal await.  The promise is resolved when the body returns.
pub(super) fn compile_async_func(
    cg: &mut Codegen,
    name: &str,
    params: &[String],
    body: &[AstNode],
    loc: Loc,
) {
    let frame = begin_function(cg, params);
    let ret_promise_reg = cg.alloc_reg();
    cg.emit(Instruction::MakePendingPromise {
        dst: ret_promise_reg,
    });

    if cg.compile_block(body).is_err() {
        cg.emit(Instruction::Return { value: None, loc });
    }
    // Replace the final return with ResolvePromise + Return(ret_promise)
    if let Some(Instruction::Return { value: reg, .. }) = cg.instructions.pop() {
        if let Some(value_reg) = reg {
            cg.emit(Instruction::ResolvePromise {
                promise: ret_promise_reg,
                value: value_reg,
            });
        } else {
            // No return value — resolve with nil
            let nil_reg = cg.alloc_reg();
            cg.emit(Instruction::LoadLiteral {
                dst: nil_reg,
                val: Value::nil(),
                loc,
            });
            cg.emit(Instruction::ResolvePromise {
                promise: ret_promise_reg,
                value: nil_reg,
            });
        }
        cg.emit(Instruction::Return {
            value: Some(ret_promise_reg),
            loc,
        });
    } else {
        // No return instruction at all — body ran to end without returning
        let nil_reg = cg.alloc_reg();
        cg.emit(Instruction::LoadLiteral {
            dst: nil_reg,
            val: Value::nil(),
            loc,
        });
        cg.emit(Instruction::ResolvePromise {
            promise: ret_promise_reg,
            value: nil_reg,
        });
        cg.emit(Instruction::Return {
            value: Some(ret_promise_reg),
            loc,
        });
    }

    end_function(cg, name, params.len(), frame, loc, None, None);
}

// ---------------------------------------------------------------------------
// Closure
// ---------------------------------------------------------------------------

/// Walk the closure body AST to find free variable references — names that
/// reference variables from the enclosing scope rather than the closure's own
/// parameters or locals.  The parent [`Codegen`] determines whether each name
/// is a local (→ capture) or a global (→ skip, accessible via `LoadGlobal`).
fn find_captures(body: &AstNode, closure_params: &[String], parent: &Codegen) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let bound: Vec<String> = closure_params.iter().cloned().collect();
    walk_for_free_vars(body, &bound, &mut found);
    // Filter: keep only names that are LOCALS in the parent scope.
    found.retain(|name| {
        parent
            .get_var(name)
            .is_some_and(|v| !v.is_global)
    });
    found
}

fn walk_for_free_vars(node: &AstNode, bound: &[String], found: &mut Vec<String>) {
    match node {
        // -- Leaf nodes that may reference a variable ------------------------
        AstNode::Ident(name, _) => {
            if !bound.contains(name) && !found.contains(name) {
                found.push(name.clone());
            }
        }
        AstNode::FunCall { name, args, named, .. } => {
            // The function name might be a captured variable (e.g. `fn(x)`).
            if !bound.contains(name) && !found.contains(name) {
                found.push(name.clone());
            }
            for a in args {
                walk_for_free_vars(a, bound, found);
            }
            for (_n, v) in named {
                walk_for_free_vars(v, bound, found);
            }
        }
        AstNode::DynamicCall { callee, args, named, .. } => {
            walk_for_free_vars(callee, bound, found);
            for a in args {
                walk_for_free_vars(a, bound, found);
            }
            for (_n, v) in named {
                walk_for_free_vars(v, bound, found);
            }
        }

        // -- Variable-binding constructs (add to bound set then recurse) -----
        AstNode::For { var, iter, body, .. } => {
            walk_for_free_vars(iter, bound, found);
            let mut b = bound.to_vec();
            b.push(var.clone());
            for stmt in body {
                walk_for_free_vars(stmt, &b, found);
            }
        }
        AstNode::FunDecl { name, params, body, .. } => {
            let mut b = bound.to_vec();
            b.push(name.clone());
            for p in params {
                b.push(p.name.clone());
            }
            for stmt in body {
                walk_for_free_vars(stmt, &b, found);
            }
        }
        AstNode::Closure { params, body, .. } => {
            let mut b: Vec<String> = bound.to_vec();
            for p in params {
                b.push(p.name.clone());
            }
            walk_for_free_vars(body, &b, found);
        }

        // -- Assignments: target is being defined, value is read -------------
        AstNode::Assign { target, value, .. } => {
            // The value may reference free variables.
            walk_for_free_vars(value, bound, found);
            // The target (if an Ident) is being defined — add to bound set.
            if let AstNode::Ident(name, _) = target.as_ref() {
                let mut b = bound.to_vec();
                b.push(name.clone());
                // Also walk the value with the new binding if needed
                // (already done above — value was walked with old bound set).
            } else {
                // For computed targets (index/field), walk them too.
                walk_for_free_vars(target, bound, found);
            }
        }

        // -- Composite expressions (recurse into children) -------------------
        AstNode::Binary { lhs, rhs, .. } => {
            walk_for_free_vars(lhs, bound, found);
            walk_for_free_vars(rhs, bound, found);
        }
        AstNode::Unary { expr, .. } => walk_for_free_vars(expr, bound, found),
        AstNode::Index { obj, index, .. } => {
            walk_for_free_vars(obj, bound, found);
            walk_for_free_vars(index, bound, found);
        }
        AstNode::Field { obj, .. } => walk_for_free_vars(obj, bound, found),
        AstNode::Return { value: Some(v), .. } => walk_for_free_vars(v, bound, found),
        AstNode::Return { value: None, .. } => {}
        AstNode::Yield(expr, _) => walk_for_free_vars(expr, bound, found),
        AstNode::Await(expr, _) => walk_for_free_vars(expr, bound, found),
        AstNode::Splat(expr, _) => walk_for_free_vars(expr, bound, found),
        AstNode::Range { start, end, step, .. } => {
            walk_for_free_vars(start, bound, found);
            walk_for_free_vars(end, bound, found);
            if let Some(s) = step {
                walk_for_free_vars(s, bound, found);
            }
        }
        AstNode::ListLit(elems, _) => {
            for e in elems {
                walk_for_free_vars(e, bound, found);
            }
        }
        AstNode::ListRepeat { val, count, .. } => {
            walk_for_free_vars(val, bound, found);
            walk_for_free_vars(count, bound, found);
        }
        AstNode::ObjectLit(fields, _) => {
            for (_, v) in fields {
                walk_for_free_vars(v, bound, found);
            }
        }
        AstNode::If { cond, then_block, else_block, .. } => {
            walk_for_free_vars(cond, bound, found);
            for stmt in then_block {
                walk_for_free_vars(stmt, bound, found);
            }
            for stmt in else_block {
                walk_for_free_vars(stmt, bound, found);
            }
        }
        AstNode::While { cond, body, .. } => {
            walk_for_free_vars(cond, bound, found);
            for stmt in body {
                walk_for_free_vars(stmt, bound, found);
            }
        }
        AstNode::Switch { expr, arms, .. } => {
            walk_for_free_vars(expr, bound, found);
            for arm in arms {
                for pat in &arm.patterns {
                    walk_for_free_vars(pat, bound, found);
                }
                for stmt in &arm.body {
                    walk_for_free_vars(stmt, bound, found);
                }
            }
        }
        AstNode::Fallback { expr, default, .. } => {
            walk_for_free_vars(expr, bound, found);
            walk_for_free_vars(default, bound, found);
        }
        AstNode::Except { expr, arms, .. } => {
            walk_for_free_vars(expr, bound, found);
            for arm in arms {
                for stmt in &arm.body {
                    walk_for_free_vars(stmt, bound, found);
                }
            }
        }
        AstNode::Block(stmts, _) => {
            for stmt in stmts {
                walk_for_free_vars(stmt, bound, found);
            }
        }
        AstNode::Decorator { name: _, args, inner, .. } => {
            for a in args {
                walk_for_free_vars(a, bound, found);
            }
            walk_for_free_vars(inner, bound, found);
        }

        // -- Literals and other leaf nodes (no variable references) ----------
        AstNode::Number(..) | AstNode::Bool(..) | AstNode::Nil(..) | AstNode::Str(..)
        | AstNode::Break(..) | AstNode::Fail { .. }
        | AstNode::ErrorDecl { .. } | AstNode::ErrorEnum { .. } | AstNode::Use { .. }
        | AstNode::AsyncFun { .. } | AstNode::Template { .. } => {}
    }
}

pub(super) fn compile_closure(
    cg: &mut Codegen,
    params: &[String],
    body: &AstNode,
    loc: Loc,
) -> Result<usize, JitError> {
    // 1. Detect captures: free variables in the body that are locals in the parent scope.
    let capture_names: Vec<String> = find_captures(body, params, cg);

    // 2. Build the closure's register layout: captures first, then params.
    let capture_base = 0usize;
    let params_base = capture_base + capture_names.len();

    let mut func = Codegen::new();
    func.closure_counter = cg.closure_counter;
    func.is_in_function = true;

    // Register captures as locals (indices 0..captures.len()).
    for (i, name) in capture_names.iter().enumerate() {
        func.locals.insert(
            name.clone(),
            VarInfo { idx: i, is_global: false },
        );
        func.var_mask |= 1 << i;
        func.next_reg = i + 1;
    }
    // Register params after captures.
    for (i, p) in params.iter().enumerate() {
        let idx = params_base + i;
        func.locals.insert(
            p.clone(),
            VarInfo {
                idx,
                is_global: false,
            },
        );
        func.var_mask |= 1 << idx;
        func.next_reg = idx + 1;
    }

    // Copy parent scope metadata so decorated / global / named-arg resolution
    // works inside the closure body.
    func.globals = cg.globals.clone();
    func.fn_params = cg.fn_params.clone();
    func.decorated_fns = cg.decorated_fns.clone();

    let result_reg = func.compile_node(body).unwrap_or(0);

    for mut nested in std::mem::take(&mut func.functions) {
        let new_name = format!("__closure_{}", cg.closure_counter);
        cg.closure_counter += 1;
        nested.name_id = cg.intern(&new_name);
        cg.functions.push(nested);
    }
    cg.closure_counter = std::cmp::max(cg.closure_counter, func.closure_counter);

    for instr in &mut func.instructions {
        match instr {
            Instruction::MakeClosure { name_id, .. }
            | Instruction::ObjectGet { name_id, .. }
            | Instruction::ObjectSet { name_id, .. } => {
                if let Some(name) = func.string_pool.get(*name_id as usize) {
                    *name_id = cg.intern(name);
                }
            }
            Instruction::Call(data) => {
                if let Some(name) = func.string_pool.get(data.name_id as usize) {
                    data.name_id = cg.intern(name);
                }
            }
            _ => {}
        }
    }

    let is_expr_body = !matches!(body, AstNode::Block(_, _));
    if is_expr_body && !matches!(func.instructions.last(), Some(Instruction::Return { .. })) {
        func.emit(Instruction::Return {
            value: Some(result_reg),
            loc,
        });
    } else if !matches!(func.instructions.last(), Some(Instruction::Return { .. })) {
        func.emit(Instruction::Return { value: None, loc });
    }

    // Use the shared closure counter for unique naming.
    let closure_name = format!("__closure_{}", cg.closure_counter);
    cg.closure_counter += 1;
    let name_id = cg.intern(&closure_name);
    cg.functions.push(UserFunction {
        name_id,
        params_count: params.len(),
        locals_count: func.next_reg,
        instructions: Arc::from(func.instructions),
        rest_at: None,
        kwargs_at: None,
    });
    // Build capture register indices from the PARENT's register space.
    let capture_regs: Vec<usize> = capture_names
        .iter()
        .filter_map(|name| cg.get_var(name).map(|v| v.idx))
        .collect();
    let dst = cg.alloc_reg();
    cg.emit(Instruction::MakeClosure {
        dst,
        name_id,
        captures: Arc::from(capture_regs),
    });
    Ok(dst)
}
