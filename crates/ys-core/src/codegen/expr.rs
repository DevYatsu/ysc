//! Expression code generation — template literals, function calls, short-circuit
//! operators.
//!
//! These are free functions that take `&mut Codegen` so they can be defined in
//! a child module without creating a circular dependency.

use super::Codegen;
use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;
use rustc_hash::FxHashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Template literals
// ---------------------------------------------------------------------------

pub(super) fn compile_template(
    cg: &mut Codegen,
    parts: &[TemplatePart],
    loc: Loc,
) -> Result<usize, JitError> {
    let mut result: Option<usize> = None;
    for part in parts {
        match part {
            TemplatePart::Text(s) => {
                let r = cg.alloc_reg();
                let val = Value::sso(s).unwrap_or_else(|| Value::pool(cg.intern(s)));
                cg.emit(Instruction::LoadLiteral { dst: r, val, loc });
                result = Some(concat(cg, result, r, loc)?);
            }
            TemplatePart::Expr(expr) => {
                let r = cg.compile_node(expr)?;
                // Wrap in str() call to ensure string
                let str_dst = cg.alloc_reg();
                let str_name = cg.intern("str");
                cg.emit(Instruction::Call(CallData {
                    name_id: str_name,
                    args_regs: Arc::from(vec![r]),
                    dst: Some(str_dst),
                    loc,
                }));
                result = Some(concat(cg, result, str_dst, loc)?);
            }
        }
    }
    Ok(result.unwrap_or_else(|| {
        let dst = cg.alloc_reg();
        cg.emit(Instruction::LoadLiteral {
            dst,
            val: Value::nil(),
            loc,
        });
        dst
    }))
}

pub(super) fn concat(
    cg: &mut Codegen,
    left: Option<usize>,
    right: usize,
    loc: Loc,
) -> Result<usize, JitError> {
    match left {
        None => Ok(right),
        Some(l) => {
            let dst = cg.alloc_reg();
            cg.emit(Instruction::Add {
                dst,
                lhs: l,
                rhs: right,
                loc,
            });
            cg.free_reg(l);
            cg.free_reg(right);
            Ok(dst)
        }
    }
}

// ---------------------------------------------------------------------------
// Function calls
// ---------------------------------------------------------------------------

