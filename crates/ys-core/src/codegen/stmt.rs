//! Statement code generation — assignment, if/while/for control flow.
//!
//! These are free functions that take `&mut Codegen` so they can be defined in
//! a child module without creating a circular dependency.

use super::{Codegen, VarInfo};
use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

pub(super) fn compile_assign(
    cg: &mut Codegen,
    target: &AstNode,
    value: &AstNode,
    loc: Loc,
) -> Result<usize, JitError> {
    let src = cg.compile_node(value)?;
    let dst = cg.alloc_reg();
    cg.emit(Instruction::Move { dst, src });

    match target {
        AstNode::Ident(name, _) => {
            // Check for increment pattern: x = x + 1 or x = 1 + x
            if let AstNode::Binary {
                op: BinOp::Add,
                lhs,
                rhs,
                ..
            } = value
            {
                if let (AstNode::Ident(lname, _), AstNode::Number(n, _)) = (&**lhs, &**rhs)
                    && lname == name
                    && *n == 1.0
                {
                    let info = cg.ensure_var(name);
                    cg.instructions.pop();
                    if info.is_global {
                        cg.emit(Instruction::IncrementGlobal(info.idx));
                    } else {
                        cg.emit(Instruction::Increment(info.idx));
                    }
                    return Ok(info.idx);
                }
                if let (AstNode::Number(n, _), AstNode::Ident(rname, _)) = (&**lhs, &**rhs)
                    && rname == name
                    && *n == 1.0
                {
                    let info = cg.ensure_var(name);
                    cg.instructions.pop();
                    if info.is_global {
                        cg.emit(Instruction::IncrementGlobal(info.idx));
                    } else {
                        cg.emit(Instruction::Increment(info.idx));
                    }
                    return Ok(info.idx);
                }
            }
            // Check for decrement pattern: x = x - 1
            if let AstNode::Binary {
                op: BinOp::Sub,
                lhs,
                rhs,
                ..
            } = value
                && let (AstNode::Ident(lname, _), AstNode::Number(n, _)) = (&**lhs, &**rhs)
                && lname == name
                && *n == 1.0
            {
                let info = cg.ensure_var(name);
                cg.instructions.pop();
                if info.is_global {
                    cg.emit(Instruction::DecrementGlobal(info.idx));
                } else {
                    cg.emit(Instruction::Decrement(info.idx));
                }
                return Ok(info.idx);
            }
            let info = cg.ensure_var(name);
            if info.is_global {
                cg.emit(Instruction::StoreGlobal {
                    global: info.idx,
                    src,
                });
            } else {
                cg.emit(Instruction::Move { dst: info.idx, src });
            }
            Ok(dst)
        }
        AstNode::Index { obj, index, .. } => {
            let obj_r = cg.compile_node(obj)?;
            let idx_r = cg.compile_node(index)?;
            cg.emit(Instruction::ListSet {
                list: obj_r,
                index_reg: idx_r,
                src,
                loc,
            });
            Ok(src)
        }
        AstNode::Field { obj, name, .. } => {
            let obj_r = cg.compile_node(obj)?;
            let name_id = cg.intern(name);
            cg.emit(Instruction::ObjectSet {
                obj: obj_r,
                name_id,
                src,
                loc,
            });
            Ok(src)
        }
        _ => Err(JitError::parsing(
            "Invalid assignment target",
            loc.as_error_pos(),
        )),
    }
}

// ---------------------------------------------------------------------------
// If / else
// ---------------------------------------------------------------------------

