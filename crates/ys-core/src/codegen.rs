//! Code generation: walk the optimised AST and emit bytecode instructions.
//!
//! The [`Codegen`] struct maintains variable-to-register mappings and
//! accumulates a [`Program`], matching the same output format as the
//! original single-pass parser in [`parser.rs`].
//!
//! # Sub-modules
//!
//! Most compilation logic lives in sibling modules under `codegen/`:
//!
//! * [`expr`]   — template literals, function calls, short-circuit operators
//! * [`stmt`]   — assignment, if/while/for control flow
//! * [`func`]   — function, closure, and async function codegen

pub mod expr;
pub mod func;
pub mod stmt;

use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::ast::*;
use crate::compiler::*;
use crate::error::JitError;

//  Variable tracking

#[derive(Debug, Clone, Copy)]
struct VarInfo {
    idx: usize,
    is_global: bool,
}

//  Codegen

pub struct Codegen {
    /// Local variables per function: name → register / global info.
    locals: FxHashMap<String, VarInfo>,
    globals: FxHashMap<String, VarInfo>,
    next_reg: usize,
    next_global: usize,

    /// A registry of freed register indices that can be reused.
    freed_regs: Vec<usize>,
    /// Bitmask of registers that are variable bindings (locals, params,
    /// loop vars).  `free_reg()` checks this before recycling.
    var_mask: u64,

    /// Accumulated main‑program instructions.
    instructions: Vec<Instruction>,

    /// Functions collected during codegen (including the current one).
    functions: Vec<UserFunction>,
    /// Name → index in `functions` for duplicate detection.
    function_map: FxHashMap<String, usize>,

    /// Interned string pool.
    string_pool: Vec<Arc<str>>,
    string_map: FxHashMap<Arc<str>, u32>,

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

    /// Function parameter lists for named-argument resolution at call sites.
    /// Maps function name → ordered parameter list with defaults.
    fn_params: FxHashMap<String, Vec<FuncParam>>,
    /// Set of function names that have been decorated.  Calls to these
    /// functions use dynamic dispatch so the decorator's result is invoked.
    decorated_fns: FxHashSet<String>,
}

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

impl Codegen {
    pub fn new() -> Self {
        Self {
            locals: FxHashMap::default(),
            globals: FxHashMap::default(),
            next_reg: 0,
            next_global: 0,
            instructions: Vec::new(),
            functions: Vec::new(),
            function_map: FxHashMap::default(),
            string_pool: Vec::new(),
            string_map: FxHashMap::default(),
            is_in_function: false,
            declared_errors: FxHashSet::default(),
            current_error_kind: None,
            current_fail_kinds: Vec::new(),
            freed_regs: Vec::new(),
            var_mask: 0,
            closure_counter: 0,
            fn_params: FxHashMap::default(),
            decorated_fns: FxHashSet::default(),
        }
    }

    //  Helpers

    fn alloc_reg(&mut self) -> usize {
        
        self.freed_regs.pop().unwrap_or_else(|| {
            let r = self.next_reg;
            self.next_reg += 1;
            r
        })
    }

    /// Mark a register as dead so it can be reused by the next allocation.
    /// Variable-binding registers (params, locals, loop vars) are tracked
    /// in `var_mask` and are never recycled.
    #[inline(always)]
    fn free_reg(&mut self, reg: usize) {
        if reg < 64 && (self.var_mask & (1 << reg)) != 0 {
            return; // Variable binding — don't recycle
        }
        self.freed_regs.push(reg);
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.string_map.get(s) {
            return id;
        }
        let id = self.string_pool.len() as u32;
        let arc: Arc<str> = Arc::from(s);
        self.string_pool.push(arc.clone());
        self.string_map.insert(arc, id);
        id
    }

    fn get_var(&self, name: &str) -> Option<VarInfo> {
        self.locals
            .get(name)
            .copied()
            .or_else(|| self.globals.get(name).copied())
    }

