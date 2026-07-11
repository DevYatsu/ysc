//! Optimisation passes over the AST.
//!
//! Each pass transforms the AST in-place. Passes are run in order by
//! [`optimize_program`].

use crate::ast::*;
use crate::compiler::Loc;

/// Run all optimisation passes on a complete AST program.
pub fn optimize_program(program: &mut AstBlock) {
    constant_fold_block(program);
}

// ── Constant folding ─────────────────────────────────────────────────────────

fn constant_fold_block(block: &mut AstBlock) {
    for node in block.iter_mut() {
        *node = constant_fold(std::mem::replace(node, AstNode::Nil(Loc { line: 0, col: 0 })));
    }
}

fn constant_fold(node: AstNode) -> AstNode {
    match node {
        // Fold binary ops with constant operands
        AstNode::Binary { op, lhs, rhs, loc } => {
            let lhs = constant_fold(*lhs);
            let rhs = constant_fold(*rhs);
            match (&lhs, &rhs) {
                (AstNode::Number(l, _), AstNode::Number(r, _)) => {
                    match op {
                        BinOp::Add => AstNode::Number(l + r, loc),
                        BinOp::Sub => AstNode::Number(l - r, loc),
                        BinOp::Mul => AstNode::Number(l * r, loc),
                        BinOp::Div => AstNode::Number(l / r, loc),
                        BinOp::Mod => AstNode::Number(l % r, loc),
                        BinOp::Eq => AstNode::Bool((l - r).abs() < f64::EPSILON, loc),
                        BinOp::Ne => AstNode::Bool((l - r).abs() >= f64::EPSILON, loc),
                        BinOp::Lt => AstNode::Bool(l < r, loc),
                        BinOp::Le => AstNode::Bool(l <= r, loc),
                        BinOp::Gt => AstNode::Bool(l > r, loc),
                        BinOp::Ge => AstNode::Bool(l >= r, loc),
                        BinOp::And | BinOp::Or => {
                            AstNode::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), loc }
                        }
                    }
                }
                _ => AstNode::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), loc },
            }
        }

        // Fold unary ops with constant operand
        AstNode::Unary { op, expr, loc } => {
            let expr = constant_fold(*expr);
            match (op, &expr) {
                (UnaryOp::Neg, AstNode::Number(n, _)) => AstNode::Number(-n, loc),
                (UnaryOp::Not, AstNode::Bool(b, _))   => AstNode::Bool(!b, loc),
                _ => AstNode::Unary { op, expr: Box::new(expr), loc },
            }
        }

        // Recurse into compound nodes
        AstNode::Block(stmts, loc) => {
            let mut new_stmts = stmts;
            constant_fold_block(&mut new_stmts);
            AstNode::Block(new_stmts, loc)
        }
        AstNode::If { cond, then_block, else_block, loc } => {
            let cond = constant_fold(*cond);
            let mut then_block = then_block;
            let mut else_block = else_block;
            constant_fold_block(&mut then_block);
            constant_fold_block(&mut else_block);
            // Strength reduction: known condition → eliminate dead branch
            match &cond {
                AstNode::Bool(true, _) => {
                    if else_block.is_empty() {
                        if then_block.len() == 1 {
                            return then_block.into_iter().next().unwrap();
                        }
                        return AstNode::Block(then_block, loc);
                    }
                }
                AstNode::Bool(false, _) => {
                    if then_block.is_empty() {
                        if else_block.len() == 1 {
                            return else_block.into_iter().next().unwrap();
                        }
                        return AstNode::Block(else_block, loc);
                    }
                }
                _ => {}
            }
            AstNode::If { cond: Box::new(cond), then_block, else_block, loc }
        }
        AstNode::While { cond, body, loc } => {
            let cond = constant_fold(*cond);
            let mut body = body;
            constant_fold_block(&mut body);
            // while false → eliminate entire loop
            if matches!(&cond, AstNode::Bool(false, _)) {
                return AstNode::Block(vec![], loc);
            }
            AstNode::While { cond: Box::new(cond), body, loc }
        }
        AstNode::For { var, iter, body, loc } => {
            let iter = constant_fold(*iter);
            let mut body = body;
            constant_fold_block(&mut body);
            AstNode::For { var, iter: Box::new(iter), body, loc }
        }
        AstNode::Closure { params, body, is_move, loc } => {
            let body = constant_fold(*body);
            AstNode::Closure { params, body: Box::new(body), is_move, loc }
        }
        AstNode::ListLit(elems, loc) => {
            AstNode::ListLit(elems.into_iter().map(constant_fold).collect(), loc)
        }
        AstNode::FunDecl { name, params, body, exported, loc } => {
            let mut body = body;
            constant_fold_block(&mut body);
            AstNode::FunDecl { name, params, body, exported, loc }
        }

        // Everything else: leave as-is
        _ => node,
    }
}
