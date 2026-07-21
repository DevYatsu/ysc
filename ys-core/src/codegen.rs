//! Code generation: walk the optimised AST and emit bytecode instructions.
//!
//! The [`Codegen`] struct maintains variable-to-register mappings and
//! accumulates a [`Program`], matching the same output format as the
//! original single-pass parser in [`parser.rs`].

use std::sync::Arc;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;

//  Variable tracking

#[derive(Debug, Clone, Copy)]
struct VarInfo {
    idx:       usize,
    is_global: bool,
}

//  Codegen

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

    /// Set of declared error kind full paths — for duplicate detection.
    declared_errors: FxHashSet<String>,
    /// Error kind of the function currently being compiled (None = no ! annotation).
    current_error_kind: Option<String>,
    /// All fail full-paths seen in the current function body.
    current_fail_kinds: Vec<String>,

    /// Monotonically increasing counter for unique closure naming across all
    /// nesting levels.  Copied from parent when creating nested codegens.
    closure_counter: usize,
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
            declared_errors: FxHashSet::default(),
            current_error_kind: None,
            current_fail_kinds: Vec::new(),
            closure_counter: 0,
        }
    }

    //  Helpers

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

    //  Compile a complete program

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

    //  Block

    fn compile_block(&mut self, block: &[AstNode]) -> Result<(), JitError> {
        for node in block {
            self.compile_node(node)?;
        }
        Ok(())
    }

    //  Node dispatch

    fn compile_node(&mut self, node: &AstNode) -> Result<usize, JitError> {
        match node {
            //  Literals
            AstNode::Number(n, loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::number(*n), loc: *loc });
                Ok(dst)
            }
            AstNode::Bool(b, loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::bool(*b), loc: *loc });
                Ok(dst)
            }
            AstNode::Nil(loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst, val: Value::from_bits(0), loc: *loc });
                Ok(dst)
            }
            AstNode::Str(s, loc) => {
                let dst = self.alloc_reg();
                let val = Value::sso(s)
                    .unwrap_or_else(|| Value::pool(self.intern(s)));
                self.emit(Instruction::LoadLiteral { dst, val, loc: *loc });
                Ok(dst)
            }
            AstNode::Template { parts, loc } => {
                self.compile_template(parts, *loc)
            }

            //  Variables
            AstNode::Ident(name, loc) => {
                if let Some(info) = self.get_var(name) {
                    Ok(self.load_var(info))
                } else {
                    // Unknown identifier — check for similar names and error
                    let msg = format!("'{}' is not defined", name);
                    Err(JitError::unknown_variable(msg, loc.line as usize, loc.col as usize))
                }
            }

            //  Assignment
            AstNode::Assign { target, value, loc } => {
                self.compile_assign(target, value, *loc)
            }

            //  Binary ops
            AstNode::Binary { op, lhs, rhs, loc } => {
                let l = self.compile_node(lhs)?;
                let r = self.compile_node(rhs)?;
                let dst = self.alloc_reg();
                let instr = match op {
                    BinOp::Add => Instruction::Add   { dst, lhs: l, rhs: r, loc: *loc },
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

            //  Unary ops
            AstNode::Unary { op, expr, loc } => {
                let src = self.compile_node(expr)?;
                match op {
                    UnaryOp::Neg => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Neg { dst, src, loc: *loc });
                        Ok(dst)
                    }
                    UnaryOp::Not => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Not { dst, src, loc: *loc });
                        Ok(dst)
                    }
                }
            }

            //  Control flow
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
            AstNode::Return { value, loc } => {
                let reg = match value {
                    Some(expr) => Some(self.compile_node(expr)?),
                    None => None,
                };
                self.emit(Instruction::Return { value: reg, loc: *loc });
                Ok(0)
            }
            AstNode::Yield(expr, loc) => {
                let value_reg = self.compile_node(expr)?;
                self.emit(Instruction::Yield { dst: 0, value: value_reg, gen_reg: 0, loc: *loc });
                // After yield, the function suspends and returns via Yield instruction.
                // The compiler emits Return(0) as a safety net, but the actual return
                // happens inside the Yield instruction (which pops the frame). This
                // Return should never be reached — it's dead code after Yield.
                Ok(0)
            }

            //  Switch
            AstNode::Switch { expr, arms, .. } => {
                let val_reg = self.compile_node(expr)?;
                let mut fail_jumps: Vec<(usize, usize)> = Vec::new();  // pattern mismatches
                let mut end_jumps: Vec<usize> = Vec::new();   // arm bodies → exit
                for arm in arms {
                    // For each pattern: check if val matches, skip arm if not
                    for pattern in &arm.patterns {
                        let pat_reg = self.compile_node(pattern)?;
                        let eq_reg = self.alloc_reg();
                        self.emit(Instruction::Eq { dst: eq_reg, lhs: val_reg, rhs: pat_reg });
                        let fail_idx = self.instructions.len();
                        self.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse
                        fail_jumps.push((fail_idx, eq_reg));
                    }
                    // Default arm (empty patterns) — no check, always enter
                    // Compile arm body
                    for stmt in &arm.body {
                        self.compile_node(stmt)?;
                    }
                    // Jump to end of switch (skip later arms)
                    let end_jump = self.instructions.len();
                    self.emit(Instruction::Jump(0));
                    end_jumps.push(end_jump);
                    // Patch fail jumps for this arm to skip to next arm
                    for &(fail_idx, eq_reg) in &fail_jumps {
                        let next_arm = self.instructions.len();
                        if let Instruction::Jump(_) = &mut self.instructions[fail_idx] {
                            self.instructions[fail_idx] = Instruction::JumpIfFalse { cond: eq_reg, target: next_arm };
                        }
                    }
                    fail_jumps.clear();
                }
                // Patch all end jumps to jump to after the switch
                let switch_end = self.instructions.len();
                for &j in &end_jumps {
                    if let Instruction::Jump(_) = &mut self.instructions[j] {
                        self.instructions[j] = Instruction::Jump(switch_end);
                    }
                }
                Ok(0)
            }
            AstNode::Break(_) => {
                // Break → jump to end of switch. Gets patched by the outer switch handler.
                self.emit(Instruction::Jump(0));
                Ok(0)
            }
            //  Async / Await
            AstNode::AsyncFun { name, params, body, loc } => {
                self.compile_async_func(name, params, body, *loc);
                Ok(0)
            }
            AstNode::Await(expr, loc) => {
                let promise_reg = self.compile_node(expr)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Await { dst, promise: promise_reg, loc: *loc });
                Ok(dst)
            }

            //  Calls
            AstNode::FunCall { name, args, loc } => {
                // Optimise `range |> step(n)` — emit Range with step directly.
                if name == "step" && args.len() == 2 {
                    if let AstNode::Range { start, end, step: _existing_step, .. } = &args[0] {
                        let start_r = self.compile_node(start)?;
                        let end_r = self.compile_node(end)?;
                        let step_r = self.compile_node(&args[1])?;
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Range {
                            dst,
                            start: start_r,
                            end: end_r,
                            step: Some(step_r),
                            loc: *loc,
                        });
                        return Ok(dst);
                    }
                }
                self.compile_fun_call(name, args, *loc)
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

            //  Functions & closures
            AstNode::FunDecl { name, params, body, exported: _, loc, error_kind } => {
                // Save outer state
                let old_error_kind = self.current_error_kind.take();
                let old_fail_kinds = std::mem::take(&mut self.current_fail_kinds);

                self.current_error_kind = error_kind.clone();

                // Compile the function body
                self.compile_func(name, params, body, *loc);

                // Infer error kind from collected fail calls
                if !self.current_fail_kinds.is_empty() {
                    let first = &self.current_fail_kinds[0];
                    let base = first.split('.').next().unwrap_or(first);
                    for path in &self.current_fail_kinds {
                        let b = path.split('.').next().unwrap_or(path);
                        if b != base {
                            return Err(JitError::parsing(
                                format!("Function '{}' mixes error kinds '{}' and '{}'", name, base, b),
                                loc.line as usize, loc.col as usize,
                            ));
                        }
                    }
                }

                // Restore outer state
                self.current_error_kind = old_error_kind;
                self.current_fail_kinds = old_fail_kinds;

                Ok(0)
            }
            AstNode::Closure { params, body, is_move: _, loc } => {
                self.compile_closure(params, body, *loc)
            }

            //  Collections
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
            AstNode::ListRepeat { val, count, loc: _ } => {
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

            //  Ranges
            AstNode::Range { start, end, step, loc } => {
                let start_r = self.compile_node(start)?;
                let end_r = self.compile_node(end)?;
                let step_r = step.as_ref().map(|s| self.compile_node(s)).transpose()?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Range { dst, start: start_r, end: end_r, step: step_r, loc: *loc });
                Ok(dst)
            }

            //  Failure handling
            AstNode::Fail { type_name, loc: _ } => {
                let dst = self.alloc_reg();
                // Resolve short-form: if no dot and current_error_kind is set, prepend kind
                let full_name = if type_name.contains('.') {
                    type_name.clone()
                } else if let Some(ref kind) = self.current_error_kind {
                    format!("{}.{}", kind, type_name)
                } else {
                    type_name.clone()
                };

                self.current_fail_kinds.push(full_name.clone());

                let name_id = self.intern(&full_name);
                self.emit(Instruction::Fail { dst, name_id });
                Ok(dst)
            }
            AstNode::Fallback { expr, default, loc: _ } => {
                let left_r = self.compile_node(expr)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Move { dst, src: left_r });
                let jump_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder → JumpIfNotFail
                let right_r = self.compile_node(default)?;
                self.emit(Instruction::Move { dst, src: right_r });
                let end = self.instructions.len();
                self.instructions[jump_idx] = Instruction::JumpIfNotFail { src: left_r, target: end };
                Ok(dst)
            }
            AstNode::Except { expr, arms, loc } => {
                let val_r = self.compile_node(expr)?;
                let dst = self.alloc_reg();

                // Move the value to dst (may be failure or normal value)
                self.emit(Instruction::Move { dst, src: val_r });

                // Jump to end if NOT a failure (success path)
                let skip_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder → JumpIfNotFail

                let mut end_jumps: Vec<usize> = Vec::new();
                for arm in arms {
                    if arm.type_name.is_empty() {
                        // Default arm (_): execute body
                        for stmt in &arm.body {
                            self.compile_node(stmt)?;
                        }
                        // Body's last expression value should be in dst
                        if let Some(last) = arm.body.last() {
                            let last_r = self.compile_node(last)?;
                            self.emit(Instruction::Move { dst, src: last_r });
                        }
                        let end_jump = self.instructions.len();
                        self.emit(Instruction::Jump(0));
                        end_jumps.push(end_jump);
                    } else {
                        // Named arm: check if failure matches this type
                        let name_id = self.intern(&arm.type_name);
                        // Compare the runtime failure value against a constructed failure literal
                        let lit_r = self.alloc_reg();
                        self.emit(Instruction::LoadLiteral { dst: lit_r, val: Value::failure(name_id), loc: *loc });
                        let eq_r = self.alloc_reg();
                        self.emit(Instruction::Eq { dst: eq_r, lhs: val_r, rhs: lit_r });
                        let next_arm_idx = self.instructions.len();
                        self.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse to next arm
                        for stmt in &arm.body {
                            self.compile_node(stmt)?;
                        }
                        if let Some(last) = arm.body.last() {
                            let last_r = self.compile_node(last)?;
                            self.emit(Instruction::Move { dst, src: last_r });
                        }
                        let end_jump = self.instructions.len();
                        self.emit(Instruction::Jump(0));
                        end_jumps.push(end_jump);
                        // Patch the JumpIfFalse to point here (next arm)
                        let next_arm_pos = self.instructions.len();
                        match &mut self.instructions[next_arm_idx] {
                            Instruction::Jump(_) => {
                                self.instructions[next_arm_idx] = Instruction::JumpIfFalse { cond: eq_r, target: next_arm_pos };
                            }
                            _ => unreachable!(),
                        }
                    }
                }

                // Patch skip_idx to JumpIfNotFail → end
                let end_pos = self.instructions.len();
                self.instructions[skip_idx] = Instruction::JumpIfNotFail { src: val_r, target: end_pos };

                // Patch all end_jumps to → end_pos
                for &j in &end_jumps {
                    match &mut self.instructions[j] {
                        Instruction::Jump(_) => {
                            self.instructions[j] = Instruction::Jump(end_pos);
                        }
                        _ => unreachable!(),
                    }
                }

                Ok(dst)
            }

            //  Error declarations
            AstNode::ErrorDecl { name, loc } => {
                if !self.declared_errors.insert(name.clone()) {
                    return Err(JitError::parsing(
                        format!("Duplicate error kind '{}'", name),
                        loc.line as usize, loc.col as usize,
                    ));
                }
                self.intern(name);
                Ok(0)
            }
            AstNode::ErrorEnum { name, variants, loc } => {
                for v in variants {
                    let path = format!("{}.{}", name, v);
                    if !self.declared_errors.insert(path.clone()) {
                        return Err(JitError::parsing(
                            format!("Duplicate error kind '{}'", path),
                            loc.line as usize, loc.col as usize,
                        ));
                    }
                    self.intern(&path);
                }
                Ok(0)
            }

            //  Modules — not yet implemented in runtime, compile as no-op
            AstNode::Use { .. } => Ok(0),
        }
    }

    //  Assignment

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

    //  Short-circuit and/or

    fn compile_short_circuit(&mut self, op: BinOp, l: usize, r: usize, _loc: Loc) -> Result<usize, JitError> {
        let dst = self.alloc_reg();
        match op {
            BinOp::And => {
                // a && b: if a is falsy → short-circuit (result = a), else evaluate b
                self.emit(Instruction::Move { dst, src: l });
                let jump_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder
                self.emit(Instruction::Move { dst, src: r });
                let end = self.instructions.len();
                self.instructions[jump_idx] = Instruction::JumpIfFalse { cond: l, target: end };
            }
            BinOp::Or => {
                // a || b: if a is truthy → short-circuit (result = a), else evaluate b
                // Only have JumpIfFalse, so invert: if l is falsy, evaluate r
                self.emit(Instruction::Move { dst, src: l });
                let jump_false_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder → JumpIfFalse to eval_r
                let jump_end_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder → Jump(end) when truthy
                let eval_r = self.instructions.len();
                self.emit(Instruction::Move { dst, src: r });
                let end = self.instructions.len();
                self.instructions[jump_false_idx] =
                    Instruction::JumpIfFalse { cond: l, target: eval_r };
                self.instructions[jump_end_idx] = Instruction::Jump(end);
            }
            _ => unreachable!(),
        }
        Ok(dst)
    }

    //  If / else

    fn compile_if(&mut self, cond: &AstNode, then_block: &[AstNode], else_block: &[AstNode], _loc: Loc) -> Result<usize, JitError> {
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

    //  While loop

    fn compile_while(&mut self, cond: &AstNode, body: &[AstNode], _loc: Loc) -> Result<usize, JitError> {
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

    //  For loop

    fn compile_for(&mut self, var: &str, iter: &AstNode, body: &[AstNode], loc: Loc) -> Result<usize, JitError> {
        let iter_r = self.compile_node(iter)?;

        // Build a Range object at runtime if it's a compile-time Range AST node
        // so that ForNext can handle it uniformly with lists and objects.
        let iter_reg = if let AstNode::Range { start, end, step, .. } = iter {
            let s = self.compile_node(start)?;
            let e = self.compile_node(end)?;
            let st = match step {
                Some(sn) => self.compile_node(sn)?,
                None => {
                    let r = self.alloc_reg();
                    self.emit(Instruction::LoadLiteral { dst: r, val: Value::number(1.0), loc });
                    r
                }
            };
            let dst = self.alloc_reg();
            self.emit(Instruction::Range { dst, start: s, end: e, step: Some(st), loc });
            dst
        } else {
            iter_r
        };

        // Index register (starts at 0, incremented each iteration by ForNext)
        let idx_reg = self.alloc_reg();
        self.emit(Instruction::LoadLiteral { dst: idx_reg, val: Value::from_bits(0), loc });

        // "Has more" flag register
        let done_reg = self.alloc_reg();
        let var_reg = self.alloc_reg();

        let was_in_fn = self.is_in_function;
        self.is_in_function = true;
        self.locals.insert(var.to_string(), VarInfo { idx: var_reg, is_global: false });

        let loop_start = self.instructions.len();

        // ForNext: dst_val = iterable[idx_reg], dst_done = has_more, idx_reg++
        self.emit(Instruction::ForNext {
            dst_val: var_reg,
            dst_done: done_reg,
            iterable: iter_reg,
            idx_reg,
            loc,
        });

        // If not has_more → exit
        let exit_jump = self.instructions.len();
        self.emit(Instruction::JumpIfFalse { cond: done_reg, target: 0 });

        // Body
        self.compile_block(body)?;

        // Loop back
        self.emit(Instruction::Jump(loop_start));

        // Patch exit jump
        let end_pos = self.instructions.len();
        self.instructions[exit_jump] = Instruction::JumpIfFalse { cond: done_reg, target: end_pos };

        self.locals.remove(var);
        self.is_in_function = was_in_fn;
        Ok(0)
    }

    //  Function calls

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

    //  Function declaration

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
            self.emit(Instruction::Return { value: None, loc });
        }
        if !matches!(self.instructions.last(), Some(Instruction::Return { .. })) {
            self.emit(Instruction::Return { value: None, loc });
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

    /// Compile an async function — creates a pending return promise at the
    /// start so callers immediately get a Promise, even if the body suspends
    /// on an internal await.  The promise is resolved when the body returns.
    fn compile_async_func(&mut self, name: &str, params: &[String], body: &[AstNode], loc: Loc) {
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
        // Register 0..params.len()-1 = params, next = return_promise
        let ret_promise_reg = self.alloc_reg();
        self.emit(Instruction::MakePendingPromise { dst: ret_promise_reg });

        if let Err(_) = self.compile_block(body) {
            self.emit(Instruction::Return { value: None, loc });
        }
        // Replace the final return with ResolvePromise + Return(ret_promise)
        if let Some(Instruction::Return { value: reg, .. }) = self.instructions.pop() {
            if let Some(value_reg) = reg {
                self.emit(Instruction::ResolvePromise { promise: ret_promise_reg, value: value_reg });
            } else {
                // No return value — resolve with nil
                let nil_reg = self.alloc_reg();
                self.emit(Instruction::LoadLiteral { dst: nil_reg, val: Value::from_bits(0), loc });
                self.emit(Instruction::ResolvePromise { promise: ret_promise_reg, value: nil_reg });
            }
            self.emit(Instruction::Return { value: Some(ret_promise_reg), loc });
        } else {
            // No return instruction at all — body ran to end without returning
            let nil_reg = self.alloc_reg();
            self.emit(Instruction::LoadLiteral { dst: nil_reg, val: Value::from_bits(0), loc });
            self.emit(Instruction::ResolvePromise { promise: ret_promise_reg, value: nil_reg });
            self.emit(Instruction::Return { value: Some(ret_promise_reg), loc });
        }
        let name_id = self.intern(name);
        let func_body = std::mem::replace(&mut self.instructions, saved_instrs);
        let locals_count = self.next_reg;

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

    //  Closure

    fn compile_closure(&mut self, params: &[String], body: &AstNode, loc: Loc) -> Result<usize, JitError> {
        let mut func = Codegen::new();
        func.closure_counter = self.closure_counter;
        func.is_in_function = true;
        for (i, p) in params.iter().enumerate() {
            func.locals.insert(p.clone(), VarInfo { idx: i, is_global: false });
            func.next_reg = i + 1;
        }
        let captures: Vec<String> = Vec::new();

        let result_reg = func.compile_node(body).unwrap_or(0);

        for mut nested in std::mem::take(&mut func.functions) {
            let new_name = format!("__closure_{}", self.closure_counter);
            self.closure_counter += 1;
            nested.name_id = self.intern(&new_name);
            self.functions.push(nested);
        }
        self.closure_counter = std::cmp::max(self.closure_counter, func.closure_counter);

        for instr in &mut func.instructions {
            match instr {
                Instruction::MakeClosure { name_id, .. }
                | Instruction::ObjectGet { name_id, .. }
                | Instruction::ObjectSet { name_id, .. } => {
                    if let Some(name) = func.string_pool.get(*name_id as usize) {
                        *name_id = self.intern(name);
                    }
                }
                Instruction::Call(data) => {
                    if let Some(name) = func.string_pool.get(data.name_id as usize) {
                        data.name_id = self.intern(name);
                    }
                }
                _ => {}
            }
        }

        let is_expr_body = !matches!(body, AstNode::Block(_, _));
        if is_expr_body && !matches!(func.instructions.last(), Some(Instruction::Return { .. })) {
            func.emit(Instruction::Return { value: Some(result_reg), loc });
        } else if !matches!(func.instructions.last(), Some(Instruction::Return { .. })) {
            func.emit(Instruction::Return { value: None, loc });
        }

        // Use the shared closure counter for unique naming.
        let closure_name = format!("__closure_{}", self.closure_counter);
        self.closure_counter += 1;
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

    //  Template literals

    fn compile_template(&mut self, parts: &[TemplatePart], loc: Loc) -> Result<usize, JitError> {
        let mut result: Option<usize> = None;
        for part in parts {
            match part {
                TemplatePart::Text(s) => {
                    let r = self.alloc_reg();
                    let val = Value::sso(s)
                        .unwrap_or_else(|| Value::pool(self.intern(s)));
                    self.emit(Instruction::LoadLiteral { dst: r, val, loc });
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
            self.emit(Instruction::LoadLiteral { dst, val: Value::from_bits(0), loc });
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
