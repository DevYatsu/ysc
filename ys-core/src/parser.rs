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

#[derive(Clone, Copy)]
enum Accessor {
    Index(usize),
    Field(u32),
}

/// The Parser transforms source code into a compiled Program (bytecode).
pub struct Parser<'source> {
    stream: TokenStream<'source>,

    /// Global variables.
    globals: FxHashMap<&'source str, VarInfo>,
    /// Local variables in the current function/scope.
    locals: FxHashMap<&'source str, VarInfo>,

    /// Interned strings.
    strings: Vec<Arc<str>>,
    /// Map for fast interning lookups.
    string_map: FxHashMap<Arc<str>, u32>,

    functions: Vec<UserFunction>,

    next_reg: usize,
    next_global: usize,

    is_in_function: bool,
    loop_continues: Vec<Vec<usize>>,
}

impl<'source> Parser<'source> {
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
            loop_continues: Vec::new(),
            functions: Vec::with_capacity(16),
        })
    }

    pub fn compile(mut self) -> Result<Program, JitError> {
        let mut instructions = Vec::new();
        while self.stream.peek().is_some() {
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

    fn parse_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        self.parse_binary(0, instructions)
    }

    fn parse_binary(
        &mut self,
        min_prec: u8,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut lhs = self.parse_primary(instructions)?;
        while let Some(op) = self.stream.peek() {
            let prec = match op {
                Token::Range => 0,
                Token::Eq | Token::Ne => 1,
                Token::Lt | Token::Le | Token::Gt | Token::Ge => 2,
                Token::Plus | Token::Minus => 3,
                Token::Mul | Token::Div => 4,
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.stream.advance()?;
            let loc = self.stream.loc();
            let rhs = self.parse_binary(prec + 1, instructions)?;
            let dst = self.alloc_reg();
            let instr = match op {
                Token::Range => Instruction::Range {
                    dst,
                    start: lhs,
                    end: rhs,
                    step: None,
                    loc,
                },
                Token::Eq => Instruction::Eq { dst, lhs, rhs },
                Token::Ne => Instruction::Ne { dst, lhs, rhs },
                Token::Lt => Instruction::Lt { dst, lhs, rhs, loc },
                Token::Le => Instruction::Le { dst, lhs, rhs, loc },
                Token::Gt => Instruction::Gt { dst, lhs, rhs, loc },
                Token::Ge => Instruction::Ge { dst, lhs, rhs, loc },
                Token::Plus => Instruction::Add { dst, lhs, rhs, loc },
                Token::Minus => Instruction::Sub { dst, lhs, rhs, loc },
                Token::Mul => Instruction::Mul { dst, lhs, rhs, loc },
                Token::Div => Instruction::Div { dst, lhs, rhs, loc },
                _ => unreachable!(),
            };
            instructions.push(instr);
            lhs = dst;
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let loc = self.stream.loc();
        let token = self.stream.advance()?;
        let mut current_reg = match token {
            Token::LParen => {
                let r = self.parse_expr(instructions)?;
                self.stream.expect(Token::RParen)?;
                Ok(r)
            }
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
            Token::String(s) => {
                let unescaped = unescape_string(s);
                let val = Value::sso(&unescaped)
                    .unwrap_or_else(|| Value::object(self.intern(&unescaped)));
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral { dst: r, val });
                Ok(r)
            }
            Token::Template(s) => self.parse_template_literal(s, instructions),
            Token::LBracket => self.parse_list_literal(instructions),
            Token::LBrace => self.parse_object_literal(instructions),
            Token::Identifier(id) => {
                if matches!(self.stream.peek(), Some(Token::LParen)) {
                    self.stream.advance()?; // consume (
                    let args = self.parse_call_args(instructions)?;
                    let dst = self.alloc_reg();

                    if let Some(info) = self.get_var(id) {
                        let callee_reg = self.load_var(info, instructions);
                        self.emit_call_dynamic(callee_reg, args, Some(dst), instructions);
                    } else {
                        let name_id = self.intern(id);
                        self.emit_call(name_id, args, Some(dst), instructions);
                    }
                    Ok(dst)
                } else if let Some(info) = self.get_var(id) {
                    Ok(self.load_var(info, instructions))
                } else {
                    // Potential function literal
                    let val = Value::sso(id).unwrap_or_else(|| Value::object(self.intern(id)));
                    let r = self.alloc_reg();
                    instructions.push(Instruction::LoadLiteral { dst: r, val });
                    Ok(r)
                }
            }
            Token::Not => {
                let inner = self.parse_primary(instructions)?;
                let dst = self.alloc_reg();
                instructions.push(Instruction::Not {
                    dst,
                    src: inner,
                    loc,
                });
                Ok(dst)
            }
            _ => Err(JitError::parsing(
                format!("Expected expression, found {:?}", token),
                loc.line as usize,
                loc.col as usize,
            )),
        }?;

        // Handle suffixes:indexing [expr] and property access .id
        loop {
            self.stream.skip_newlines();
            match self.stream.peek() {
                Some(Token::LBracket) => {
                    self.stream.advance()?;
                    let index_reg = self.parse_expr(instructions)?;
                    self.stream.expect(Token::RBracket)?;
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::ListGet {
                        dst,
                        list: current_reg,
                        index_reg,
                        loc: self.stream.loc(),
                    });
                    current_reg = dst;
                }
                Some(Token::Dot) => {
                    self.stream.advance()?;
                    let id = match self.stream.advance()? {
                        Token::Identifier(id) => id,
                        t => {
                            return Err(JitError::parsing(
                                format!("Expected property name after '.', found {:?}", t),
                                self.stream.loc().line as usize,
                                self.stream.loc().col as usize,
                            ));
                        }
                    };
                    // Optimize .step(N) on Range: reorder so step loads before Range.
                    if id == "step" && matches!(self.stream.peek(), Some(Token::LParen)) {
                        self.stream.advance()?; // consume (
                        let step_reg = self.parse_expr(instructions)?;
                        self.stream.expect(Token::RParen)?;
                        let range_idx = instructions.iter().rposition(|i| matches!(i, Instruction::Range { dst, .. } if *dst == current_reg));
                        if let Some(idx) = range_idx {
                            let mut range = instructions.remove(idx);
                            if let Instruction::Range { step, .. } = &mut range { *step = Some(step_reg); }
                            instructions.push(range);
                        } else {
                            let dst = self.alloc_reg();
                            instructions.push(Instruction::Range { dst, start: current_reg, end: 0, step: None, loc: self.stream.loc() });
                            current_reg = dst;
                        }
                    } else {
                        let name_id = self.intern(id);
                        let dst = self.alloc_reg();
                        instructions.push(Instruction::ObjectGet {
                            dst,
                            obj: current_reg,
                            name_id,
                            loc: self.stream.loc(),
                        });
                        current_reg = dst;
                    }
                }
                Some(Token::LParen) => {
                    self.stream.advance()?;
                    let args = self.parse_call_args(instructions)?;
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::CallDynamic(Box::new(
                        crate::compiler::CallDynamicData {
                            callee_reg: current_reg,
                            args_regs: Arc::from(args),
                            dst: Some(dst),
                            loc: self.stream.loc(),
                        },
                    )));
                    current_reg = dst;
                }
                _ => break,
            }
        }
        Ok(current_reg)
    }

    fn parse_call_args(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<Vec<usize>, JitError> {
        let mut args = Vec::new();
        self.stream.skip_newlines();
        if !matches!(self.stream.peek(), Some(Token::RParen)) {
            loop {
                self.stream.skip_newlines();
                args.push(self.parse_expr(instructions)?);
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

    fn parse_list_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut elements = Vec::new();
        self.stream.skip_newlines();
        if !matches!(self.stream.peek(), Some(Token::RBracket)) {
            loop {
                self.stream.skip_newlines();
                elements.push(self.parse_expr(instructions)?);
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

    fn parse_object_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut fields = Vec::with_capacity(4);
        self.stream.skip_newlines();
        if !matches!(self.stream.peek(), Some(Token::RBrace)) {
            loop {
                self.stream.skip_newlines();
                let name = match self.stream.advance()? {
                    Token::Identifier(id) => id,
                    t => {
                        return Err(JitError::parsing(
                            format!("Expected field name, found {:?}", t),
                            self.stream.loc().line as usize,
                            self.stream.loc().col as usize,
                        ));
                    }
                };
                self.stream.expect(Token::Colon)?;
                let val_reg = self.parse_expr(instructions)?;
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
            let pairs: Vec<(u32, usize)> = fields.into_iter()
                .map(|(name, src)| (self.intern(name), src))
                .collect();
            instructions.push(Instruction::NewObjectFrom {
                dst,
                fields: Arc::from(pairs),
            });
        }
        Ok(dst)
    }

    fn parse_statement(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Option<Result<(), JitError>> {
        loop {
            let token = self.stream.peek()?;

            match token {
                Token::Newline => {
                    self.stream.advance().ok();
                }
                Token::For => return Some(self.parse_for_loop(instructions)),
                Token::While => {
                    self.stream.advance().ok();
                    return Some(self.parse_while_loop(instructions));
                }
                Token::Fun => {
                    self.stream.advance().ok();
                    return Some(self.parse_fn_decl());
                }
                Token::If => {
                    self.stream.advance().ok();
                    return Some(self.parse_if_stmt(instructions));
                }
                Token::Return => {
                    self.stream.advance().ok();
                    return Some(self.parse_return_stmt(instructions));
                }
                Token::Continue => {
                    self.stream.advance().ok();
                    if let Some(list) = self.loop_continues.last_mut() {
                        list.push(instructions.len());
                        instructions.push(Instruction::Jump(0));
                    } else {
                        return Some(Err(JitError::parsing(
                            "continue outside of loop".to_string(),
                            self.stream.loc().line as usize,
                            self.stream.loc().col as usize,
                        )));
                    }
                    return Some(Ok(()));
                }
                Token::Identifier(id) => {
                    if self.is_assignment() {
                        self.stream.advance().ok();
                        return Some(self.parse_assignment(id, instructions));
                    } else if matches!(
                        self.stream.peek_n(1),
                        Some(Token::Dot) | Some(Token::LBracket) | Some(Token::LParen)
                    ) {
                        return Some(self.parse_expr(instructions).map(|_| ()));
                    } else {
                        self.stream.advance().ok();
                        return Some(self.parse_call_stmt(id, instructions));
                    }
                }
                Token::RBrace => return None,
                _ => {
                    return Some(Err(JitError::parsing(
                        format!("Unexpected token {:?}", token),
                        self.stream.loc().line as usize,
                        self.stream.loc().col as usize,
                    )));
                }
            }
        }
    }

    fn is_assignment(&self) -> bool {
        let tokens = &self.stream.tokens;
        let mut p = self.stream.pos + 1;
        while let Some(td) = tokens.get(p) {
            match td.token {
                Token::LBracket => {
                    // Find matching RBracket
                    let mut depth = 1;
                    p += 1;
                    while let Some(td) = tokens.get(p) {
                        match td.token {
                            Token::LBracket => depth += 1,
                            Token::RBracket => depth -= 1,
                            _ => {}
                        }
                        p += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                Token::Dot => {
                    p += 1;
                    match tokens.get(p).map(|td| &td.token) {
                        Some(Token::Identifier(_)) => p += 1,
                        _ => return false,
                    }
                }
                Token::Equals => return true,
                _ => return false,
            }
        }
        false
    }

    fn parse_call_stmt(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let args = if matches!(self.stream.peek(), Some(Token::LParen)) {
            self.stream.advance()?;
            self.parse_call_args(instructions)?
        } else {
            let mut args = Vec::new();
            while let Some(t) = self.stream.peek() {
                if matches!(
                    t,
                    Token::Newline | Token::RBrace | Token::RParen | Token::RBracket | Token::Comma
                ) {
                    break;
                }
                args.push(self.parse_expr(instructions)?);
                if matches!(self.stream.peek(), Some(Token::Comma)) {
                    self.stream.advance()?;
                } else {
                    break;
                }
            }
            args
        };

        if let Some(info) = self.get_var(id) {
            let callee_reg = self.load_var(info, instructions);
            self.emit_call_dynamic(callee_reg, args, None, instructions);
        } else {
            let name_id = self.intern(id);
            self.emit_call(name_id, args, None, instructions);
        }
        Ok(())
    }

    fn parse_assignment(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let loc = self.stream.loc();
        // In v2, all variables are mutable and auto-declared on first assignment.
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

        let mut accessors = Vec::new();
        loop {
            match self.stream.peek() {
                Some(Token::LBracket) => {
                    self.stream.advance()?;
                    accessors.push(Accessor::Index(self.parse_expr(instructions)?));
                    self.stream.expect(Token::RBracket)?;
                }
                Some(Token::Dot) => {
                    self.stream.advance()?;
                    match self.stream.advance()? {
                        Token::Identifier(field) => {
                            accessors.push(Accessor::Field(self.intern(field)))
                        }
                        t => {
                            return Err(JitError::parsing(
                                format!("Expected field name after '.', found {:?}", t),
                                self.stream.loc().line as usize,
                                self.stream.loc().col as usize,
                            ));
                        }
                    }
                }
                _ => break,
            }
        }

        self.stream.expect(Token::Equals)?;

        // Optimization: x = x + 1  or  x = 1 + x
        if accessors.is_empty()
            && self.try_parse_increment(id, &info, instructions)? {
                return Ok(());
            }

        let src = self.parse_expr(instructions)?;
        if accessors.is_empty() {
            if info.is_global {
                instructions.push(Instruction::StoreGlobal {
                    global: info.idx,
                    src,
                });
            } else {
                instructions.push(Instruction::Move { dst: info.idx, src });
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

    fn parse_var_decl(
        &mut self,
        is_mut: bool,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        self.stream.advance()?; // consume el/le
        let id = match self.stream.advance()? {
            Token::Identifier(id) => id,
            t => {
                return Err(JitError::parsing(
                    format!("Expected identifier, found {:?}", t),
                    self.stream.loc().line as usize,
                    self.stream.loc().col as usize,
                ));
            }
        };
        self.stream.expect(Token::Colon)?;
        let src = self.parse_expr(instructions)?;

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
            is_mut,
            is_global,
            first_line: self.stream.loc().line as usize,
        };

        if is_global {
            self.globals.insert(id, info);
            instructions.push(Instruction::StoreGlobal { global: idx, src });
        } else {
            self.locals.insert(id, info);
            instructions.push(Instruction::Move { dst: idx, src });
        }
        Ok(())
    }

    fn parse_block(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        self.stream.skip_newlines();
        self.stream.expect(Token::LBrace)?;
        while self.stream.peek().is_some() && self.stream.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(instructions) {
                res?;
            } else {
                break;
            }
        }
        self.stream.expect(Token::RBrace)?;
        Ok(())
    }

    fn parse_if_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let cond = self.parse_expr(instructions)?;
        let jump_if_false_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;

        self.stream.skip_newlines();
        if matches!(self.stream.peek(), Some(Token::Else)) {
            self.stream.advance()?;
            let jump_to_end_idx = instructions.len();
            instructions.push(Instruction::Jump(0));
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };
            self.parse_block(instructions)?;
            instructions[jump_to_end_idx] = Instruction::Jump(instructions.len());
        } else {
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };
        }
        Ok(())
    }

    fn parse_while_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let start = instructions.len();
        self.loop_continues.push(Vec::new());
        let cond = self.parse_expr(instructions)?;
        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;
        instructions.push(Instruction::Jump(start));
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond,
            target: instructions.len(),
        };
        for continue_idx in self.loop_continues.pop().unwrap() {
            instructions[continue_idx] = Instruction::Jump(start);
        }
        Ok(())
    }

    fn parse_for_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let loc = self.stream.loc();
        self.stream.advance()?; // for
        let id = match self.stream.advance()? {
            Token::Identifier(id) => id,
            t => {
                return Err(JitError::parsing(
                    format!("Expected identifier, found {:?}", t),
                    self.stream.loc().line as usize,
                    self.stream.loc().col as usize,
                ));
            }
        };
        self.stream.expect(Token::In)?;
        let iter_val = self.parse_expr(instructions)?;

        // When the iterator is a Range, avoid the heap allocation + RangeInfo
        // round-trip by using the component registers directly.
        // Check if the iterator is a simple Range — extract its registers
        // and skip the heap allocation + RangeInfo round-trip.
        let is_range = matches!(instructions.last(), Some(Instruction::Range { dst, .. }) if *dst == iter_val);
        let (start_reg, end_reg, step_reg) = if is_range {
            // Read fields from the Range before we pop it.
            let (s, e, st) = match instructions.last().unwrap() {
                Instruction::Range { start, end, step, .. } => (*start, *end, *step),
                _ => unreachable!(),
            };
            instructions.pop(); // Remove the Range, avoid heap alloc.
            let step_reg = match st {
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
            (s, e, step_reg)
        } else {
                // Generic path: heap-allocated iterable via RangeInfo.
                let start_reg = self.alloc_reg();
                let end_reg = self.alloc_reg();
                let step_reg = self.alloc_reg();
                instructions.push(Instruction::RangeInfo {
                    range: iter_val,
                    start_dst: start_reg,
                    end_dst: end_reg,
                    step_dst: step_reg,
                });
                (start_reg, end_reg, step_reg)
        };

        let var_idx = self.alloc_reg();
        self.locals.insert(
            id,
            VarInfo {
                idx: var_idx,
                is_mut: true,
                is_global: false,
                first_line: self.stream.loc().line as usize,
            },
        );

        instructions.push(Instruction::Move {
            dst: var_idx,
            src: start_reg,
        });

        let loop_start = instructions.len();

        let jump_idx = instructions.len();
        // Placeholder replaced with JumpIfNotLess after the body.
        instructions.push(Instruction::Jump(0));

        self.loop_continues.push(Vec::new());
        self.parse_block(instructions)?;

        let continue_target = instructions.len();
        for continue_idx in self.loop_continues.pop().unwrap() {
            instructions[continue_idx] = Instruction::Jump(continue_target);
        }

        instructions.push(Instruction::Add {
            dst: var_idx,
            lhs: var_idx,
            rhs: step_reg,
            loc,
        });

        instructions.push(Instruction::Jump(loop_start));
        instructions[jump_idx] = Instruction::JumpIfNotLess {
            var: var_idx,
            end: end_reg,
            target: instructions.len(),
        };
        Ok(())
    }

    fn parse_fn_decl(&mut self) -> Result<(), JitError> {
        let name = match self.stream.advance()? {
            Token::Identifier(id) => id,
            t => {
                return Err(JitError::parsing(
                    format!("Expected function name, found {:?}", t),
                    self.stream.loc().line as usize,
                    self.stream.loc().col as usize,
                ));
            }
        };
        self.stream.expect(Token::LParen)?;
        let mut params = Vec::new();
        if !matches!(self.stream.peek(), Some(Token::RParen)) {
            loop {
                match self.stream.advance()? {
                    Token::Identifier(id) => params.push(id),
                    t => {
                        return Err(JitError::parsing(
                            format!("Expected parameter name, found {:?}", t),
                            self.stream.loc().line as usize,
                            self.stream.loc().col as usize,
                        ));
                    }
                }
                if matches!(self.stream.peek(), Some(Token::Comma)) {
                    self.stream.advance()?;
                } else {
                    break;
                }
            }
        }
        self.stream.expect(Token::RParen)?;

        let old_locals = std::mem::take(&mut self.locals);
        let (old_reg, old_func) = (self.next_reg, self.is_in_function);
        self.next_reg = 0;
        self.is_in_function = true;

        for &p in &params {
            let r = self.alloc_reg();
            self.locals.insert(
                p,
                VarInfo {
                    idx: r,
                    is_mut: false,
                    is_global: false,
                    first_line: self.stream.loc().line as usize,
                },
            );
        }

        let mut body = Vec::new();
        self.parse_block(&mut body)?;
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

    fn parse_return_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let val = if !matches!(
            self.stream.peek(),
            None | Some(Token::Newline) | Some(Token::RBrace)
        ) {
            Some(self.parse_expr(instructions)?)
        } else {
            None
        };
        instructions.push(Instruction::Return(val));
        Ok(())
    }

    fn get_var(&self, id: &str) -> Option<VarInfo> {
        self.locals
            .get(id)
            .or_else(|| self.globals.get(id))
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
                    instructions.push(Instruction::Call(Box::new(crate::compiler::CallData {
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

    fn parse_sub_expr(
        &mut self,
        src: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let old_tokens = std::mem::take(&mut self.stream.tokens);
        let old_pos = self.stream.pos;

        self.stream.tokens = TokenStream::lex_all(src)?;
        self.stream.pos = 0;

        let res = self.parse_expr(instructions);

        self.stream.tokens = old_tokens;
        self.stream.pos = old_pos;

        res
    }

    /// If the next tokens match `x: x + 1` or `x: 1 + x`, emit an Increment instruction.
    /// Returns true if the pattern matched and was consumed.
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
}

/// True if the token is a statement terminator (newline, brace, comma, paren, or EOF).
fn is_stmt_end(t: Option<Token<'_>>) -> bool {
    matches!(
        t,
        None | Some(Token::Newline)
            | Some(Token::RBrace)
            | Some(Token::Comma)
            | Some(Token::RParen)
    )
}


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

        // LoadLiteral
        // StoreGlobal

        match &program.instructions[1] {
            Instruction::StoreGlobal { global, src } => {
                assert_eq!(*global, 0);
                assert_eq!(*src, 0); // first register allocated for literal
            }
            _ => panic!("Expected StoreGlobal, found {:?}", program.instructions[1]),
        }
    }

    #[test]
    fn test_parse_arithmetic() {
        let input = "x = 1 + 2 * 3";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        // 1. Literal 1 -> r0
        // 2. Literal 2 -> r1
        // 3. Literal 3 -> r2
        // 4. r1 * r2 -> r3
        // 5. r0 + r3 -> r4
        // 6. StoreGlobal(0, r4)

        assert_eq!(program.instructions.len(), 6);
        assert_eq!(program.globals_count, 1);
    }

    #[test]
    fn test_parse_if_statement() {
        let input = "x = 10\nif x > 0 {\n  x = 20\n}";
        let parser = Parser::new(input).unwrap();
        let program = parser.compile().unwrap();

        // 1. LoadLiteral(r0, 10)
        // 2. StoreGlobal(0, r0)
        // 3. LoadGlobal(r1, 0)
        // 4. LoadLiteral(r2, 0)
        // 5. Gt(r3, r1, r2)
        // 6. JumpIfFalse(r3, target)
        // 7. LoadLiteral(r4, 20)
        // 8. StoreGlobal(0, r4)
        // 9. (Target)

        assert_eq!(program.globals_count, 1);
        // Checking for JumpIfFalse
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
        // "add" should be in string pool
        assert!(program.string_pool.iter().any(|s| &**s == "add"));

        // Return instruction should be present
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
        let has_new_list = program
            .instructions
            .iter()
            .any(|i| matches!(i, Instruction::NewList { .. } | Instruction::NewListFrom { .. }));
        let has_new_obj = program
            .instructions
            .iter()
            .any(|i| matches!(i, Instruction::NewObject { .. } | Instruction::NewObjectFrom { .. }));

        assert!(has_new_list);
        assert!(has_new_obj);
    }

    #[test]
    fn test_parse_error_unknown_variable() {
        // In v2, identifiers in expressions are treated as potential function
        // references (loaded as string literals), so `x = y` succeeds.
        // Test a real syntax error instead: missing expression after `=`.
        let input = "x = ";
        let parser = Parser::new(input).unwrap();
        let result = parser.compile();
        assert!(result.is_err());
    }
}