pub(super) fn compile_if(
    cg: &mut Codegen,
    cond: &AstNode,
    then_block: &[AstNode],
    else_block: &[AstNode],
    _loc: Loc,
) -> Result<usize, JitError> {
    let cond_r = cg.compile_node(cond)?;
    let jump_idx = cg.instructions.len();
    cg.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse

    let _ = cg.compile_block(then_block)?;

    if !else_block.is_empty() {
        let else_jump = cg.instructions.len();
        cg.emit(Instruction::Jump(0));
        let else_start = cg.instructions.len();
        cg.instructions[jump_idx] = Instruction::JumpIfFalse {
            cond: cond_r,
            target: else_start,
        };
        let _ = cg.compile_block(else_block)?;
        cg.instructions[else_jump] = Instruction::Jump(cg.instructions.len());
    } else {
        let end = cg.instructions.len();
        cg.instructions[jump_idx] = Instruction::JumpIfFalse {
            cond: cond_r,
            target: end,
        };
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// While loop
// ---------------------------------------------------------------------------

pub(super) fn compile_while(
    cg: &mut Codegen,
    cond: &AstNode,
    body: &[AstNode],
    _loc: Loc,
) -> Result<usize, JitError> {
    let loop_start = cg.instructions.len();
    let cond_r = cg.compile_node(cond)?;
    let jump_idx = cg.instructions.len();
    cg.emit(Instruction::Jump(0)); // placeholder
    let _ = cg.compile_block(body)?;
    cg.emit(Instruction::Jump(loop_start));
    cg.instructions[jump_idx] = Instruction::JumpIfFalse {
        cond: cond_r,
        target: cg.instructions.len(),
    };
    Ok(0)
}

// ---------------------------------------------------------------------------
// For loop
// ---------------------------------------------------------------------------

pub(super) fn compile_for(
    cg: &mut Codegen,
    var: &str,
    iter: &AstNode,
    body: &[AstNode],
    loc: Loc,
) -> Result<usize, JitError> {
    let iter_r = cg.compile_node(iter)?;

    // Build a Range object at runtime if it's a compile-time Range AST node
    // so that ForNext can handle it uniformly with lists and objects.
    let iter_reg = if let AstNode::Range {
        start, end, step, ..
    } = iter
    {
        let s = cg.compile_node(start)?;
        let e = cg.compile_node(end)?;
        let st = match step {
            Some(sn) => cg.compile_node(sn)?,
            None => {
                let r = cg.alloc_reg();
                cg.emit(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::number(1.0),
                    loc,
                });
                r
            }
        };
        let dst = cg.alloc_reg();
        cg.emit(Instruction::Range {
            dst,
            start: s,
            end: e,
            step: Some(st),
            loc,
        });
        dst
    } else {
        iter_r
    };

    // Index register (starts at 0, incremented each iteration by ForNext)
    let idx_reg = cg.alloc_reg();
    cg.emit(Instruction::LoadLiteral {
        dst: idx_reg,
        val: Value::nil(),
        loc,
    });

    // "Has more" flag register
    let done_reg = cg.alloc_reg();
    let var_reg = cg.alloc_reg();

    let was_in_fn = cg.is_in_function;
    cg.is_in_function = true;
    cg.locals.insert(
        var.to_string(),
        VarInfo {
            idx: var_reg,
            is_global: false,
        },
    );
    cg.var_mask |= 1 << var_reg; // Loop variable is not a temporary

    let loop_start = cg.instructions.len();

    // ForNext: dst_val = iterable[idx_reg], dst_done = has_more, idx_reg++
    cg.emit(Instruction::ForNext {
        dst_val: var_reg,
        dst_done: done_reg,
        iterable: iter_reg,
        idx_reg,
        loc,
    });

    // If not has_more → exit
    let exit_jump = cg.instructions.len();
    cg.emit(Instruction::JumpIfFalse {
        cond: done_reg,
        target: 0,
    });

    // Body
    let _ = cg.compile_block(body)?;

    // Loop back
    cg.emit(Instruction::Jump(loop_start));

    // Patch exit jump
    let end_pos = cg.instructions.len();
    cg.instructions[exit_jump] = Instruction::JumpIfFalse {
        cond: done_reg,
        target: end_pos,
    };

    cg.locals.remove(var);
    cg.is_in_function = was_in_fn;
    Ok(0)
}
