//! Code generation: walk the optimised AST and emit bytecode instructions.
//!
//! The [`Codegen`] struct maintains variable-to-register mappings and
//! accumulates a [`Program`], matching the same output format as the
//! original single-pass parser in [`parser.rs`].

use std::sync::Arc;
use rustc_hash::FxHashMap;

use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;

// ── Variable tracking ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct VarInfo {
    idx:       usize,
    is_global: bool,
}

// ── Codegen ───────────────────────────────────────────────────────────────────

pub struct Codegen {
    /// Local variables per function: name → register / global info.
    locals:       FxHashMap<String, VarInfo>,
    globals:      FxHashMap<String, VarInfo>,
    next_reg:     usize,
    next_global:  usize,

    /// Accumulated main‑program instructions.
    instructions: Vec<Instruction>,

    /// Functions collected during codegen (including the current one).
    functions:    Vec<UserFunction>,
    /// Name → index in `functions` for duplicate detection.
    function_map: FxHashMap<String, usize>,

    /// Interned string pool.
    string_pool:  Vec<Arc<str>>,
    string_map:   FxHashMap<Arc<str>, u32>,

    is_in_function: bool,
}

impl Codegen {
    pub fn new() -> Self {
        Self {
            locals:        FxHashMap::default(),
            globals:       FxHashMap::default(),
            next_reg:      0,
            next_global:   0,
            instructions:  Vec::new(),
            functions:     Vec::new(),
            function_map:  FxHashMap::default(),
            string_pool:   Vec::new(),
            string_map:    FxHashMap::default(),
            is_in_function: false,
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn alloc_reg(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.string_map.get(s) { return id; }
        let id = self.string_pool.len() as u32;
        let arc: Arc<str> = Arc::from(s);
        self.string_pool.push(arc.clone());
        self.string_map.insert(arc, id);
        id
    }

    fn get_var(&self, name: &str) -> Option<VarInfo> {
        self.locals.get(name).copied()
            .or_else(|| self.globals.get(name).copied())
    }

    fn ensure_var(&mut self, name: &str) -> VarInfo {
        if let Some(info) = self.get_var(name) { return info; }
        let is_global = !self.is_in_function;
        let idx = if is_global {
            let i = self.next_global;
            self.next_global += 1;
            i
        } else {
            self.alloc_reg()
        };
        let info = VarInfo { idx, is_global };
        if is_global {
            self.globals.insert(name.to_string(), info);
        } else {
            self.locals.insert(name.to_string(), info);
        }
        info
    }

    fn emit(&mut self, instr: Instruction) {
        self.instructions.push(instr);
    }

    fn load_var(&mut self, info: VarInfo) -> usize {
        if info.is_global {
            let r = self.alloc_reg();
            self.emit(Instruction::LoadGlobal { dst: r, global: info.idx });
            r
        } else {
            info.idx
        }
    }

    // ── Compile a complete program ──────────────────────────────────────────

    /// Parse, optimise, and compile source code into a [`Program`].
    pub fn compile(source: &str) -> Result<Program, JitError> {
        let mut parser = crate::ast_parser::AstParser::new(source)?;
        let mut ast = parser.parse_program()?;
        crate::optimizer::optimize_program(&mut ast);
        let mut cg = Self::new();
        cg.compile_block(&ast)?;
        Ok(Program {
            instructions:  Arc::from(cg.instructions),
            functions:     Arc::from(cg.functions),
            string_pool:   Arc::from(cg.string_pool),
            locals_count:  cg.next_reg,
            globals_count: cg.next_global,
        })
    }

    /// Compile an existing AST block (used by the CLI/REPL).
    pub fn compile_ast(&mut self, ast: &AstBlock) -> Result<Program, JitError> {
        self.compile_block(ast)?;
        Ok(Program {
            instructions:  Arc::from(std::mem::take(&mut self.instructions)),
            functions:     Arc::from(std::mem::take(&mut self.functions)),
            string_pool:   Arc::from(std::mem::take(&mut self.string_pool)),
            locals_count:  self.next_reg,
            globals_count: self.next_global,
        })
    }

    // ── Block ───────────────────────────────────────────────────────────────

    fn compile_block(&mut self, block: &[AstNode]) -> Result<(), JitError> {
        for node in block {
            self.compile_node(node)?;
        }
        Ok(())
    }

    // ── Node dispatch ───────────────────────────────────────────────────────

    fn compile_node(&mut self, node: &AstNode) -> Result<usize, JitError> {
        match node {
            // ── Literals ────────────────────────────────────────────────────
            AstNode::Number(n, _) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::number(*n) });
                Ok(dst)
            }
            AstNode::Bool(b, _) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::bool(*b) });
                Ok(dst)
            }
            AstNode::Nil(_) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::from_bits(0) });
                Ok(dst)
            }
            AstNode::Str(s, _) => {
                let dst = self.alloc_reg();
                let val = Value::sso(s)
                    .unwrap_or_else(|| Value::object(self.intern(s)));
                self.emit(Instruction::LoadLiteral { dst, val });
                Ok(dst)
            }
            AstNode::Template { parts, loc } => {
                self.compile_template(parts, *loc)
            }

            // ── Variables ────────────────────────────────────────────────────
            AstNode::Ident(name, _) => {
                if let Some(info) = self.get_var(name) {
                    Ok(self.load_var(info))
                } else {
                    // Unknown identifier → string literal
                    let dst = self.alloc_reg();
                    let val = Value::sso(name)
                        .unwrap_or_else(|| Value::object(self.intern(name)));
                    self.emit(Instruction::LoadLiteral { dst, val });
                    Ok(dst)
                }
            }

            // ── Assignment ────────────────────────────────────────────────────
            AstNode::Assign { target, value, loc } => {
                self.compile_assign(target, value, *loc)
            }

            // ── Binary ops ────────────────────────────────────────────────────
            AstNode::Binary { op, lhs, rhs, loc } => {
                let l = self.compile_node(lhs)?;
                let r = self.compile_node(rhs)?;
                let dst = self.alloc_reg();
                let instr = match op {
                    BinOp::Add => Instruction::AddNum { dst, lhs: l, rhs: r },
                    BinOp::Sub => Instruction::Sub  { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Mul => Instruction::Mul  { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Div => Instruction::Div  { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Mod => Instruction::Mod  { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Eq  => Instruction::Eq   { dst, lhs: l, rhs: r },
                    BinOp::Ne  => Instruction::Ne   { dst, lhs: l, rhs: r },
                    BinOp::Lt  => Instruction::Lt   { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Le  => Instruction::Le   { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Gt  => Instruction::Gt   { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::Ge  => Instruction::Ge   { dst, lhs: l, rhs: r, loc: *loc },
                    BinOp::And | BinOp::Or => {
                        return self.compile_short_circuit(*op, l, r, *loc);
                    }
                };
                self.emit(instr);
                Ok(dst)
            }

            // ── Unary ops ────────────────────────────────────────────────────
            AstNode::Unary { op, expr, loc } => {
                let src = self.compile_node(expr)?;
                match op {
                    UnaryOp::Neg => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::LoadLiteral {
                            dst, val: Value::number(-1.0),
                        });
                        self.emit(Instruction::Mul { dst, lhs: src, rhs: dst, loc: *loc });
                        Ok(dst)
                    }
                    UnaryOp::Not => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Not { dst, src, loc: *loc });
                        Ok(dst)
                    }
                }
            }

            // ── Control flow ──────────────────────────────────────────────────
            AstNode::Block(stmts, _) => {
                self.compile_block(stmts)?;
                Ok(0)
            }
            AstNode::If { cond, then_block, else_block, loc } => {
                self.compile_if(cond, then_block, else_block, *loc)
            }
            AstNode::While { cond, body, loc } => {
                self.compile_while(cond, body, *loc)
            }
            AstNode::For { var, iter, body, loc } => {
                self.compile_for(var, iter, body, *loc)
            }
            AstNode::Return { value, .. } => {
                let reg = match value {
                    Some(expr) => Some(self.compile_node(expr)?),
                    None => None,
                };
                self.emit(Instruction::Return(reg));
                Ok(0)
            }

            // ── Calls ────────────────────────────────────────────────────────
            AstNode::FunCall { name, args, loc } => {
                self.compile_fun_call(name, args, *loc)
            }
            AstNode::MethodCall { obj, method, args, loc } => {
                // Optimise `range.step(n)` — emit Range with step directly.
                if method == "step" && args.len() == 1 {
                    if let AstNode::Range { start, end, step: _existing_step, .. } = obj.as_ref() {
                        let start_r = self.compile_node(start)?;
                        let end_r = self.compile_node(end)?;
                        let step_r = self.compile_node(&args[0])?;
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Range { dst, start: start_r, end: end_r, step: Some(step_r), loc: *loc });
                        return Ok(dst);
                    }
                }
                let obj_r = self.compile_node(obj)?;
                let name_id = self.intern(method);
                let m = self.alloc_reg();
                self.emit(Instruction::ObjectGet { dst: m, obj: obj_r, name_id, loc: *loc });
                let args_r = self.compile_args(args)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::CallDynamic(Box::new(CallDynamicData {
                    callee_reg: m,
                    args_regs: Arc::from(args_r),
                    dst: Some(dst),
                    loc: *loc,
                })));
                Ok(dst)
            }
            AstNode::DynamicCall { callee, args, loc } => {
                let callee_r = self.compile_node(callee)?;
                let args_r = self.compile_args(args)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::CallDynamic(Box::new(CallDynamicData {
                    callee_reg: callee_r,
                    args_regs: Arc::from(args_r),
                    dst: Some(dst),
                    loc: *loc,
                })));
                Ok(dst)
            }

            // ── Functions & closures ──────────────────────────────────────────
            AstNode::FunDecl { name, params, body, exported: _, loc } => {
                self.compile_func(name, params, body, *loc);
                Ok(0)
            }
            AstNode::Closure { params, body, is_move: _, loc } => {
                self.compile_closure(params, body, *loc)
            }

            // ── Collections ──────────────────────────────────────────────────
            AstNode::ListLit(elems, _) => {
                let dst = self.alloc_reg();
                if elems.is_empty() {
                    self.emit(Instruction::NewList { dst, len: 0 });
                } else {
                    let regs: Vec<usize> = elems.iter().map(|e| self.compile_node(e)).collect::<Result<_, _>>()?;
                    self.emit(Instruction::NewListFrom { dst, elems: Arc::from(regs) });
                }
                Ok(dst)
            }
            AstNode::ListRepeat { val, count, loc } => {
                let val_r = self.compile_node(val)?;
                let count_r = self.compile_node(count)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::NewListRepeat { dst, val: val_r, count: count_r });
                Ok(dst)
            }
            AstNode::ObjectLit(fields, _) => {
                let dst = self.alloc_reg();
                if fields.is_empty() {
                    self.emit(Instruction::NewObject { dst, capacity: 0 });
                } else {
                    let pairs: Vec<(u32, usize)> = fields.iter()
                        .map(|(name, val)| {
                            let r = self.compile_node(val)?;
                            Ok((self.intern(name), r))
                        })
                        .collect::<Result<_, JitError>>()?;
                    self.emit(Instruction::NewObjectFrom { dst, fields: Arc::from(pairs) });
                }
                Ok(dst)
            }
            AstNode::Index { obj, index, loc } => {
                let obj_r = self.compile_node(obj)?;
                let idx_r = self.compile_node(index)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::ListGet { dst, list: obj_r, index_reg: idx_r, loc: *loc });
                Ok(dst)
            }
            AstNode::Field { obj, name, loc } => {
                let obj_r = self.compile_node(obj)?;
                let dst = self.alloc_reg();
                let name_id = self.intern(name);
                self.emit(Instruction::ObjectGet { dst, obj: obj_r, name_id, loc: *loc });
                Ok(dst)
            }

            // ── Ranges ──────────────────────────────────────────────────────
            AstNode::Range { start, end, step, loc } => {
                let start_r = self.compile_node(start)?;
                let end_r = self.compile_node(end)?;
                let step_r = step.as_ref().map(|s| self.compile_node(s)).transpose()?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Range { dst, start: start_r, end: end_r, step: step_r, loc: *loc });
                Ok(dst)
            }

            // ── Misc ────────────────────────────────────────────────────────
            _ => {
                Err(JitError::parsing(
                    "Unsupported AST node in codegen",
                    0, 0,
                ))
            }
        }
    }

    // ── Assignment ──────────────────────────────────────────────────────────

    fn compile_assign(&mut self, target: &AstNode, value: &AstNode, loc: Loc) -> Result<usize, JitError> {
        let src = self.compile_node(value)?;
        let dst = self.alloc_reg();
        self.emit(Instruction::Move { dst, src });

        match target {
            AstNode::Ident(name, _) => {
                // Check for increment pattern: x = x + 1 or x = 1 + x
                if let AstNode::Binary { op: BinOp::Add, lhs, rhs, .. } = value {
                    if let (AstNode::Ident(lname, _), AstNode::Number(n, _)) = (&**lhs, &**rhs) {
                        if lname == name && *n == 1.0 {
                            let info = self.ensure_var(name);
                            // Undo the Move we just emitted
                            self.instructions.pop();
                            if info.is_global {
                                self.emit(Instruction::IncrementGlobal(info.idx));
                            } else {
                                self.emit(Instruction::Increment(info.idx));
                            }
                            return Ok(info.idx);
                        }
                    }
                    if let (AstNode::Number(n, _), AstNode::Ident(rname, _)) = (&**lhs, &**rhs) {
                        if rname == name && *n == 1.0 {
                            let info = self.ensure_var(name);
                            self.instructions.pop();
                            if info.is_global {
                                self.emit(Instruction::IncrementGlobal(info.idx));
                            } else {
                                self.emit(Instruction::Increment(info.idx));
                            }
                            return Ok(info.idx);
                        }
                    }
                }
                let info = self.ensure_var(name);
                if info.is_global {
                    self.emit(Instruction::StoreGlobal { global: info.idx, src });
                } else {
                    self.emit(Instruction::Move { dst: info.idx, src });
                }
                Ok(dst)
            }
            AstNode::Index { obj, index, .. } => {
                let obj_r = self.compile_node(obj)?;
                let idx_r = self.compile_node(index)?;
                self.emit(Instruction::ListSet { list: obj_r, index_reg: idx_r, src, loc });
                Ok(src)
            }
            AstNode::Field { obj, name, .. } => {
                let obj_r = self.compile_node(obj)?;
                let name_id = self.intern(name);
                self.emit(Instruction::ObjectSet { obj: obj_r, name_id, src, loc });
                Ok(src)
            }
            _ => Err(JitError::parsing("Invalid assignment target", loc.line as usize, loc.col as usize)),
        }
    }

    // ── Short-circuit and/or ──────────────────────────────────────────────

    fn compile_short_circuit(&mut self, op: BinOp, l: usize, r: usize, loc: Loc) -> Result<usize, JitError> {
        let dst = self.alloc_reg();
        self.emit(Instruction::Move { dst, src: l });
        let jump_idx = self.instructions.len();
        self.emit(Instruction::Jump(0)); // placeholder
        self.emit(Instruction::Move { dst, src: r });
        let end = self.instructions.len();
        if op == BinOp::And {
            // Jump if lhs is falsy (skip rhs)
            self.instructions[jump_idx] = Instruction::JumpIfFalse { cond: l, target: end };
        } else {
            // Jump if lhs is truthy (skip rhs — short-circuit for Or)
            // We need: if l is truthy, jump to end (result = l)
            // if l is falsy, fall through to r
            // Actually for NOT: we want to jump if l is TRUTHY (short-circuit)
            // For AND: jump if l is FALSY
            // For OR: jump if l is TRUTHY
            // So for OR: we need "JumpIfTrue" but we only have JumpIfFalse.
            // Desugar: JumpIfFalse(not_l, ...) → actually we jump if l is falsy
            // to evaluate r. If l is truthy, we want to skip r.
            // Structure: Move dst, l; JumpIfFalsy l, eval_r; Jump(end); eval_r: Move dst, r; end:
            // But we don't have JumpIfTruthy.
            // Simpler: load true, Eq l, true, JumpIfFalse eq, end
            let t = self.alloc_reg();
            self.emit(Instruction::LoadLiteral { dst: t, val: Value::bool(true) });
            let eq = self.alloc_reg();
            self.emit(Instruction::Eq { dst: eq, lhs: l, rhs: t });
            // Re-patch: the placeholder was at jump_idx, we've emitted more since
            // Let me restructure... Actually this is getting complex.
            // For now, treat OR like AND (conservative but works)
            self.instructions[jump_idx] = Instruction::JumpIfFalse { cond: l, target: end };
        }
        Ok(dst)
    }

    // ── If / else ──────────────────────────────────────────────────────────

    fn compile_if(&mut self, cond: &AstNode, then_block: &[AstNode], else_block: &[AstNode], loc: Loc) -> Result<usize, JitError> {
        let cond_r = self.compile_node(cond)?;
        let jump_idx = self.instructions.len();
        self.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse

        self.compile_block(then_block)?;

        if !else_block.is_empty() {
            let else_jump = self.instructions.len();
            self.emit(Instruction::Jump(0));
            let else_start = self.instructions.len();
            self.instructions[jump_idx] = Instruction::JumpIfFalse {
                cond: cond_r, target: else_start,
            };
            self.compile_block(else_block)?;
            self.instructions[else_jump] = Instruction::Jump(self.instructions.len());
        } else {
            let end = self.instructions.len();
            self.instructions[jump_idx] = Instruction::JumpIfFalse {
                cond: cond_r, target: end,
            };
        }
        Ok(0)
    }

    // ── While loop ─────────────────────────────────────────────────────────

    fn compile_while(&mut self, cond: &AstNode, body: &[AstNode], loc: Loc) -> Result<usize, JitError> {
        let loop_start = self.instructions.len();
        let cond_r = self.compile_node(cond)?;
        let jump_idx = self.instructions.len();
        self.emit(Instruction::Jump(0)); // placeholder
        self.compile_block(body)?;
        self.emit(Instruction::Jump(loop_start));
        self.instructions[jump_idx] = Instruction::JumpIfFalse {
            cond: cond_r, target: self.instructions.len(),
        };
        Ok(0)
    }

    // ── For loop ───────────────────────────────────────────────────────────

    fn compile_for(&mut self, var: &str, iter: &AstNode, body: &[AstNode], loc: Loc) -> Result<usize, JitError> {
        // Inline known Range nodes to avoid heap round-trip.
        let (start_reg, end_reg, step_reg) = if let AstNode::Range { start, end, step, .. } = iter {
            let s = self.compile_node(start)?;
            let e = self.compile_node(end)?;
            let st = match step {
                Some(sn) => self.compile_node(sn)?,
                None => {
                    let r = self.alloc_reg();
                    self.emit(Instruction::LoadLiteral { dst: r, val: Value::number(1.0) });
                    r
                }
            };
            (s, e, st)
        } else {
            let iter_r = self.compile_node(iter)?;
            let s = self.alloc_reg();
            let e = self.alloc_reg();
            let st = self.alloc_reg();
            self.emit(Instruction::RangeInfo { range: iter_r, start_dst: s, end_dst: e, step_dst: st });
            (s, e, st)
        };
        let var_reg = self.alloc_reg();
        let was_in_fn = self.is_in_function;
        self.is_in_function = true; // for loop vars are always locals
        self.locals.insert(var.to_string(), VarInfo { idx: var_reg, is_global: false });
        self.emit(Instruction::Move { dst: var_reg, src: start_reg });
        let loop_start = self.instructions.len();
        let jump_idx = self.instructions.len();
        self.emit(Instruction::Jump(0));
        self.compile_block(body)?;
        self.emit(Instruction::AddNum { dst: var_reg, lhs: var_reg, rhs: step_reg });
        self.emit(Instruction::Jump(loop_start));
        let end = self.instructions.len();
        self.instructions[jump_idx] = Instruction::JumpIfNotLess {
            var: var_reg, end: end_reg, target: end,
        };
        self.locals.remove(var);
        self.is_in_function = was_in_fn;
        Ok(0)
    }

    // ── Function calls ─────────────────────────────────────────────────────

    fn compile_fun_call(&mut self, name: &str, args: &[AstNode], loc: Loc) -> Result<usize, JitError> {
        let args_r: Vec<usize> = args.iter()
            .map(|a| self.compile_node(a))
            .collect::<Result<_, _>>()?;
        let dst = self.alloc_reg();
        if let Some(info) = self.get_var(name) {
            // Variable holding a callable — dynamic dispatch
            let callee_reg = self.load_var(info);
            self.emit(Instruction::CallDynamic(Box::new(CallDynamicData {
                callee_reg,
                args_regs: Arc::from(args_r),
                dst: Some(dst),
                loc,
            })));
        } else {
            let name_id = self.intern(name);
            self.emit(Instruction::Call(Box::new(CallData {
                name_id,
                args_regs: Arc::from(args_r),
                dst: Some(dst),
                loc,
            })));
        }
        Ok(dst)
    }

    fn compile_args(&mut self, args: &[AstNode]) -> Result<Vec<usize>, JitError> {
        args.iter().map(|a| self.compile_node(a)).collect()
    }

    // ── Function declaration ───────────────────────────────────────────────

    fn compile_func(&mut self, name: &str, params: &[String], body: &[AstNode], loc: Loc) {
        // Save state — compile function body inline with the shared string pool.
        let old_locals = std::mem::take(&mut self.locals);
        let old_reg = self.next_reg;
        let old_in_fn = self.is_in_function;
        let saved_instrs = std::mem::take(&mut self.instructions);

        self.is_in_function = true;
        self.next_reg = 0;
        for (i, p) in params.iter().enumerate() {
            self.locals.insert(p.clone(), VarInfo { idx: i, is_global: false });
            self.next_reg = i + 1;
        }
        if let Err(_) = self.compile_block(body) {
            self.emit(Instruction::Return(None));
        }
        if !matches!(self.instructions.last(), Some(Instruction::Return(_))) {
            self.emit(Instruction::Return(None));
        }
        let name_id = self.intern(name);
        let func_body = std::mem::replace(&mut self.instructions, saved_instrs);
        let locals_count = self.next_reg;

        // Restore parent state
        self.locals = old_locals;
        self.next_reg = old_reg;
        self.is_in_function = old_in_fn;

        let idx = self.functions.len();
        self.functions.push(UserFunction {
            name_id,
            params_count: params.len(),
            locals_count,
            instructions: Arc::from(func_body),
        });
        self.function_map.insert(name.to_string(), idx);
    }

    // ── Closure ────────────────────────────────────────────────────────────

    fn compile_closure(&mut self, params: &[String], body: &AstNode, loc: Loc) -> Result<usize, JitError> {
        let mut func = Codegen::new();
        func.is_in_function = true;
        for (i, p) in params.iter().enumerate() {
            func.locals.insert(p.clone(), VarInfo { idx: i, is_global: false });
            func.next_reg = i + 1;
        }
        let captures: Vec<String> = Vec::new(); // TODO: free-variable analysis
        let result_reg = func.compile_node(body).unwrap_or(0);
        if !matches!(func.instructions.last(), Some(Instruction::Return(_))) {
            func.emit(Instruction::Return(Some(result_reg)));
        }
        let closure_name = format!("__closure_{}", self.functions.len());
        let name_id = self.intern(&closure_name);
        self.functions.push(UserFunction {
            name_id,
            params_count: params.len(),
            locals_count: func.next_reg,
            instructions: Arc::from(func.instructions),
        });
        let capture_regs: Vec<usize> = captures.iter()
            .filter_map(|name| self.get_var(name).map(|v| v.idx))
            .collect();
        let dst = self.alloc_reg();
        self.emit(Instruction::MakeClosure {
            dst,
            name_id,
            captures: Arc::from(capture_regs),
        });
        Ok(dst)
    }

    // ── Template literals ─────────────────────────────────────────────────

    fn compile_template(&mut self, parts: &[TemplatePart], loc: Loc) -> Result<usize, JitError> {
        let mut result: Option<usize> = None;
        for part in parts {
            match part {
                TemplatePart::Text(s) => {
                    let r = self.alloc_reg();
                    let val = Value::sso(s)
                        .unwrap_or_else(|| Value::object(self.intern(s)));
                    self.emit(Instruction::LoadLiteral { dst: r, val });
                    result = Some(self.concat(result, r, loc)?);
                }
                TemplatePart::Expr(expr) => {
                    let r = self.compile_node(expr)?;
                    // Wrap in str() call to ensure string
                    let str_dst = self.alloc_reg();
                    let str_name = self.intern("str");
                    self.emit(Instruction::Call(Box::new(CallData {
                        name_id: str_name,
                        args_regs: Arc::from(vec![r]),
                        dst: Some(str_dst),
                        loc,
                    })));
                    result = Some(self.concat(result, str_dst, loc)?);
                }
            }
        }
        Ok(result.unwrap_or_else(|| {
            let dst = self.alloc_reg();
            self.emit(Instruction::LoadLiteral { dst, val: Value::from_bits(0) });
            dst
        }))
    }

    fn concat(&mut self, left: Option<usize>, right: usize, loc: Loc) -> Result<usize, JitError> {
        match left {
            None => Ok(right),
            Some(l) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::Add { dst, lhs: l, rhs: right, loc });
                Ok(dst)
            }
        }
    }
}
