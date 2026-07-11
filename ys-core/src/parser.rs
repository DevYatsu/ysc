use crate::{
    compiler::{CallData, CallDynamicData, Instruction, Program, UserFunction, Value},
    error::JitError,
    lexer::Token,
    template::{split_template_parts, TemplatePart},
    unescape::unescape_string,
    token_stream::{TokenStream, VarInfo},
};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// The Parser transforms source code into a compiled Program (bytecode).
///
/// # Grammar
///
/// The parser implements recursive descent with correct precedence:
///
/// ```text
/// expression      = or_expr
/// or_expr         = and_expr ("or" and_expr)*       (short-circuit)
/// and_expr        = comp_expr ("and" comp_expr)*     (short-circuit)
/// comp_expr       = add_expr (("=="|"!="|"<"|"<="|">"|">=") add_expr)*
/// add_expr        = mul_expr (("+"|"-") mul_expr)*
/// mul_expr        = unary_expr (("*"|"/") unary_expr)*
/// unary_expr      = ("!"|"-") unary_expr | postfix_expr
/// postfix_expr    = primary ("(" args ")" | "[" expr "]" | "." identifier)*
/// primary_expr    = literal | identifier | "(" expr ")"
///                 | "[" list_lit "]" | "{" obj_lit "}"
///                 | closure ("move"? "|" params "|" body)
/// ```
pub struct Parser<'source> {
    pub stream: TokenStream<'source>,

    /// Local variables in the current function/scope.
    locals: FxHashMap<&'source str, VarInfo>,
    /// Global variables.
    globals: FxHashMap<&'source str, VarInfo>,

    /// Interned strings (Arc so they outlive `'source` for the runtime).
    strings: Vec<Arc<str>>,
    /// Map for fast interning lookups.
    string_map: FxHashMap<Arc<str>, u32>,

    /// User-defined functions accumulated during compilation.
    functions: Vec<UserFunction>,

    next_reg: usize,
    next_global: usize,

    is_in_function: bool,


    /// Collected `use` paths (just parsed, evaluated later in Task 4).
    uses: Vec<Vec<String>>,
}

impl<'source> Parser<'source> {
    // ═══════════════════════════════════════════════════════════════
    //  Construction & entry-point
    // ═══════════════════════════════════════════════════════════════

    pub fn new(input: &'source str) -> Result<Self, JitError> {
        let tokens = TokenStream::lex_all(input)?;
        Ok(Self {
            stream: TokenStream::new(tokens),
            globals: FxHashMap::default(),
            locals: FxHashMap::default(),
            strings: Vec::with_capacity(64),
            string_map: FxHashMap::default(),
            next_reg: 0,
            next_global: 0,
            is_in_function: false,
            functions: Vec::with_capacity(16),
            uses: Vec::new(),
        })
    }

    pub fn compile(mut self) -> Result<Program, JitError> {
        let mut instructions = Vec::new();
        loop {
            self.stream.skip_newlines();
            if self.stream.peek().is_none() {
                break;
            }
            if let Some(res) = self.parse_statement(&mut instructions) {
                res?;
            } else {
                break;
            }
        }
        Ok(Program {
            instructions: Arc::from(instructions),
            functions: Arc::from(self.functions),
            string_pool: Arc::from(self.strings),
            locals_count: self.next_reg,
            globals_count: self.next_global,
        })
    }