    fn ensure_var(&mut self, name: &str) -> VarInfo {
        if let Some(info) = self.get_var(name) {
            return info;
        }
        let is_global = !self.is_in_function;
        let idx = if is_global {
            let i = self.next_global;
            self.next_global += 1;
            i
        } else {
            let r = self.alloc_reg();
            self.var_mask |= 1 << r;
            r
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
            self.emit(Instruction::LoadGlobal {
                dst: r,
                global: info.idx,
            });
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
        let last_reg = cg.compile_block(&ast)?;
        // If the last expression left a value in a register and there is no
        // explicit Return, emit one so exec() returns the expression result.
        if let Some(reg) = last_reg {
            if !matches!(cg.instructions.last(), Some(Instruction::Return { .. })) {
                cg.emit(Instruction::Return { value: Some(reg), loc: Loc::ZERO });
            }
        }
        Ok(Program {
            instructions: Arc::from(cg.instructions),
            functions: Arc::from(cg.functions),
            string_pool: Arc::from(cg.string_pool),
            locals_count: cg.next_reg,
            globals_count: cg.next_global,
        })
    }

    /// Compile an existing AST block (used by the CLI/REPL).
    pub fn compile_ast(&mut self, ast: &AstBlock) -> Result<Program, JitError> {
        let _ = self.compile_block(ast)?;
        Ok(Program {
            instructions: Arc::from(std::mem::take(&mut self.instructions)),
            functions: Arc::from(std::mem::take(&mut self.functions)),
            string_pool: Arc::from(std::mem::take(&mut self.string_pool)),
            locals_count: self.next_reg,
            globals_count: self.next_global,
        })
    }

    //  Block

    fn compile_block(&mut self, block: &[AstNode]) -> Result<Option<usize>, JitError> {
        let mut last_reg = None;
        for node in block {
            last_reg = Some(self.compile_node(node)?);
        }
        Ok(last_reg)
    }

    /// Return `true` when the AST node is guaranteed to produce a number,
    /// used to decide between `AddNumFast` (no checks) and `AddNum` (with
    /// failure/string fallback) for the `+` operator.
    fn is_numeric_expr(node: &AstNode) -> bool {
        match node {
            AstNode::Number(..) => true,
            AstNode::Binary { op, lhs, rhs, .. } => match op {
                BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => true,
                BinOp::Add => Self::is_numeric_expr(lhs) && Self::is_numeric_expr(rhs),
                _ => false,
            },
            AstNode::Unary { op: UnaryOp::Neg, expr, .. } => Self::is_numeric_expr(expr),
            _ => false,
        }
    }

    //  Node dispatch

    fn compile_node(&mut self, node: &AstNode) -> Result<usize, JitError> {
        match node {
            //  Literals
            AstNode::Number(n, loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral {
                    dst,
                    val: Value::number(*n),
                    loc: *loc,
                });
                Ok(dst)
            }
            AstNode::Bool(b, loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral {
                    dst,
                    val: Value::bool(*b),
                    loc: *loc,
                });
                Ok(dst)
            }
            AstNode::Nil(loc) => {
                let dst = self.alloc_reg();
                self.emit(Instruction::LoadLiteral {
                    dst,
                    val: Value::nil(),
                    loc: *loc,
                });
                Ok(dst)
            }
            AstNode::Str(s, loc) => {
                let dst = self.alloc_reg();
                let val = Value::sso(s).unwrap_or_else(|| Value::pool(self.intern(s)));
                self.emit(Instruction::LoadLiteral {
                    dst,
                    val,
                    loc: *loc,
                });
                Ok(dst)
            }
            AstNode::Template { parts, loc } => expr::compile_template(self, parts, *loc),

            //  Variables
            AstNode::Ident(name, loc) => {
                if let Some(info) = self.get_var(name) {
                    Ok(self.load_var(info))
                } else {
                    // Unknown identifier — check for similar names and error
                    let msg = format!("'{}' is not defined", name);
                    Err(JitError::unknown_variable(msg, loc.as_error_pos()))
                }
            }

            //  Assignment
            AstNode::Assign { target, value, loc } => {
                stmt::compile_assign(self, target, value, *loc)
            }

            //  Binary ops
            AstNode::Binary { op, lhs, rhs, loc } => {
                let l = self.compile_node(lhs)?;
                let r = self.compile_node(rhs)?;
                let dst = self.alloc_reg();
                let instr = match op {
                    // When both operands are provably numeric emit the
                    // unchecked AddNumFast, saving the failure + string checks.
                    BinOp::Add if Self::is_numeric_expr(lhs) && Self::is_numeric_expr(rhs) => {
                        Instruction::AddNumFast { dst, lhs: l, rhs: r, loc: *loc }
                    }
                    BinOp::Add => Instruction::AddNum {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Sub => Instruction::Sub {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Mul => Instruction::Mul {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Div => Instruction::Div {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Mod => Instruction::Mod {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Eq => Instruction::Eq {
                        dst,
                        lhs: l,
                        rhs: r,
                    },
                    BinOp::Ne => Instruction::Ne {
                        dst,
                        lhs: l,
                        rhs: r,
                    },
                    BinOp::Lt => Instruction::Lt {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Le => Instruction::Le {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Gt => Instruction::Gt {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::Ge => Instruction::Ge {
                        dst,
                        lhs: l,
                        rhs: r,
                        loc: *loc,
                    },
                    BinOp::And | BinOp::Or => {
                        return expr::compile_short_circuit(self, *op, l, r, *loc);
                    }
                };
                self.emit(instr);
                self.free_reg(l);
                if l != r {
                    self.free_reg(r);
                }
                Ok(dst)
            }

            //  Unary ops
            AstNode::Unary { op, expr, loc } => {
                let src = self.compile_node(expr)?;
                match op {
                    UnaryOp::Neg => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Neg {
                            dst,
                            src,
                            loc: *loc,
                        });
                        self.free_reg(src);
                        Ok(dst)
                    }
                    UnaryOp::Not => {
                        let dst = self.alloc_reg();
                        self.emit(Instruction::Not {
                            dst,
                            src,
                            loc: *loc,
                        });
                        self.free_reg(src);
                        Ok(dst)
                    }
                }
            }

            //  Control flow
            AstNode::Block(stmts, _) => {
                let _ = self.compile_block(stmts)?;
                Ok(0)
            }
            AstNode::If {
                cond,
                then_block,
                else_block,
                loc,
            } => stmt::compile_if(self, cond, then_block, else_block, *loc),
            AstNode::While { cond, body, loc } => stmt::compile_while(self, cond, body, *loc),
            AstNode::For {
                var,
                iter,
                body,
                loc,
            } => stmt::compile_for(self, var, iter, body, *loc),
            AstNode::Return { value, loc } => {
                let reg = match value {
                    Some(expr) => Some(self.compile_node(expr)?),
                    None => None,
                };
                self.emit(Instruction::Return {
                    value: reg,
                    loc: *loc,
                });
                Ok(0)
            }
            AstNode::Yield(expr, loc) => {
                let value_reg = self.compile_node(expr)?;
                self.emit(Instruction::Yield {
                    dst: 0,
                    value: value_reg,
                    gen_reg: 0,
                    loc: *loc,
                });
                // After yield, the function suspends and returns via Yield instruction.
                // The compiler emits Return(0) as a safety net, but the actual return
                // happens inside the Yield instruction (which pops the frame). This
                // Return should never be reached — it's dead code after Yield.
                Ok(0)
            }

            //  Switch
            AstNode::Switch { expr, arms, .. } => {
                let val_reg = self.compile_node(expr)?;
                let mut fail_jumps: Vec<(usize, usize)> = Vec::new(); // pattern mismatches
                let mut end_jumps: Vec<usize> = Vec::new(); // arm bodies → exit
                for arm in arms {
                    // For each pattern: check if val matches, skip arm if not
                    for pattern in &arm.patterns {
                        let pat_reg = self.compile_node(pattern)?;
                        let eq_reg = self.alloc_reg();
                        self.emit(Instruction::Eq {
                            dst: eq_reg,
                            lhs: val_reg,
                            rhs: pat_reg,
                        });
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
                            self.instructions[fail_idx] = Instruction::JumpIfFalse {
                                cond: eq_reg,
                                target: next_arm,
                            };
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
            AstNode::AsyncFun {
                name,
                params,
                body,
                loc,
            } => {
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                func::compile_async_func(self, name, &pn, body, *loc);
                Ok(0)
            }
            AstNode::Await(expr, loc) => {
                let promise_reg = self.compile_node(expr)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Await {
                    dst,
                    promise: promise_reg,
                    loc: *loc,
                });
                self.free_reg(promise_reg);
                Ok(dst)
            }

            //  Expression-level spread — evaluate the inner expr
            //  (actual unpacking is handled in compile_fun_call's arg processing)
            AstNode::Splat(inner, _) => self.compile_node(inner),

            //  Calls
            AstNode::FunCall { name, args, named, loc } => {
                // Optimise `range |> step(n)` — emit Range with step directly.
                if name == "step" && args.len() == 2
                    && let AstNode::Range {
                        start,
                        end,
                        step: _existing_step,
                        ..
                    } = &args[0]
                    {
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
                        self.free_reg(start_r);
                        self.free_reg(end_r);
                        self.free_reg(step_r);
                        return Ok(dst);
                    }
                expr::compile_fun_call(self, name, args, &named, *loc)
            }
            AstNode::DynamicCall { callee, args, named: _, loc } => {
                let callee_r = self.compile_node(callee)?;
                let args_r = expr::compile_args(self, args)?;
                let dst = self.alloc_reg();
                for &r in &args_r {
                    self.free_reg(r);
                }
                self.free_reg(callee_r);
                self.emit(Instruction::CallDynamic(CallDynamicData {
                    callee_reg: callee_r,
                    args_regs: Arc::from(args_r),
                    dst: Some(dst),
                    loc: *loc,
                }));
                Ok(dst)
            }

            //  Decorators — compile the inner function, then call the decorator.
            //  The decorator receives the function NAME (pool string) as its
            //  first argument, plus any additional decorator args.
            AstNode::Decorator { name: dec_name, args, named: _, inner, loc } => {
                let fn_name = match inner.as_ref() {
                    AstNode::FunDecl { name, .. } => name.clone(),
                    AstNode::AsyncFun { name, .. } => name.clone(),
                    _ => return Err(JitError::runtime(
                        "Decorator target must be a function declaration",
                        loc.as_error_pos(),
                    )),
                };
                // Mark as decorated so external calls use dynamic dispatch
                // (loading the decorator's result from the global variable).
                self.decorated_fns.insert(fn_name.clone());

                // 1. Compile the inner function as normal
                self.compile_node(inner)?;

                // 2. Create a closure for the decorated function (so the
                //    decorator can actually CALL it, not just see its name).
                let fn_name_id = self.intern(&fn_name);
                let fn_reg = self.alloc_reg();
                self.emit(Instruction::MakeClosure {
                    dst: fn_reg,
                    name_id: fn_name_id,
                    captures: Arc::from([]),
                });

                // 3. Ensure the global variable exists and store the ORIGINAL
                //    closure first (so recursive calls work during the decorator).
                let info = self.ensure_var(&fn_name);
                if info.is_global {
                    self.emit(Instruction::StoreGlobal { global: info.idx, src: fn_reg });
                }

                // 4. Compile decorator positional arguments
                let mut arg_regs: Vec<usize> = vec![fn_reg];
                for a in args {
                    let r = self.compile_node(a)?;
                    arg_regs.push(r);
                }
                for &r in &arg_regs[1..] { self.free_reg(r); }
                self.free_reg(fn_reg);

                // 5. Call the decorator
                let dst_reg = self.alloc_reg();
                let dec_name_id = self.intern(dec_name);
                self.emit(Instruction::Call(CallData {
                    name_id: dec_name_id,
                    args_regs: Arc::from(arg_regs),
                    dst: Some(dst_reg),
                    loc: *loc,
                }));

                // 6. Overwrite with the decorator's result.
                if info.is_global {
                    self.emit(Instruction::StoreGlobal { global: info.idx, src: dst_reg });
                }
                self.free_reg(dst_reg);
                Ok(0)
            }

            //  Functions & closures
            AstNode::FunDecl {
                name,
                params,
                body,
                exported: _,
                loc,
                error_kind,
            } => {
                // Save outer state
                let old_error_kind = self.current_error_kind.take();
                let old_fail_kinds = std::mem::take(&mut self.current_fail_kinds);

                self.current_error_kind = error_kind.clone();

                // Compile the function body
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let rest_at = params.iter().position(|p| p.is_rest);
                let kwargs_at = params.iter().position(|p| p.is_kwargs);
                func::compile_func(self, name, &pn, body, *loc, rest_at, kwargs_at);

                // Infer error kind from collected fail calls
                if !self.current_fail_kinds.is_empty() {
                    let first = &self.current_fail_kinds[0];
                    let base = first.split('.').next().unwrap_or(first);
                    for path in &self.current_fail_kinds {
                        let b = path.split('.').next().unwrap_or(path);
                        if b != base {
                            return Err(JitError::parsing(
                                format!(
                                    "Function '{}' mixes error kinds '{}' and '{}'",
                                    name, base, b
                                ),
                                loc.as_error_pos(),
                            ));
                        }
                    }
                }

                // Register function params for named argument resolution
                self.fn_params.insert(name.clone(), params.clone());

                // Restore outer state
                self.current_error_kind = old_error_kind;
                self.current_fail_kinds = old_fail_kinds;

                Ok(0)
            }
            AstNode::Closure {
                params,
                body,
                is_move: _,
                loc,
            } => {
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let dst = func::compile_closure(self, &pn, body, *loc)?;
                Ok(dst)
            },

            //  Collections
            AstNode::ListLit(elems, _) => {
                let dst = self.alloc_reg();
                if elems.is_empty() {
                    self.emit(Instruction::NewList { dst, len: 0 });
                } else {
                    let regs: Vec<usize> = elems
                        .iter()
                        .map(|e| self.compile_node(e))
                        .collect::<Result<_, _>>()?;
                    for &r in &regs {
                        self.free_reg(r);
                    }
                    self.emit(Instruction::NewListFrom {
                        dst,
                        elems: Arc::from(regs),
                    });
                }
                Ok(dst)
            }
            AstNode::ListRepeat { val, count, loc: _ } => {
                let val_r = self.compile_node(val)?;
                let count_r = self.compile_node(count)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::NewListRepeat {
                    dst,
                    val: val_r,
                    count: count_r,
                });
                self.free_reg(val_r);
                self.free_reg(count_r);
                Ok(dst)
            }
            AstNode::ObjectLit(fields, _) => {
                let dst = self.alloc_reg();
                if fields.is_empty() {
                    self.emit(Instruction::NewObject { dst, capacity: 0 });
                } else {
                    let pairs: Vec<(u32, usize)> = fields
                        .iter()
                        .map(|(name, val)| {
                            let r = self.compile_node(val)?;
                            Ok((self.intern(name), r))
                        })
                        .collect::<Result<_, JitError>>()?;
                    for &(_, r) in &pairs {
                        self.free_reg(r);
                    }
                    self.emit(Instruction::NewObjectFrom {
                        dst,
                        fields: Arc::from(pairs),
                    });
                }
                Ok(dst)
            }
            AstNode::Index { obj, index, loc } => {
                let obj_r = self.compile_node(obj)?;
                let idx_r = self.compile_node(index)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::ListGet {
                    dst,
                    list: obj_r,
                    index_reg: idx_r,
                    loc: *loc,
                });
                self.free_reg(obj_r);
                self.free_reg(idx_r);
                Ok(dst)
            }
            AstNode::Field { obj, name, loc } => {
                let obj_r = self.compile_node(obj)?;
                let dst = self.alloc_reg();
                let name_id = self.intern(name);
                self.emit(Instruction::ObjectGet {
                    dst,
                    obj: obj_r,
                    name_id,
                    loc: *loc,
                });
                self.free_reg(obj_r);
                Ok(dst)
            }

            //  Ranges
            AstNode::Range {
                start,
                end,
                step,
                loc,
            } => {
                let start_r = self.compile_node(start)?;
                let end_r = self.compile_node(end)?;
                let step_r = step.as_ref().map(|s| self.compile_node(s)).transpose()?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Range {
                    dst,
                    start: start_r,
                    end: end_r,
                    step: step_r,
                    loc: *loc,
                });
                self.free_reg(start_r);
                self.free_reg(end_r);
                if let Some(s) = step_r {
                    self.free_reg(s);
                }
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
            AstNode::Fallback {
                expr,
                default,
                loc: _,
            } => {
                let left_r = self.compile_node(expr)?;
                let dst = self.alloc_reg();
                self.emit(Instruction::Move { dst, src: left_r });
                let jump_idx = self.instructions.len();
                self.emit(Instruction::Jump(0)); // placeholder → JumpIfNotFail
                let right_r = self.compile_node(default)?;
                self.emit(Instruction::Move { dst, src: right_r });
                let end = self.instructions.len();
                self.instructions[jump_idx] = Instruction::JumpIfNotFail {
                    src: left_r,
                    target: end,
                };
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
                        self.emit(Instruction::LoadLiteral {
                            dst: lit_r,
                            val: Value::failure(name_id),
                            loc: *loc,
                        });
                        let eq_r = self.alloc_reg();
                        self.emit(Instruction::Eq {
                            dst: eq_r,
                            lhs: val_r,
                            rhs: lit_r,
                        });
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
                                self.instructions[next_arm_idx] = Instruction::JumpIfFalse {
                                    cond: eq_r,
                                    target: next_arm_pos,
                                };
                            }
                            _ => unreachable!(),
                        }
                    }
                }

                // Patch skip_idx to JumpIfNotFail → end
                let end_pos = self.instructions.len();
                self.instructions[skip_idx] = Instruction::JumpIfNotFail {
                    src: val_r,
                    target: end_pos,
                };

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
                        loc.as_error_pos(),
                    ));
                }
                self.intern(name);
                Ok(0)
            }
            AstNode::ErrorEnum {
                name,
                variants,
                loc,
            } => {
                for v in variants {
                    let path = format!("{}.{}", name, v);
                    if !self.declared_errors.insert(path.clone()) {
                        return Err(JitError::parsing(
                            format!("Duplicate error kind '{}'", path),
                            loc.as_error_pos(),
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
}