pub(super) fn compile_fun_call(
    cg: &mut Codegen,
    name: &str,
    args: &[AstNode],
    named: &FxHashMap<String, AstNode>,
    loc: Loc,
) -> Result<usize, JitError> {
    // Resolve named arguments + defaults: map names to positions, fill defaults.
    let mut resolved: Vec<AstNode> = Vec::new();
    if let Some(params) = cg.fn_params.get(name).cloned() {
        let pcount = params.len();
        let rest_idx = params.iter().position(|p| p.is_rest);
        let kwargs_idx = params.iter().position(|p| p.is_kwargs);

        // Create slot for each declared param.  For rest params we skip the
        // rest slot — the runtime's `apply_rest` builds the list from extra
        // positional args directly, without a Nil placeholder in args_r.
        resolved = vec![AstNode::Nil(loc); pcount];

        // Fill positional args (one-to-one for declared params).
        for (i, arg) in args.iter().enumerate().take(pcount) {
            if Some(i) != rest_idx {
                resolved[i] = arg.clone();
            }
        }

        // For rest params, append remaining positional args past pcount so
        // the runtime can collect them into a list.
        if rest_idx.is_some() {
            for arg in args.iter().skip(pcount) {
                resolved.push(arg.clone());
            }
        }

        // Fill named args by parameter name.
        for (n, val) in named {
            if let Some(pos) = params.iter().position(|p| p.name == *n) {
                if pos < resolved.len() {
                    resolved[pos] = val.clone();
                }
            }
        }

        // Fill defaults for missing params (skip rest — it captures remaining).
        for (i, param) in params.iter().enumerate() {
            if param.is_rest { continue; }
            if i >= resolved.len() || matches!(resolved[i], AstNode::Nil(_)) {
                if let Some(ref default) = param.default {
                    if i >= resolved.len() { resolved.resize(i + 1, AstNode::Nil(loc)); }
                    resolved[i] = *default.clone();
                }
            }
        }

        // For kwargs, collect unmatched named args into an object.
        if kwargs_idx.is_some() {
            let extra_named: Vec<(String, AstNode)> = named
                .iter()
                .filter(|(n, _)| !params.iter().any(|p| p.name == **n && !p.is_kwargs))
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            if kwargs_idx.unwrap() < resolved.len() {
                resolved[kwargs_idx.unwrap()] = AstNode::ObjectLit(extra_named, loc);
            }
        }
    }
    if resolved.is_empty() {
        resolved = args.to_vec();
    }

    let args_r: Vec<usize> = resolved
        .iter()
        .map(|a| cg.compile_node(a))
        .collect::<Result<_, _>>()?;
    let dst = cg.alloc_reg();
    // For decorated functions: load from global (may hold wrapped version).
    if cg.decorated_fns.contains(name) {
        if let Some(info) = cg.get_var(name) {
            let callee_reg = cg.load_var(info);
            for &r in &args_r { cg.free_reg(r); }
            if info.is_global { cg.free_reg(callee_reg); }
            cg.emit(Instruction::CallDynamic(CallDynamicData {
                callee_reg, args_regs: Arc::from(args_r), dst: Some(dst), loc,
            }));
        } else {
            // Global doesn't exist yet — function is being decorated.
            // Use static dispatch so recursion still works.
            for &r in &args_r { cg.free_reg(r); }
            let name_id = cg.intern(name);
            cg.emit(Instruction::Call(CallData {
                name_id, args_regs: Arc::from(args_r), dst: Some(dst), loc,
            }));
        }
    } else if let Some(info) = cg.get_var(name) {
        // Variable holding a callable — dynamic dispatch
        let callee_reg = cg.load_var(info);
        for &r in &args_r { cg.free_reg(r); }
        if info.is_global { cg.free_reg(callee_reg); }
        cg.emit(Instruction::CallDynamic(CallDynamicData {
            callee_reg, args_regs: Arc::from(args_r), dst: Some(dst), loc,
        }));
    } else {
        for &r in &args_r { cg.free_reg(r); }
        let name_id = cg.intern(name);
        cg.emit(Instruction::Call(CallData {
            name_id, args_regs: Arc::from(args_r), dst: Some(dst), loc,
        }));
    }
    Ok(dst)
}

pub(super) fn compile_args(cg: &mut Codegen, args: &[AstNode]) -> Result<Vec<usize>, JitError> {
    args.iter().map(|a| cg.compile_node(a)).collect()
}

// ---------------------------------------------------------------------------
// Short-circuit && and ||
// ---------------------------------------------------------------------------

pub(super) fn compile_short_circuit(
    cg: &mut Codegen,
    op: BinOp,
    l: usize,
    r: usize,
    _loc: Loc,
) -> Result<usize, JitError> {
    let dst = cg.alloc_reg();
    match op {
        BinOp::And => {
            // a && b: if a is falsy → short-circuit (result = a), else evaluate b
            cg.emit(Instruction::Move { dst, src: l });
            let jump_idx = cg.instructions.len();
            cg.emit(Instruction::Jump(0)); // placeholder
            cg.emit(Instruction::Move { dst, src: r });
            let end = cg.instructions.len();
            cg.instructions[jump_idx] = Instruction::JumpIfFalse {
                cond: l,
                target: end,
            };
        }
        BinOp::Or => {
            // a || b: if a is truthy → short-circuit (result = a), else evaluate b
            // Only have JumpIfFalse, so invert: if l is falsy, evaluate r
            cg.emit(Instruction::Move { dst, src: l });
            let jump_false_idx = cg.instructions.len();
            cg.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse to eval_r
            let jump_end_idx = cg.instructions.len();
            cg.emit(Instruction::Jump(0)); // placeholder → Jump(end) when truthy
            let eval_r = cg.instructions.len();
            cg.emit(Instruction::Move { dst, src: r });
            let end = cg.instructions.len();
            cg.instructions[jump_false_idx] = Instruction::JumpIfFalse {
                cond: l,
                target: eval_r,
            };
            cg.instructions[jump_end_idx] = Instruction::Jump(end);
        }
        _ => unreachable!(),
    }
    Ok(dst)
}