    // ═══════════════════════════════════════════════════════════════
    //  Helpers
    // ═══════════════════════════════════════════════════════════════

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.string_map.get(s) {
            id
        } else {
            let id = self.strings.len() as u32;
            let arc_s: Arc<str> = Arc::from(s);
            self.strings.push(arc_s.clone());
            self.string_map.insert(arc_s, id);
            id
        }
    }

    fn alloc_reg(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn get_var(&self, name: &str) -> Option<VarInfo> {
        self.locals
            .get(name)
            .or_else(|| self.globals.get(name))
            .copied()
    }

    fn load_var(&mut self, info: VarInfo, instructions: &mut Vec<Instruction>) -> usize {
        if info.is_global {
            let r = self.alloc_reg();
            instructions.push(Instruction::LoadGlobal {
                dst: r,
                global: info.idx,
            });
            r
        } else {
            info.idx
        }
    }

    fn emit_call(
        &mut self,
        name_id: u32,
        args: Vec<usize>,
        dst: Option<usize>,
        instructions: &mut Vec<Instruction>,
    ) {
        instructions.push(Instruction::Call(Box::new(CallData {
            name_id,
            args_regs: Arc::from(args),
            dst,
            loc: self.stream.loc(),
        })));
    }

    fn emit_call_dynamic(
        &mut self,
        callee_reg: usize,
        args: Vec<usize>,
        dst: Option<usize>,
        instructions: &mut Vec<Instruction>,
    ) {
        instructions.push(Instruction::CallDynamic(Box::new(CallDynamicData {
            callee_reg,
            args_regs: Arc::from(args),
            dst,
            loc: self.stream.loc(),
        })));
    }

    fn expect_identifier(&mut self) -> Result<&'source str, JitError> {
        let loc = self.stream.loc();
        match self.stream.advance()? {
            Token::Identifier(id) => Ok(id),
            t => Err(JitError::parsing(
                format!("Expected identifier, found {:?}", t),
                loc.line as usize,
                loc.col as usize,
            )),
        }
    }

    fn try_parse_increment(
        &mut self,
        id: &'source str,
        info: &VarInfo,
        instructions: &mut Vec<Instruction>,
    ) -> Result<bool, JitError> {
        let emit_inc = |instructions: &mut Vec<Instruction>, info: &VarInfo| {
            if info.is_global {
                instructions.push(Instruction::IncrementGlobal(info.idx));
            } else {
                instructions.push(Instruction::Increment(info.idx));
            }
        };

        // Check for `id + 1`
        if matches!(self.stream.peek(), Some(Token::Identifier(rhs_id)) if rhs_id == id)
            && matches!(self.stream.peek_n(1), Some(Token::Plus))
            && matches!(self.stream.peek_n(2), Some(Token::Number(1.0)))
            && is_stmt_end(self.stream.peek_n(3))
        {
            self.stream.advance()?;
            self.stream.advance()?;
            self.stream.advance()?;
            emit_inc(instructions, info);
            return Ok(true);
        }

        // Check for `1 + id`
        if matches!(self.stream.peek(), Some(Token::Number(1.0)))
            && matches!(self.stream.peek_n(1), Some(Token::Plus))
            && matches!(self.stream.peek_n(2), Some(Token::Identifier(rhs_id)) if rhs_id == id)
            && is_stmt_end(self.stream.peek_n(3))
        {
            self.stream.advance()?;
            self.stream.advance()?;
            self.stream.advance()?;
            emit_inc(instructions, info);
            return Ok(true);
        }

        Ok(false)
    }

    // ═══════════════════════════════════════════════════════════════
    //  Statement parsing
    // ═══════════════════════════════════════════════════════════════

    /// Parse a single statement. Returns `None` when a closing `}` or EOF ends
    /// the current block.
    fn parse_statement(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Option<Result<(), JitError>> {
        self.stream.skip_newlines();
        let token = self.stream.peek()?;

        match token {
            Token::Newline => {
                self.stream.advance().ok();
                Some(Ok(()))
            }

            // ── Function declaration ────────────────────────────────
            Token::Fun => {
                self.stream.advance().ok(); // consume 'fun'
                Some(self.parse_fun_declaration(instructions))
            }

            // ── Export modifier ─────────────────────────────────────
            Token::Exp => {
                self.stream.advance().ok(); // consume 'exp'
                if self.stream.peek() == Some(Token::Fun) {
                    self.stream.advance().ok(); // consume 'fun'
                    // TODO(Task 4): mark function as exported
                    Some(self.parse_fun_declaration(instructions))
                } else {
                    Some(Err(JitError::parsing(
                        "expected 'fun' after 'exp'".to_string(),
                        self.stream.loc().line as usize,
                        self.stream.loc().col as usize,
                    )))
                }
            }

            // ── Control flow ───────────────────────────────────────
            Token::Return => {
                self.stream.advance().ok();
                Some(self.parse_return_stmt(instructions))
            }
            Token::If => {
                self.stream.advance().ok();
                Some(self.parse_if_stmt(instructions))
            }
            Token::While => {
                self.stream.advance().ok();
                Some(self.parse_while_loop(instructions))
            }
            Token::For => {
                self.stream.advance().ok();
                Some(self.parse_for_loop(instructions))
            }

            // ── Use / module imports ───────────────────────────────
            Token::Use => {
                self.stream.advance().ok();
                Some(self.parse_use_stmt())
            }

            // ── Continue ───────────────────────────────────────────
            Token::Continue => {
                self.stream.advance().ok();
                Some(Err(JitError::parsing(
                    "continue not yet supported".to_string(),
                    self.stream.loc().line as usize,
                    self.stream.loc().col as usize,
                )))
            }

            // ── Identifier → assignment or expression statement ──
            Token::Identifier(id) => {
                if self.is_assignment_start() {
                    self.stream.advance().ok(); // consume identifier
                    Some(self.parse_assignment(id, instructions))
                } else {
                    Some(self.parse_expression(instructions).map(|_| ()))
                }
            }

            // ── End of block ───────────────────────────────────────
            Token::RBrace => None,

            // ── Anything else → expression statement ──────────────
            _ => Some(self.parse_expression(instructions).map(|_| ())),
        }
    }

    /// Peek ahead past type annotations, dot-access, and bracket-access to
    /// see if the next significant token is `=` (indicating an assignment).
    fn is_assignment_start(&self) -> bool {
        let mut p = self.stream.pos + 1;
        let tokens = &self.stream.tokens;
        loop {
            let Some(td) = tokens.get(p) else {
                return false;
            };
            match td.token {
                Token::Newline | Token::LineComment => {
                    p += 1;
                }
                Token::Colon => {
                    // Skip `: TypeName`
                    p += 1;
                    if matches!(tokens.get(p).map(|td| &td.token), Some(Token::Identifier(_))) {
                        p += 1;
                    } else {
                        return false;
                    }
                }
                Token::LBracket => {
                    // Skip `[expr]`
                    let mut depth = 1;
                    p += 1;
                    while let Some(inner) = tokens.get(p) {
                        match inner.token {
                            Token::LBracket => depth += 1,
                            Token::RBracket => {
                                depth -= 1;
                                if depth == 0 {
                                    p += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        p += 1;
                    }
                }
                Token::Dot => {
                    // Skip `.field`
                    p += 1;
                    if matches!(tokens.get(p).map(|td| &td.token), Some(Token::Identifier(_))) {
                        p += 1;
                    } else {
                        return false;
                    }
                }
                Token::Equals => return true,
                _ => return false,
            }
        }
    }

    // ── Assignment ─────────────────────────────────────────────────

    /// Parse an assignment: `identifier (":" type)? "=" expression`.
    ///
    /// Variables are auto-declared on first assignment.  All variables are
    /// mutable (there is no `let` / `val` distinction in the new syntax).
    fn parse_assignment(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let loc = self.stream.loc();

        // Optional type annotation `: TypeName` – consumed and discarded.
        if self.stream.peek() == Some(Token::Colon) {
            self.stream.advance()?; // consume ':'
            self.expect_identifier()?; // consume type name, ignored
        }

        // Look up or auto-declare the variable.
        let info = match self.get_var(id) {
            Some(info) => info,
            None => {
                let is_global = !self.is_in_function;
                let idx = if is_global {
                    let i = self.next_global;
                    self.next_global += 1;
                    i
                } else {
                    self.alloc_reg()
                };
                let info = VarInfo {
                    idx,
                    is_mut: true,
                    is_global,
                    first_line: loc.line as usize,
                };
                if is_global {
                    self.globals.insert(id, info);
                } else {
                    self.locals.insert(id, info);
                }
                info
            }
        };

        // Collect accessor chain (for `obj.field = val` or `list[idx] = val`)
        let mut accessors: Vec<Accessor> = Vec::new();
        loop {
            match self.stream.peek() {
                Some(Token::LBracket) => {
                    self.stream.advance()?;
                    accessors.push(Accessor::Index(self.parse_expression(instructions)?));
                    self.stream.expect(Token::RBracket)?;
                }
                Some(Token::Dot) => {
                    self.stream.advance()?;
                    let field = self.expect_identifier()?;
                    accessors.push(Accessor::Field(self.intern(field)));
                }
                _ => break,
            }
        }

        self.stream.expect(Token::Equals)?;

        // Optimisation: `x = x + 1` → Increment / IncrementGlobal
        if accessors.is_empty() && self.try_parse_increment(id, &info, instructions)? {
            return Ok(());
        }

        let src = self.parse_expression(instructions)?;

        if accessors.is_empty() {
            if info.is_global {
                instructions.push(Instruction::StoreGlobal {
                    global: info.idx,
                    src,
                });
            } else {
                instructions.push(Instruction::Move {
                    dst: info.idx,
                    src,
                });
            }
        } else {
            let mut current = self.load_var(info, instructions);
            for item in accessors.iter().take(accessors.len() - 1) {
                let dst = self.alloc_reg();
                match *item {
                    Accessor::Index(index_reg) => instructions.push(Instruction::ListGet {
                        dst,
                        list: current,
                        index_reg,
                        loc,
                    }),
                    Accessor::Field(name_id) => instructions.push(Instruction::ObjectGet {
                        dst,
                        obj: current,
                        name_id,
                        loc,
                    }),
                }
                current = dst;
            }
            match accessors.last().unwrap() {
                Accessor::Index(index_reg) => instructions.push(Instruction::ListSet {
                    list: current,
                    index_reg: *index_reg,
                    src,
                    loc,
                }),
                Accessor::Field(name_id) => instructions.push(Instruction::ObjectSet {
                    obj: current,
                    name_id: *name_id,
                    src,
                    loc,
                }),
            }
        }
        Ok(())
    }

    // ── Function declaration ───────────────────────────────────────

    /// Parse a function declaration body (the stream is positioned after `fun`).
    #[allow(unused_variables)]
    fn parse_fun_declaration(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let name = self.expect_identifier()?;
        self.stream.expect(Token::LParen)?;
        let params = self.parse_params_until(Token::RParen)?;
        self.stream.expect(Token::RParen)?;

        // Optional return type `-> TypeName` – consumed and discarded.
        self.stream.skip_newlines();
        if self.stream.peek() == Some(Token::Arrow) {
            self.stream.advance()?; // consume '->'
            self.expect_identifier()?; // consume return type
        }

        self.stream.skip_newlines();
        self.stream.expect(Token::LBrace)?;

        // Save and reset state for function body.
        let old_locals = std::mem::take(&mut self.locals);
        let (old_reg, old_func) = (self.next_reg, self.is_in_function);
        self.next_reg = 0;
        self.is_in_function = true;

        // Register parameters.
        for &p in &params {
            let r = self.alloc_reg();
            self.locals.insert(
                p,
                VarInfo {
                    idx: r,
                    is_mut: true,
                    is_global: false,
                    first_line: self.stream.loc().line as usize,
                },
            );
        }

        let mut body = Vec::new();
        while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(&mut body) {
                res?;
            } else {
                break;
            }
        }
        self.stream.expect(Token::RBrace)?;

        if !matches!(body.last(), Some(Instruction::Return(_))) {
            body.push(Instruction::Return(None));
        }

        let name_id = self.intern(name);
        self.functions.push(UserFunction {
            name_id,
            instructions: Arc::from(body),
            locals_count: self.next_reg,
            params_count: params.len(),
        });

        self.locals = old_locals;
        self.next_reg = old_reg;
        self.is_in_function = old_func;
        Ok(())
    }

    // ── Return statement ───────────────────────────────────────────

    /// Parse `return` expression?  (stream is positioned after `return`)
    fn parse_return_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        self.stream.skip_newlines();
        if matches!(
            self.stream.peek(),
            None | Some(Token::Newline) | Some(Token::RBrace) | Some(Token::RParen)
        ) {
            instructions.push(Instruction::Return(None));
        } else {
            let val = self.parse_expression(instructions)?;
            instructions.push(Instruction::Return(Some(val)));
        }
        Ok(())
    }

    // ── If statement ───────────────────────────────────────────────

    fn parse_if_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        self.stream.skip_newlines();
        let cond = self.parse_expression(instructions)?;

        self.stream.skip_newlines();
        self.stream.expect(Token::LBrace)?;

        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0)); // placeholder → JumpIfFalse

        let mut body = Vec::new();
        while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(&mut body) {
                res?;
            } else {
                break;
            }
        }
        self.stream.expect(Token::RBrace)?;
        instructions.extend(body);

        self.stream.skip_newlines();
        if matches!(self.stream.peek(), Some(Token::Else)) {
            self.stream.advance()?; // consume 'else'

            let else_jump = instructions.len();
            instructions.push(Instruction::Jump(0)); // placeholder → skip to end

            // Patch the if-jump to the else body.
            instructions[jump_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };

            // Parse else-if or else block.
            if matches!(self.stream.peek(), Some(Token::If)) {
                self.stream.advance()?; // consume 'if'
                self.parse_if_stmt(instructions)?;
            } else {
                self.stream.skip_newlines();
                self.stream.expect(Token::LBrace)?;
                let mut else_body = Vec::new();
                while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
                    if let Some(res) = self.parse_statement(&mut else_body) {
                        res?;
                    } else {
                        break;
                    }
                }
                self.stream.expect(Token::RBrace)?;
                instructions.extend(else_body);
            }

            instructions[else_jump] = Instruction::Jump(instructions.len());
        } else {
            instructions[jump_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };
        }
        Ok(())
    }

    // ── While loop ─────────────────────────────────────────────────

    fn parse_while_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        self.stream.skip_newlines();
        let loop_start = instructions.len();

        let cond = self.parse_expression(instructions)?;

        self.stream.skip_newlines();
        self.stream.expect(Token::LBrace)?;

        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0)); // placeholder → JumpIfFalse

        let mut body = Vec::new();
        while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(&mut body) {
                res?;
            } else {
                break;
            }
        }
        self.stream.skip_newlines();
        self.stream.expect(Token::RBrace)?;
        instructions.extend(body);

        instructions.push(Instruction::Jump(loop_start));
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond,
            target: instructions.len(),
        };

        Ok(())
    }

    // ── For loop ───────────────────────────────────────────────────

    fn parse_for_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let loc = self.stream.loc();
        self.stream.skip_newlines();
        let var_name = self.expect_identifier()?;
        self.stream.expect(Token::In)?;

        let range_reg = self.parse_expression(instructions)?;

        // Optimisation: inline a literal Range to avoid heap round-trip.
        let is_range =
            matches!(instructions.last(), Some(Instruction::Range { dst, .. }) if *dst == range_reg);
        let (start_reg, end_reg, step_reg) = if is_range {
            if let Some(Instruction::Range {
                start, end, step, ..
            }) = instructions.pop()
            {
                let step_reg = match step {
                    Some(sr) => sr,
                    None => {
                        let sr = self.alloc_reg();
                        instructions.push(Instruction::LoadLiteral {
                            dst: sr,
                            val: Value::number(1.0),
                        });
                        sr
                    }
                };
                (start, end, step_reg)
            } else {
                unreachable!()
            }
        } else {
            let start_reg = self.alloc_reg();
            let end_reg = self.alloc_reg();
            let step_reg = self.alloc_reg();
            instructions.push(Instruction::RangeInfo {
                range: range_reg,
                start_dst: start_reg,
                end_dst: end_reg,
                step_dst: step_reg,
            });
            (start_reg, end_reg, step_reg)
        };

        let var_reg = self.alloc_reg();
        self.locals.insert(
            var_name,
            VarInfo {
                idx: var_reg,
                is_mut: true,
                is_global: false,
                first_line: self.stream.loc().line as usize,
            },
        );

        instructions.push(Instruction::Move {
            dst: var_reg,
            src: start_reg,
        });

        let loop_start = instructions.len();

        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0)); // placeholder → JumpIfNotLess

        self.stream.skip_newlines();
        self.stream.expect(Token::LBrace)?;

        let mut body = Vec::new();
        while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(&mut body) {
                res?;
            } else {
                break;
            }
        }
        self.stream.skip_newlines();
        self.stream.expect(Token::RBrace)?;
        instructions.extend(body);

        // Increment: var = var + step
        instructions.push(Instruction::Add {
            dst: var_reg,
            lhs: var_reg,
            rhs: step_reg,
            loc,
        });

        instructions.push(Instruction::Jump(loop_start));

        let end = instructions.len();
        instructions[jump_idx] = Instruction::JumpIfNotLess {
            var: var_reg,
            end: end_reg,
            target: end,
        };

        self.locals.remove(var_name);
        Ok(())
    }

    // ── Use statement ──────────────────────────────────────────────

    /// Parse `use path;`  ─  just store the path segments for Task 4.
    fn parse_use_stmt(&mut self) -> Result<(), JitError> {
        let mut path = Vec::new();
        loop {
            path.push(self.expect_identifier()?.to_string());
            if self.stream.peek() == Some(Token::Dot) {
                self.stream.advance()?; // consume '.'
            } else {
                break;
            }
        }
        self.uses.push(path);
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════
    //  Expression parsing (recursive descent with precedence)
    // ═══════════════════════════════════════════════════════════════

    fn parse_expression(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        self.parse_range_expr(instructions)
    }

    // ── Range `..` (lowest‑precedence binary) ─────────────────────

    /// `range_expr = or_expr (".." or_expr)?`
    fn parse_range_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let lhs = self.parse_or_expr(instructions)?;
        if self.stream.peek() == Some(Token::Range) {
            self.stream.advance()?;
            let loc = self.stream.loc();
            let rhs = self.parse_or_expr(instructions)?;
            let dst = self.alloc_reg();
            instructions.push(Instruction::Range {
                dst,
                start: lhs,
                end: rhs,
                step: None,
                loc,
            });
            Ok(dst)
        } else {
            Ok(lhs)
        }
    }

    // ── `or` (short-circuit) ──────────────────────────────────────

    /// `or_expr = and_expr ("or" and_expr)*`
    ///
    /// Short‑circuit:
    /// ```text
    /// result = lhs
    /// JumpIfFalse result, eval_rhs   ← if lhs falsy, go compute rhs
    /// Jump end                        ← lhs truthy, skip rhs
    /// eval_rhs:
    /// result = rhs
    /// end:
    /// ```
    /// Both paths converge to the same register (`saved_lhs`), so the result
    /// is always in that register regardless of which path was taken.
    fn parse_or_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let mut lhs = self.parse_and_expr(instructions)?;
        while self.stream.peek() == Some(Token::Or) {
            self.stream.advance()?;
            let saved_lhs = lhs;

            // If lhs is falsy, jump to evaluate rhs.
            let jump_to_rhs = instructions.len();
            instructions.push(Instruction::JumpIfFalse {
                cond: saved_lhs,
                target: 0,
            });

            // If lhs is truthy, skip over the rhs evaluation.
            let jump_over = instructions.len();
            instructions.push(Instruction::Jump(0));

            let rhs_start = instructions.len();
            instructions[jump_to_rhs] = Instruction::JumpIfFalse {
                cond: saved_lhs,
                target: rhs_start,
            };

            let rhs = self.parse_and_expr(instructions)?;
            // Overwrite the lhs register with rhs — on this path lhs was falsy
            // and we no longer need it.  This keeps the result in `saved_lhs`.
            instructions.push(Instruction::Move {
                dst: saved_lhs,
                src: rhs,
            });
            lhs = saved_lhs;

            let end = instructions.len();
            instructions[jump_over] = Instruction::Jump(end);
        }
        Ok(lhs)
    }

    // ── `and` (short-circuit) ─────────────────────────────────────

    /// `and_expr = comp_expr ("and" comp_expr)*`
    ///
    /// Short‑circuit:
    /// ```text
    /// result = lhs
    /// JumpIfFalse(result, end)   ← if lhs falsy, skip rhs
    /// result = rhs              ← only reached if lhs was truthy
    /// end:
    /// ```
    /// Both paths converge to `saved_lhs`.
    fn parse_and_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let mut lhs = self.parse_comp_expr(instructions)?;
        while self.stream.peek() == Some(Token::And) {
            self.stream.advance()?;
            let saved_lhs = lhs;

            let jump_idx = instructions.len();
            instructions.push(Instruction::JumpIfFalse {
                cond: saved_lhs,
                target: 0,
            });

            let rhs = self.parse_comp_expr(instructions)?;
            // On this path lhs was truthy — overwrite with rhs.
            instructions.push(Instruction::Move {
                dst: saved_lhs,
                src: rhs,
            });
            lhs = saved_lhs;

            let end = instructions.len();
            instructions[jump_idx] = Instruction::JumpIfFalse {
                cond: saved_lhs,
                target: end,
            };
        }
        Ok(lhs)
    }

    // ── Comparison operators ──────────────────────────────────────

    /// `comp_expr = add_expr (("=="|"!="|"<"|"<="|">"|">=") add_expr)*`
    fn parse_comp_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let mut lhs = self.parse_add_expr(instructions)?;
        while let Some(op) = self.stream.peek() {
            let instr = match op {
                Token::Eq
                | Token::Ne
                | Token::Lt
                | Token::Le
                | Token::Gt
                | Token::Ge => {
                    self.stream.advance()?;
                    let loc = self.stream.loc();
                    let rhs = self.parse_add_expr(instructions)?;
                    let dst = self.alloc_reg();

                    match op {
                        Token::Eq => Instruction::Eq { dst, lhs, rhs },
                        Token::Ne => Instruction::Ne { dst, lhs, rhs },
                        Token::Lt => Instruction::Lt { dst, lhs, rhs, loc },
                        Token::Le => Instruction::Le { dst, lhs, rhs, loc },
                        Token::Gt => Instruction::Gt { dst, lhs, rhs, loc },
                        Token::Ge => Instruction::Ge { dst, lhs, rhs, loc },
                        _ => unreachable!(),
                    }
                }
                _ => break,
            };
            instructions.push(instr);
            // After the instruction is pushed, we need the `dst` register
            // to become `lhs` for potential chaining. Since we already
            // created it above, we find it from the instruction we just pushed.
            lhs = if let Some(ins) = instructions.last() {
                match ins {
                    Instruction::Eq { dst, .. }
                    | Instruction::Ne { dst, .. }
                    | Instruction::Lt { dst, .. }
                    | Instruction::Le { dst, .. }
                    | Instruction::Gt { dst, .. }
                    | Instruction::Ge { dst, .. } => *dst,
                    _ => unreachable!(),
                }
            } else {
                unreachable!()
            };
        }
        Ok(lhs)
    }

    // ── Additive operators ────────────────────────────────────────

    /// `add_expr = mul_expr (("+"|"-") mul_expr)*`
    fn parse_add_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let mut lhs = self.parse_mul_expr(instructions)?;
        while let Some(op) = self.stream.peek() {
            let instr = match op {
                Token::Plus | Token::Minus => {
                    self.stream.advance()?;
                    let loc = self.stream.loc();
                    let rhs = self.parse_mul_expr(instructions)?;
                    let dst = self.alloc_reg();
                    match op {
                        Token::Plus => {
                            Instruction::Add { dst, lhs, rhs, loc }
                        }
                        Token::Minus => {
                            Instruction::Sub { dst, lhs, rhs, loc }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => break,
            };
            instructions.push(instr);
            lhs = if let Some(ins) = instructions.last() {
                match ins {
                    Instruction::Add { dst, .. } | Instruction::Sub { dst, .. } => *dst,
                    _ => unreachable!(),
                }
            } else {
                unreachable!()
            };
        }
        Ok(lhs)
    }

    // ── Multiplicative operators ──────────────────────────────────

    /// `mul_expr = unary_expr (("*"|"/") unary_expr)*`
    fn parse_mul_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let mut lhs = self.parse_unary_expr(instructions)?;
        while let Some(op) = self.stream.peek() {
            let instr = match op {
                Token::Mul | Token::Div | Token::Mod => {
                    self.stream.advance()?;
                    let loc = self.stream.loc();
                    let rhs = self.parse_unary_expr(instructions)?;
                    let dst = self.alloc_reg();
                    match op {
                        Token::Mul => Instruction::Mul { dst, lhs, rhs, loc },
                        Token::Div => Instruction::Div { dst, lhs, rhs, loc },
                        Token::Mod => Instruction::Mod { dst, lhs, rhs, loc },
                        _ => unreachable!(),
                    }
                }
                _ => break,
            };
            instructions.push(instr);
            lhs = if let Some(ins) = instructions.last() {
                match ins {
                    Instruction::Mul { dst, .. } | Instruction::Div { dst, .. } | Instruction::Mod { dst, .. } => *dst,
                    _ => unreachable!(),
                }
            } else {
                unreachable!()
            };
        }
        Ok(lhs)
    }

    // ── Unary operators ───────────────────────────────────────────

    /// `unary_expr = ("!"|"-") unary_expr | postfix_expr`
    fn parse_unary_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let loc = self.stream.loc();
        match self.stream.peek() {
            Some(Token::Not) => {
                self.stream.advance()?;
                let inner = self.parse_unary_expr(instructions)?;
                let dst = self.alloc_reg();
                instructions.push(Instruction::Not {
                    dst,
                    src: inner,
                    loc,
                });
                Ok(dst)
            }
            Some(Token::Minus) => {
                self.stream.advance()?;
                let inner = self.parse_unary_expr(instructions)?;
                // Emit `inner * -1` (arithmetic negation).
                let neg_one_reg = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral {
                    dst: neg_one_reg,
                    val: Value::number(-1.0),
                });
                let dst = self.alloc_reg();
                instructions.push(Instruction::Mul {
                    dst,
                    lhs: inner,
                    rhs: neg_one_reg,
                    loc,
                });
                Ok(dst)
            }
            _ => self.parse_postfix_expr(instructions),
        }
    }

    // ── Postfix operators ─────────────────────────────────────────

    /// `postfix_expr = primary ("(" args ")" | "[" expr "]" | "." identifier)*`
    fn parse_postfix_expr(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut current = self.parse_primary(instructions)?;
        loop {
            self.stream.skip_newlines();
            match self.stream.peek() {
                // Function / method call
                Some(Token::LParen) => {
                    self.stream.advance()?;
                    let args = self.parse_call_args(instructions)?;
                    let dst = self.alloc_reg();
                    // If `current` came from a variable load, check if it's a
                    // known function name (statically dispatch) or a dynamic call.
                    // Heuristic: if `current` references a global that is a
                    // known callable name, use Call. Otherwise use CallDynamic.
                    instructions.push(Instruction::CallDynamic(Box::new(CallDynamicData {
                        callee_reg: current,
                        args_regs: Arc::from(args),
                        dst: Some(dst),
                        loc: self.stream.loc(),
                    })));
                    current = dst;
                }
                // Index access: `expr[index]`
                Some(Token::LBracket) => {
                    self.stream.advance()?;
                    let index_reg = self.parse_expression(instructions)?;
                    self.stream.expect(Token::RBracket)?;
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::ListGet {
                        dst,
                        list: current,
                        index_reg,
                        loc: self.stream.loc(),
                    });
                    current = dst;
                }
                // Property access: `expr.field`
                Some(Token::Dot) => {
                    self.stream.advance()?;
                    let field = self.expect_identifier()?;
                    let name_id = self.intern(field);

                    // Check for `.step(N)` optimization on Range.
                    if field == "step" && matches!(self.stream.peek(), Some(Token::LParen)) {
                        self.stream.advance()?; // consume '('
                        let step_reg = self.parse_expression(instructions)?;
                        self.stream.expect(Token::RParen)?;
                        // Find the preceding Range instruction and set its step.
                        let range_idx = instructions.iter().rposition(|i| {
                            matches!(i, Instruction::Range { dst, .. } if *dst == current)
                        });
                        if let Some(idx) = range_idx {
                            let mut range = instructions.remove(idx);
                            if let Instruction::Range { step, .. } = &mut range {
                                *step = Some(step_reg);
                            }
                            instructions.push(range);
                        } else {
                            // No preceding Range — stand-alone call.
                            let dst = self.alloc_reg();
                            instructions.push(Instruction::Range {
                                dst,
                                start: current,
                                end: 0,
                                step: None,
                                loc: self.stream.loc(),
                            });
                            current = dst;
                        }
                    } else {
                        let dst = self.alloc_reg();
                        instructions.push(Instruction::ObjectGet {
                            dst,
                            obj: current,
                            name_id,
                            loc: self.stream.loc(),
                        });
                        current = dst;
                    }
                }
                _ => break,
            }
        }
        Ok(current)
    }

    // ── Primary expressions ───────────────────────────────────────

    fn parse_primary(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let loc = self.stream.loc();
        let token = self.stream.advance()?;

        match token {
            // Grouping
            Token::LParen => {
                let r = self.parse_expression(instructions)?;
                self.stream.expect(Token::RParen)?;
                Ok(r)
            }

            // Literals
            Token::Number(n) => {
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::number(n),
                });
                Ok(r)
            }
            Token::Bool(b) => {
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::bool(b),
                });
                Ok(r)
            }
            Token::Nil => {
                let r = self.alloc_reg();
                // Nil as object-id 0 (reserved sentinel).
                instructions.push(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::object(0),
                });
                Ok(r)
            }
            Token::String(s) => {
                let unescaped = unescape_string(s);
                let val = Value::sso(&unescaped)
                    .unwrap_or_else(|| Value::object(self.intern(&unescaped)));
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral { dst: r, val });
                Ok(r)
            }
            Token::Template(s) => self.parse_template_literal(s, instructions),

            // Lists
            Token::LBracket => self.parse_list_literal(instructions),

            // Objects
            Token::LBrace => self.parse_object_literal(instructions),

            // Closure: `|params| body` or `move |params| body`
            Token::Pipe => self.parse_closure(false, instructions),

            Token::Move => {
                if self.stream.peek() == Some(Token::Pipe) {
                    self.stream.advance()?; // consume '|'
                    self.parse_closure(true, instructions)
                } else {
                    Err(JitError::parsing(
                        "expected '|' after 'move'".to_string(),
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            }

            // Identifier → variable, function call, or string literal
            Token::Identifier(id) => {
                if matches!(self.stream.peek(), Some(Token::LParen)) {
                    // Function call: `name(args)`
                    self.stream.advance()?; // consume '('
                    let args = self.parse_call_args(instructions)?;
                    let dst = self.alloc_reg();

                    if let Some(info) = self.get_var(id) {
                        // Variable holding a callable — dynamic dispatch.
                        let callee_reg = self.load_var(info, instructions);
                        self.emit_call_dynamic(callee_reg, args, Some(dst), instructions);
                    } else {
                        // Static function call by name.
                        let name_id = self.intern(id);
                        self.emit_call(name_id, args, Some(dst), instructions);
                    }
                    Ok(dst)
                } else if let Some(info) = self.get_var(id) {
                    Ok(self.load_var(info, instructions))
                } else {
                    // Unknown identifier: treat as a string literal (function
                    // reference or potential module path).
                    let val = Value::sso(id)
                        .unwrap_or_else(|| Value::object(self.intern(id)));
                    let r = self.alloc_reg();
                    instructions.push(Instruction::LoadLiteral { dst: r, val });
                    Ok(r)
                }
            }

            t => Err(JitError::parsing(
                format!("Expected expression, found {:?}", t),
                loc.line as usize,
                loc.col as usize,
            )),
        }
    }

    // ── Closure ───────────────────────────────────────────────────

    /// Parse a closure: `| params | body`.
    ///
    /// # Register layout (after remap in a follow-up)
    ///
    /// Currently closures do NOT capture outer locals (they only see their own
    /// params and globals).  Proper capture analysis will be added in a later
    /// task by:
    /// 1. Pre‑scanning the body to find capture names.
    /// 2. Allocating capture registers at low indices (0..C-1) and shifting
    ///    explicit params to indices C..C+P-1.
    /// 3. Emitting a register remapping pass over the body instructions.
    fn parse_closure(
        &mut self,
        _moved: bool,
        outer_instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let params = self.parse_params_until(Token::Pipe)?;
        self.stream.expect(Token::Pipe)?;

        // Save outer state.
        let saved_locals = std::mem::take(&mut self.locals);
        let (saved_reg, saved_func) = (self.next_reg, self.is_in_function);

        // Start a fresh scope for the closure body.
        self.is_in_function = true;

        // Allocate registers for explicit params.
        for &p in &params {
            let r = self.alloc_reg();
            self.locals.insert(
                p,
                VarInfo {
                    idx: r,
                    is_mut: true,
                    is_global: false,
                    first_line: self.stream.loc().line as usize,
                },
            );
        }

        // Parse body into a temporary buffer.
        let mut body = Vec::new();
        if self.stream.peek() == Some(Token::LBrace) {
            self.stream.advance()?; // consume '{'
            while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
                if let Some(res) = self.parse_statement(&mut body) {
                    res?;
                } else {
                    break;
                }
            }
            self.stream.expect(Token::RBrace)?;
        } else {
            let val = self.parse_expression(&mut body)?;
            body.push(Instruction::Return(Some(val)));
        }

        if !matches!(body.last(), Some(Instruction::Return(_))) {
            body.push(Instruction::Return(None));
        }

        // TODO: capture analysis — for now, closures only capture params
        // and globals.  Outer locals are not captured.
        let captures: Arc<[usize]> = Arc::from([]);

        let func = UserFunction {
            name_id: self.intern(&format!("__closure_{}", self.functions.len())),
            instructions: Arc::from(body),
            locals_count: self.next_reg,
            // params_count = captures + explicit params
            params_count: params.len(),
        };
        let func_idx = self.functions.len();
        self.functions.push(func);

        // Restore outer state.
        self.locals = saved_locals;
        self.next_reg = saved_reg;
        self.is_in_function = saved_func;

        let dst = self.alloc_reg();
        outer_instructions.push(Instruction::MakeClosure {
            dst,
            func_index: func_idx,
            captures,
        });
        Ok(dst)
    }

    // ── Template literal ──────────────────────────────────────────

    fn parse_template_literal(
        &mut self,
        s: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let parts = split_template_parts(s);

        if parts.is_empty() {
            let r = self.alloc_reg();
            instructions.push(Instruction::LoadLiteral {
                dst: r,
                val: Value::sso("").unwrap(),
            });
            return Ok(r);
        }

        let mut current_res = None;
        for part in parts {
            let part_reg = match part {
                TemplatePart::Literal(lit) => {
                    let unescaped = unescape_string(lit);
                    let val = Value::sso(&unescaped)
                        .unwrap_or_else(|| Value::object(self.intern(&unescaped)));
                    let r = self.alloc_reg();
                    instructions.push(Instruction::LoadLiteral { dst: r, val });
                    r
                }
                TemplatePart::Expr(expr_src) => {
                    let reg = self.parse_sub_expr(expr_src, instructions)?;
                    let str_id = self.intern("str");
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::Call(Box::new(CallData {
                        name_id: str_id,
                        args_regs: Arc::from(vec![reg]),
                        dst: Some(dst),
                        loc: self.stream.loc(),
                    })));
                    dst
                }
            };

            if let Some(prev) = current_res {
                let next = self.alloc_reg();
                instructions.push(Instruction::Add {
                    dst: next,
                    lhs: prev,
                    rhs: part_reg,
                    loc: self.stream.loc(),
                });
                current_res = Some(next);
            } else {
                current_res = Some(part_reg);
            }
        }

        Ok(current_res.unwrap())
    }

    // ── Sub‑expression for template parts ─────────────────────────

    fn parse_sub_expr(
        &mut self,
        src: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let old_tokens = std::mem::take(&mut self.stream.tokens);
        let old_pos = self.stream.pos;

        self.stream.tokens = TokenStream::lex_all(src)?;
        self.stream.pos = 0;

        let res = self.parse_expression(instructions);

        self.stream.tokens = old_tokens;
        self.stream.pos = old_pos;

        res
    }

    // ═══════════════════════════════════════════════════════════════
    //  Sub‑parsers (shared helpers)
    // ═══════════════════════════════════════════════════════════════

    /// Parse a comma‑separated parameter list terminated by `closing`.
    fn parse_params_until(&mut self, closing: Token<'source>) -> Result<Vec<&'source str>, JitError> {
        let mut params = Vec::new();
        self.stream.skip_newlines();
        if self.stream.peek() != Some(closing) {
            loop {
                self.stream.skip_newlines();
                params.push(self.expect_identifier()?);
                self.stream.skip_newlines();
                if matches!(self.stream.peek(), Some(Token::Comma)) {
                    self.stream.advance()?;
                    // Allow trailing comma before the closing delimiter.
                    if self.stream.peek() == Some(closing) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        Ok(params)
    }

    /// Parse comma‑separated call arguments enclosed in `( ... )`.
    /// The leading `LParen` must already be consumed.
    fn parse_call_args(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<Vec<usize>, JitError> {
        let mut args = Vec::new();
        self.stream.skip_newlines();
        if !matches!(self.stream.peek(), Some(Token::RParen)) {
            loop {
                self.stream.skip_newlines();
                args.push(self.parse_expression(instructions)?);
                self.stream.skip_newlines();
                match self.stream.peek() {
                    Some(Token::Comma) => {
                        self.stream.advance()?;
                        if matches!(self.stream.peek(), Some(Token::RParen)) {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
        self.stream.expect(Token::RParen)?;
        Ok(args)
    }

    /// Parse a list literal: `[ expr (, expr)* ,? ]`
    fn parse_list_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        self.stream.skip_newlines();
        if matches!(self.stream.peek(), Some(Token::RBracket)) {
            // `[]`
            self.stream.advance()?;
            let dst = self.alloc_reg();
            instructions.push(Instruction::NewList { dst, len: 0 });
            return Ok(dst);
        }

        // Parse first element
        let first = self.parse_expression(instructions)?;
        self.stream.skip_newlines();

        // Check for `;` repetition syntax: `[val; count]`
        if matches!(self.stream.peek(), Some(Token::Semicolon)) {
            self.stream.advance()?;
            self.stream.skip_newlines();
            let count = self.parse_expression(instructions)?;
            self.stream.skip_newlines();
            self.stream.expect(Token::RBracket)?;
            let dst = self.alloc_reg();
            instructions.push(Instruction::NewListRepeat {
                dst,
                val: first,
                count,
            });
            return Ok(dst);
        }

        // Normal list literal: `[val, val, ...]`
        let mut elements = vec![first];
        if matches!(self.stream.peek(), Some(Token::Comma)) {
            self.stream.advance()?;
            self.stream.skip_newlines();
            if !matches!(self.stream.peek(), Some(Token::RBracket)) {
                loop {
                    elements.push(self.parse_expression(instructions)?);
                    self.stream.skip_newlines();
                    if matches!(self.stream.peek(), Some(Token::Comma)) {
                        self.stream.advance()?;
                        if matches!(self.stream.peek(), Some(Token::RBracket)) {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        self.stream.expect(Token::RBracket)?;

        let dst = self.alloc_reg();
        if elements.is_empty() {
            instructions.push(Instruction::NewList { dst, len: 0 });
        } else {
            instructions.push(Instruction::NewListFrom {
                dst,
                elems: Arc::from(elements),
            });
        }
        Ok(dst)
    }

    /// Parse an object literal: `{ id: expr (, id: expr)* ,? }`
    fn parse_object_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut fields = Vec::with_capacity(4);
        self.stream.skip_newlines();
        if !matches!(self.stream.peek(), Some(Token::RBrace)) {
            loop {
                self.stream.skip_newlines();
                let name = self.expect_identifier()?;
                self.stream.expect(Token::Colon)?;
                let val_reg = self.parse_expression(instructions)?;
                fields.push((name, val_reg));
                self.stream.skip_newlines();
                if matches!(self.stream.peek(), Some(Token::Comma)) {
                    self.stream.advance()?;
                    if matches!(self.stream.peek(), Some(Token::RBrace)) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.stream.expect(Token::RBrace)?;

        let dst = self.alloc_reg();
        if fields.is_empty() {
            instructions.push(Instruction::NewObject { dst, capacity: 0 });
        } else {
            let pairs: Vec<(u32, usize)> = fields
                .into_iter()
                .map(|(name, src)| (self.intern(name), src))
                .collect();
            instructions.push(Instruction::NewObjectFrom {
                dst,
                fields: Arc::from(pairs),
            });
        }
        Ok(dst)
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal types
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
enum Accessor {
    Index(usize),
    Field(u32),
}

// ═══════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════

/// True if the token is a statement terminator (newline, brace, comma,
/// paren, or EOF).
fn is_stmt_end(t: Option<Token<'_>>) -> bool {
    matches!(
        t,
        None | Some(Token::Newline)
            | Some(Token::RBrace)
            | Some(Token::Comma)
            | Some(Token::RParen)
    )
}

// ═══════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_assignment() {
        let input = "x = 10";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        assert_eq!(program.globals_count, 1);
        assert_eq!(program.instructions.len(), 2);

        match &program.instructions[1] {
            Instruction::StoreGlobal { global, src } => {
                assert_eq!(*global, 0);
                assert_eq!(*src, 0);
            }
            other => panic!("Expected StoreGlobal, found {other:?}"),
        }
    }

    #[test]
    fn test_parse_arithmetic() {
        let input = "x = 1 + 2 * 3";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        // 1. LoadLiteral(1) → r0
        // 2. LoadLiteral(2) → r1
        // 3. LoadLiteral(3) → r2
        // 4. r1 * r2 → r3
        // 5. r0 + r3 → r4
        // 6. StoreGlobal(0, r4)

        assert_eq!(program.instructions.len(), 6);
        assert_eq!(program.globals_count, 1);
    }

    #[test]
    fn test_parse_if_statement() {
        let input = "x = 10\nif x > 0 {\n  x = 20\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        assert_eq!(program.globals_count, 1);
        let has_jump = program.instructions.iter().any(|i| match i {
            Instruction::JumpIfFalse { .. } => true,
            _ => false,
        });
        assert!(has_jump);
    }

    #[test]
    fn test_parse_function_declaration() {
        let input = "fun add(a, b) {\n  return a + b\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        assert_eq!(program.functions.len(), 1);
        let func = &program.functions[0];
        assert!(program.string_pool.iter().any(|s| &**s == "add"));

        let has_return = func.instructions.iter().any(|i| match i {
            Instruction::Return(_) => true,
            _ => false,
        });
        assert!(has_return);
    }

    #[test]
    fn test_parse_list_and_object() {
        let input = "l = [1, 2, 3]\no = {a: 1, b: 2}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        assert_eq!(program.globals_count, 2);
        let has_new_list = program.instructions.iter().any(|i| {
            matches!(i, Instruction::NewList { .. } | Instruction::NewListFrom { .. })
        });
        let has_new_obj = program.instructions.iter().any(|i| {
            matches!(i, Instruction::NewObject { .. } | Instruction::NewObjectFrom { .. })
        });

        assert!(has_new_list);
        assert!(has_new_obj);
    }

    #[test]
    fn test_parse_error_unknown_variable() {
        // In v2, unknown identifiers are treated as string literals.
        // Test a real syntax error: missing expression after `=`.
        let input = "x = ";
        let parser = Parser::new(input).unwrap();
        let result = parser.compile();
        assert!(result.is_err());
    }

    // ── New v2 tests ──────────────────────────────────────────────

    #[test]
    fn test_or_short_circuit() {
        let input = "x = true or false";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should have JumpIfFalse, Jump instructions for short-circuit.
        let jumps = program
            .instructions
            .iter()
            .filter(|i| matches!(i, Instruction::JumpIfFalse { .. } | Instruction::Jump(_)))
            .count();
        assert!(jumps >= 2, "expected short-circuit jumps, got {jumps}");
    }

    #[test]
    fn test_and_short_circuit() {
        let input = "x = true and false";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        let jumps = program
            .instructions
            .iter()
            .filter(|i| matches!(i, Instruction::JumpIfFalse { .. }))
            .count();
        assert!(jumps >= 1, "expected JumpIfFalse for and short-circuit, got {jumps}");
    }

    #[test]
    fn test_closure_simple() {
        let input = "f = |a, b| a + b";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert_eq!(program.functions.len(), 1);
        let func = &program.functions[0];
        assert_eq!(func.params_count, 2);
    }

    #[test]
    fn test_closure_block_body() {
        let input = "f = |x| {\n  y = x + 1\n  return y\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert_eq!(program.functions.len(), 1);
        let func = &program.functions[0];
        // params: x => 1 explicit param, no captures
        assert_eq!(func.params_count, 1);
    }

    #[test]
    fn test_for_loop() {
        let input = "for i in 1..5 {\n  print(i)\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // The literal Range is optimised away (popped) and replaced with
        // direct start/end/step registers.  JumpIfNotLess should remain.
        let has_range = program.instructions.iter().any(|i| matches!(i, Instruction::Range { .. }));
        let has_jnlt = program.instructions.iter().any(|i| matches!(i, Instruction::JumpIfNotLess { .. }));
        assert!(!has_range, "inline Range should be optimised away");
        assert!(has_jnlt, "expected JumpIfNotLess instruction");
    }

    #[test]
    fn test_while_loop() {
        let input = "x = 0\nwhile x < 5 {\n  x = x + 1\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        let has_jump = program.instructions.iter().any(|i| matches!(i, Instruction::Jump(target) if *target > 0));
        assert!(has_jump, "expected back-edge Jump");
    }

    #[test]
    fn test_use_statement() {
        let input = "use math.vector";
        // Verify compile succeeds (uses are collected during compile).
        let parser = Parser::new(input).unwrap();
        let _program = parser.compile().unwrap();
    }

    #[test]
    fn test_nil_literal() {
        let input = "x = nil";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert_eq!(program.globals_count, 1);
        // Should load nil (Value::object(0))
        match &program.instructions[0] {
            Instruction::LoadLiteral { val, .. } => {
                assert!(val.as_obj_id() == Some(0));
            }
            other => panic!("Expected LoadLiteral, got {other:?}"),
        }
    }

    #[test]
    fn test_exp_fun() {
        let input = "exp fun add(a, b) {\n  return a + b\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert_eq!(program.functions.len(), 1);
        assert!(program.string_pool.iter().any(|s| &**s == "add"));
    }

    #[test]
    fn test_trailing_comma_in_list() {
        let input = "x = [1, 2, 3,]";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert_eq!(program.globals_count, 1);
    }

    #[test]
    fn test_trailing_comma_in_call() {
        let input = "print(1, 2,)";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should parse without error
        assert!(program.instructions.len() > 0);
    }

    #[test]
    fn test_comparison_chain() {
        let input = "x = 1 < 2 and 3 > 4";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should have Lt, Gt, and JumpIfFalse (and short-circuit)
        let has_lt = program.instructions.iter().any(|i| matches!(i, Instruction::Lt { .. }));
        let has_gt = program.instructions.iter().any(|i| matches!(i, Instruction::Gt { .. }));
        assert!(has_lt);
        assert!(has_gt);
    }

    #[test]
    fn test_unary_not() {
        let input = "x = !true";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        assert!(program.instructions.iter().any(|i| matches!(i, Instruction::Not { .. })));
    }

    #[test]
    fn test_unary_minus() {
        // `-5` is lexed as Number(-5.0) by the lexer, so use `-(1 + 2)` to test
        // runtime arithmetic negation.
        let input = "x = -(1 + 2)";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should have LoadLiteral(-1.0) and Mul for negation
        let has_mul = program.instructions.iter().any(|i| matches!(i, Instruction::Mul { .. }));
        assert!(has_mul, "expected Mul for negation");
    }

    #[test]
    fn test_method_call() {
        let input = "x = \"hello\"\ny = x.len()";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should have ObjectGet for .len, then CallDynamic
        let has_obj_get = program.instructions.iter().any(|i| matches!(i, Instruction::ObjectGet { .. }));
        assert!(has_obj_get);
    }

    #[test]
    fn test_else_if() {
        let input = "if x > 0 {\n  y = 1\n} else if x < 0 {\n  y = -1\n} else {\n  y = 0\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();
        // Should have multiple JumpIfFalse and Jump instructions
        let jump_count = program
            .instructions
            .iter()
            .filter(|i| {
                matches!(i, Instruction::JumpIfFalse { .. } | Instruction::Jump(_))
            })
            .count();
        assert!(jump_count >= 3, "expected >=3 jumps for else-if chain, got {jump_count}");
    }
}
