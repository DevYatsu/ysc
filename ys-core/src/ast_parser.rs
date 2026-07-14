//! Recursive‑descent parser that builds an AST instead of emitting bytecode.
//! The grammar is identical to the direct‑emit parser in [`parser.rs`].

use crate::ast::*;
use crate::compiler::Loc;
use crate::error::JitError;
use crate::lexer::Token;
use crate::token_stream::TokenStream;
use crate::unescape::unescape_string;

// Re-export helpers from the existing parser where they are pure grammar helpers.
/// Check whether a token ends a statement (newlines, braces, commas, parens).
/// Used to determine if a `return` or other keyword has an operand on the same line.
pub(crate) fn is_stmt_end(t: Option<Token<'_>>) -> bool {
    matches!(
        t,
        None | Some(Token::Newline)
            | Some(Token::RBrace)
            | Some(Token::Comma)
            | Some(Token::RParen)
    )
}

/// Recursive‑descent parser that produces an AST.
pub struct AstParser<'source> {
    pub stream: TokenStream<'source>,
}

impl<'source> AstParser<'source> {
    pub fn new(input: &'source str) -> Result<Self, JitError> {
        let tokens = TokenStream::lex_all(input)?;
        Ok(Self { stream: TokenStream::new(tokens) })
    }

    //  Entry point

    pub fn parse_program(&mut self) -> Result<AstBlock, JitError> {
        let mut stmts = Vec::new();
        loop {
            self.stream.skip_newlines();
            if self.stream.peek().is_none() { break; }
            if let Some(s) = self.parse_statement()? {
                stmts.push(s);
            }
        }
        Ok(stmts)
    }

    //  Helpers

    fn loc(&self) -> Loc { self.stream.loc() }

    fn advance(&mut self) -> Result<Token<'source>, JitError> { self.stream.advance() }

    fn peek(&self) -> Option<Token<'source>> { self.stream.peek() }

    fn expect(&mut self, t: Token<'source>) -> Result<(), JitError> { self.stream.expect(t) }

    fn expect_ident(&mut self) -> Result<&'source str, JitError> {
        let loc = self.loc();
        match self.advance()? {
            Token::Identifier(id) => Ok(id),
            t => Err(JitError::parsing(
                format!("Expected identifier, found {:?}", t),
                loc.line as usize, loc.col as usize,
            )),
        }
    }

    /// Parse a `{ … }` block and return its statement list.
    /// If the result is a single-node block, it is flattened to a `Vec`.
    fn parse_block_stmts(&mut self) -> Result<AstBlock, JitError> {
        match self.parse_block()? {
            AstNode::Block(s, _) => Ok(s),
            other => Ok(vec![other]),
        }
    }

    #[allow(dead_code)]
    fn expect_str(&mut self) -> Result<String, JitError> {
        match self.advance()? {
            Token::String(s) => Ok(unescape_string(s)),
            t => Err(JitError::parsing(
                format!("Expected string, found {:?}", t),
                self.loc().line as usize, self.loc().col as usize,
            )),
        }
    }

    //  Statement parsing

    /// Parse a single statement. Returns `None` on `}` or EOF (end of block).
    fn parse_statement(&mut self) -> Result<Option<AstNode>, JitError> {
        self.stream.skip_newlines();
        let token = match self.stream.peek() {
            Some(t) => t,
            None => return Ok(None),
        };
        let loc = self.loc();
        match token {
            Token::Newline => { self.advance()?; self.parse_statement() } // skip blank lines
            Token::Fun  => { self.advance()?; self.parse_fun_decl(false).map(Some) }
            Token::Exp  => {
                self.advance()?;
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Fun) {
                    self.advance()?;
                    self.parse_fun_decl(true).map(Some)
                } else {
                    Err(JitError::parsing(
                        "Expected 'fun' declaration after 'exp'",
                        loc.line as usize, loc.col as usize,
                    ))
                }
            }
            Token::Return => {
                self.advance()?;
                let value = if is_stmt_end(self.peek()) { None }
                            else { Some(Box::new(self.parse_expression()?)) };
                Ok(Some(AstNode::Return { value, loc }))
            }
            Token::Switch => { self.advance()?; self.parse_switch().map(Some) }
            Token::Break  => { self.advance()?; Ok(Some(AstNode::Break(loc))) }
            Token::Async  => {
                self.advance()?;
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Fun) {
                    self.advance()?;
                    self.parse_async_fun().map(Some)
                } else {
                    Err(JitError::parsing("expected 'fun' after 'async'", loc.line as usize, loc.col as usize))
                }
            }
            Token::If    => { self.advance()?; self.parse_if_stmt().map(Some) }
            Token::While => { self.advance()?; self.parse_while_loop().map(Some) }
            Token::For   => { self.advance()?; self.parse_for_loop().map(Some) }
            Token::Use   => { self.advance()?; self.parse_use_stmt().map(Some) }
            Token::Error => {
                self.advance()?;
                let loc = self.loc();
                let name = self.expect_ident()?.to_string();
                self.stream.skip_newlines();
                if self.peek() == Some(Token::LBrace) {
                    // error Name { | A | B }
                    self.advance()?;
                    let mut variants = Vec::new();
                    loop {
                        self.stream.skip_newlines();
                        if matches!(self.peek(), None | Some(Token::RBrace)) { break; }
                        self.expect(Token::Pipe)?;
                        self.stream.skip_newlines();
                        variants.push(self.expect_ident()?.to_string());
                        self.stream.skip_newlines();
                    }
                    self.expect(Token::RBrace)?;
                    if variants.is_empty() {
                        return Err(JitError::parsing(
                            "error enum must have at least one variant",
                            loc.line as usize, loc.col as usize,
                        ));
                    }
                    Ok(Some(AstNode::ErrorEnum { name, variants, loc }))
                } else {
                    // error Foo
                    Ok(Some(AstNode::ErrorDecl { name, loc }))
                }
            }
            Token::LBrace => { self.advance()?; Ok(Some(self.parse_block()?)) }
            Token::RBrace => Ok(None),

            // Identifier → might be assignment or expression statement
            Token::Identifier(id) => {
                if self.is_assignment_start() {
                    self.advance()?; // consume id
                    self.parse_assignment(id)
                } else {
                    self.parse_expression().map(|e| Some(e))
                }
            }
            _ => {
                self.parse_expression().map(|e| Some(e))
            }
        }
    }

    //  Block

    fn parse_block(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        // The opening `{` must already be consumed by the caller.
        let mut stmts = Vec::new();
        loop {
            self.stream.skip_newlines();
            if matches!(self.peek(), None | Some(Token::RBrace)) { break; }
            if let Some(s) = self.parse_statement()? { stmts.push(s); }
        }
        self.expect(Token::RBrace)?;
        Ok(AstNode::Block(stmts, loc))
    }

    //  Assignment

    /// Determine whether the current identifier starts an assignment.
    /// Peek past type-annotations, dots, and brackets to see if `=` follows.
    fn is_assignment_start(&self) -> bool {
        let mut p = self.stream.pos + 1;
        let tokens = &self.stream.tokens;
        loop {
            let Some(td) = tokens.get(p) else { return false };
            match td.token {
                Token::Newline | Token::LineComment => p += 1,
                Token::Colon => {
                    p += 1;
                    // skip the type name
                    if let Some(td2) = tokens.get(p) {
                        if matches!(td2.token, Token::Identifier(_)) {
                            p += 1;
                        }
                    }
                }
                Token::LBracket => {
                    // skip to matching ]
                    p += 1;
                    let mut depth = 1;
                    while let Some(td) = tokens.get(p) {
                        match td.token {
                            Token::LBracket => depth += 1,
                            Token::RBracket => { depth -= 1; if depth == 0 { p += 1; break; } }
                            _ => {}
                        }
                        p += 1;
                    }
                }
                Token::Dot => p += 1,
                Token::Equals => return true,
                _ => return false,
            }
        }
    }

    fn parse_assignment(&mut self, id: &'source str) -> Result<Option<AstNode>, JitError> {
        let loc = self.loc();
        // Optional type annotation
        if self.peek() == Some(Token::Colon) {
            self.advance()?; // ':'
            self.expect_ident()?; // type name, ignored
        }
        // Build the target (handle dot/bracket accessors)
        let mut target = AstNode::Ident(id.to_string(), loc);
        loop {
            match self.peek() {
                Some(Token::LBracket) => {
                    self.advance()?;
                    let index = self.parse_expression()?;
                    self.expect(Token::RBracket)?;
                    target = AstNode::Index { obj: Box::new(target), index: Box::new(index), loc: self.loc() };
                }
                Some(Token::Dot) => {
                    self.advance()?;
                    let field = self.expect_ident()?;
                    target = AstNode::Field { obj: Box::new(target), name: field.to_string(), loc: self.loc() };
                }
                _ => break,
            }
        }
        self.expect(Token::Equals)?;
        let value = self.parse_expression()?;
        Ok(Some(AstNode::Assign { target: Box::new(target), value: Box::new(value), loc }))
    }

    //  Function declaration

    fn parse_fun_decl(&mut self, exported: bool) -> Result<AstNode, JitError> {
        let loc = self.loc();
        let name = self.expect_ident()?.to_string();
        self.expect(Token::LParen)?;
        let params = self.parse_params_until(Token::RParen)?;
        self.expect(Token::RParen)?;
        // Optional return type and error kind
        self.stream.skip_newlines();
        let mut error_kind = None;
        if self.peek() == Some(Token::Arrow) {
            self.advance()?; // '->'
            self.expect_ident()?; // return type (ignored at runtime)
            self.stream.skip_newlines();
            if self.peek() == Some(Token::Not) { // '!' as error kind separator
                self.advance()?;
                error_kind = Some(self.expect_ident()?.to_string());
            }
        }
        self.stream.skip_newlines();
        self.expect(Token::LBrace)?;
        let body = self.parse_block_stmts()?;
        Ok(AstNode::FunDecl { name, params, body, exported, loc, error_kind })
    }

    fn parse_params_until(&mut self, end: Token<'source>) -> Result<Vec<String>, JitError> {
        let mut params = Vec::new();
        loop {
            self.stream.skip_newlines();
            if self.peek() == Some(end) { break; }
            if !params.is_empty() { self.expect(Token::Comma)?; self.stream.skip_newlines(); }
            if self.peek() == Some(end) { break; }
            params.push(self.expect_ident()?.to_string());
        }
        Ok(params)
    }

    //  If / else

    fn parse_if_stmt(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        self.stream.skip_newlines();
        let cond = self.parse_expression()?;
        self.stream.skip_newlines();
        self.expect(Token::LBrace)?;
        let then_block = self.parse_block_stmts()?;
        self.stream.skip_newlines();
        let else_block = if self.peek() == Some(Token::Else) {
            self.advance()?; // 'else'
            self.stream.skip_newlines();
            if self.peek() == Some(Token::If) {
                self.advance()?;
                vec![self.parse_if_stmt()?]
            } else {
                self.stream.skip_newlines();
                self.expect(Token::LBrace)?;
                self.parse_block_stmts()?
            }
        } else {
            Vec::new()
        };
        Ok(AstNode::If { cond: Box::new(cond), then_block, else_block, loc })
    }

    //  While loop

    fn parse_while_loop(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        self.stream.skip_newlines();
        let cond = self.parse_expression()?;
        self.stream.skip_newlines();
        self.expect(Token::LBrace)?;
        let body = self.parse_block_stmts()?;
        Ok(AstNode::While { cond: Box::new(cond), body, loc })
    }

    //  For loop

    fn parse_for_loop(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        self.stream.skip_newlines();
        let var = self.expect_ident()?.to_string();
        self.expect(Token::In)?;
        let iter = self.parse_expression()?;
        self.stream.skip_newlines();
        self.expect(Token::LBrace)?;
        let body = self.parse_block_stmts()?;
        Ok(AstNode::For { var, iter: Box::new(iter), body, loc })
    }

    //  Use statement

    fn parse_use_stmt(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        let mut path = vec![self.expect_ident()?.to_string()];
        loop {
            if self.peek() == Some(Token::Dot) {
                self.advance()?;
                path.push(self.expect_ident()?.to_string());
            } else { break; }
        }
        Ok(AstNode::Use { path, loc })
    }

    //  Switch statement

    fn parse_switch(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        self.stream.skip_newlines();
        let expr = self.parse_expression()?;
        self.stream.skip_newlines();
        self.expect(Token::LBrace)?;
        let mut arms = Vec::new();
        loop {
            self.stream.skip_newlines();
            if matches!(self.peek(), None | Some(Token::RBrace)) { break; }
            let _arm_loc = self.loc();
            // Parse patterns (value | value | ...)
            let mut patterns = Vec::new();
            if self.peek() == Some(Token::Identifier("_")) {
                self.advance()?; // wildcard — empty patterns = default
            } else {
                loop {
                    patterns.push(self.parse_expression()?);
                    self.stream.skip_newlines();
                    if self.peek() == Some(Token::Pipe) {
                        self.advance()?;
                    } else { break; }
                }
            }
            self.stream.skip_newlines();
            self.expect(Token::Arrow)?;
            // Body: either a block or an expression
            self.stream.skip_newlines();
            let body = if self.peek() == Some(Token::LBrace) {
                self.parse_block_stmts()?
            } else {
                vec![self.parse_expression()?]
            };
            arms.push(SwitchArm { patterns, body });
        }
        self.expect(Token::RBrace)?;
        Ok(AstNode::Switch { expr: Box::new(expr), arms, loc })
    }

    //  Async function

    fn parse_async_fun(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        let name = self.expect_ident()?.to_string();
        self.expect(Token::LParen)?;
        let params = self.parse_params_until(Token::RParen)?;
        self.expect(Token::RParen)?;
        self.stream.skip_newlines();
        if self.peek() == Some(Token::Arrow) {
            self.advance()?;
            self.expect_ident()?;
        }
        self.expect(Token::LBrace)?;
        let body = self.parse_block_stmts()?;
        Ok(AstNode::AsyncFun { name, params, body, loc })
    }

    // ══════════════════════════════════════════════════════════════════════
    //  Expression parsing (recursive descent with precedence)
    // ══════════════════════════════════════════════════════════════════════

    fn parse_expression(&mut self) -> Result<AstNode, JitError> {
        self.parse_fallthrough_expr()
    }

    // ── Fallthrough (or / except) — lowest precedence

    fn parse_fallthrough_expr(&mut self) -> Result<AstNode, JitError> {
        let lhs = self.parse_range_expr()?;
        let loc = self.loc();

        // `or` — inline fallback for failures (disambiguates from boolean
        // `or` at runtime via TAG_FAILURE — here it's the same token)
        if self.peek() == Some(Token::Or) {
            self.advance()?;
            let rhs = self.parse_fallthrough_expr()?;
            return Ok(AstNode::Fallback { expr: Box::new(lhs), default: Box::new(rhs), loc });
        }

        // `except` — pattern matching on failure types
        if self.peek() == Some(Token::Except) {
            self.advance()?;
            self.stream.skip_newlines();
            self.expect(Token::LBrace)?;
            let mut arms = Vec::new();
            loop {
                self.stream.skip_newlines();
                if matches!(self.peek(), None | Some(Token::RBrace)) { break; }
                self.expect(Token::Pipe)?;
                self.stream.skip_newlines();
                let type_name = if self.peek() == Some(Token::Identifier("_")) {
                    self.advance()?;
                    String::new()
                } else {
                    self.expect_ident()?.to_string()
                };
                self.stream.skip_newlines();
                self.expect(Token::Arrow)?;
                self.stream.skip_newlines();
                let body = if self.peek() == Some(Token::LBrace) {
                    self.parse_block_stmts()?
                } else {
                    vec![self.parse_expression()?]
                };
                arms.push(ExceptArm { type_name, body });
            }
            self.expect(Token::RBrace)?;
            return Ok(AstNode::Except { expr: Box::new(lhs), arms, loc });
        }

        Ok(lhs)
    }

    //  Range `..` (lowest‑precedence binary)

    fn parse_range_expr(&mut self) -> Result<AstNode, JitError> {
        let lhs = self.parse_or_expr()?;
        if self.peek() == Some(Token::Range) {
            let loc = self.loc();
            self.advance()?; // '..'
            let rhs = self.parse_or_expr()?;
            // Check for .step(N) after the range
            if self.peek() == Some(Token::Dot) {
                // Lookahead: peek at the next token to see if it's "step"
                let saved = self.stream.pos;
                self.advance()?; // '.'
                if self.peek() == Some(Token::Identifier("step")) {
                    self.advance()?; // 'step'
                    if self.peek() == Some(Token::LParen) {
                        self.advance()?; // '('
                        let step = self.parse_expression()?;
                        self.expect(Token::RParen)?;
                        return Ok(AstNode::Range {
                            start: Box::new(lhs),
                            end: Box::new(rhs),
                            step: Some(Box::new(step)),
                            loc,
                        });
                    }
                }
                // Not .step(N) — backtrack: restore position
                self.stream.pos = saved;
            }
            Ok(AstNode::Range {
                start: Box::new(lhs),
                end: Box::new(rhs),
                step: None,
                loc,
            })
        } else {
            Ok(lhs)
        }
    }

    //  or (short-circuit)

    fn parse_or_expr(&mut self) -> Result<AstNode, JitError> {
        let mut lhs = self.parse_and_expr()?;
        let loc = self.loc();
        while self.peek() == Some(Token::Or) {
            self.advance()?;
            let rhs = self.parse_and_expr()?;
            lhs = AstNode::Binary { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
        }
        Ok(lhs)
    }

    //  and (short-circuit)

    fn parse_and_expr(&mut self) -> Result<AstNode, JitError> {
        let mut lhs = self.parse_comp_expr()?;
        let loc = self.loc();
        while self.peek() == Some(Token::And) {
            self.advance()?;
            let rhs = self.parse_comp_expr()?;
            lhs = AstNode::Binary { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
        }
        Ok(lhs)
    }

    //  Comparisons

    fn parse_comp_expr(&mut self) -> Result<AstNode, JitError> {
        let mut lhs = self.parse_add_expr()?;
        loop {
            let loc = self.loc();
            let op = match self.peek() {
                Some(Token::Eq)  => { self.advance()?; Some(BinOp::Eq) }
                Some(Token::Ne)  => { self.advance()?; Some(BinOp::Ne) }
                Some(Token::Lt)  => { self.advance()?; Some(BinOp::Lt) }
                Some(Token::Le)  => { self.advance()?; Some(BinOp::Le) }
                Some(Token::Gt)  => { self.advance()?; Some(BinOp::Gt) }
                Some(Token::Ge)  => { self.advance()?; Some(BinOp::Ge) }
                _ => None,
            };
            match op {
                Some(op) => {
                    let rhs = self.parse_add_expr()?;
                    lhs = AstNode::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                None => break,
            }
        }
        Ok(lhs)
    }

    //  Additive

    fn parse_add_expr(&mut self) -> Result<AstNode, JitError> {
        let mut lhs = self.parse_mul_expr()?;
        loop {
            let loc = self.loc();
            match self.peek() {
                Some(Token::Plus)  => { self.advance()?;
                    let rhs = self.parse_mul_expr()?;
                    lhs = AstNode::Binary { op: BinOp::Add, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                Some(Token::Minus) => { self.advance()?;
                    let rhs = self.parse_mul_expr()?;
                    lhs = AstNode::Binary { op: BinOp::Sub, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    //  Multiplicative

    fn parse_mul_expr(&mut self) -> Result<AstNode, JitError> {
        let mut lhs = self.parse_unary_expr()?;
        loop {
            let loc = self.loc();
            match self.peek() {
                Some(Token::Mul)  => { self.advance()?;
                    let rhs = self.parse_unary_expr()?;
                    lhs = AstNode::Binary { op: BinOp::Mul, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                Some(Token::Div)  => { self.advance()?;
                    let rhs = self.parse_unary_expr()?;
                    lhs = AstNode::Binary { op: BinOp::Div, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                Some(Token::Mod)  => { self.advance()?;
                    let rhs = self.parse_unary_expr()?;
                    lhs = AstNode::Binary { op: BinOp::Mod, lhs: Box::new(lhs), rhs: Box::new(rhs), loc };
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    //  Unary

    fn parse_unary_expr(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        match self.peek() {
            Some(Token::Not)   => { self.advance()?;
                let expr = self.parse_unary_expr()?;
                Ok(AstNode::Unary { op: UnaryOp::Not, expr: Box::new(expr), loc })
            }
            Some(Token::Minus) => { self.advance()?;
                let expr = self.parse_unary_expr()?;
                Ok(AstNode::Unary { op: UnaryOp::Neg, expr: Box::new(expr), loc })
            }
            Some(Token::Fail) => {
                self.advance()?;
                let mut path = vec![self.expect_ident()?.to_string()];
                while self.peek() == Some(Token::Dot) {
                    self.advance()?;
                    path.push(self.expect_ident()?.to_string());
                }
                let type_name = path.join(".");
                Ok(AstNode::Fail { type_name, loc })
            }
            _ => self.parse_postfix_expr(),
        }
    }

    //  Postfix (calls, indexing, field access, ranges)

    fn parse_postfix_expr(&mut self) -> Result<AstNode, JitError> {
        let mut left = self.parse_primary()?;
        loop {
            let loc = self.loc();
            match self.peek() {
                // obj(args) → dynamic call
                Some(Token::LParen) => {
                    self.advance()?;
                    let args = self.parse_call_args()?;
                    // If left is an Ident, emit a static FunCall
                    // Otherwise it's a DynamicCall
                    if let AstNode::Ident(name, _) = &left {
                        left = AstNode::FunCall {
                            name: name.clone(),
                            args,
                            loc,
                        };
                    } else {
                        left = AstNode::DynamicCall {
                            callee: Box::new(left),
                            args,
                            loc,
                        };
                    }
                }
                // obj[index]
                Some(Token::LBracket) => {
                    self.advance()?;
                    let index = self.parse_expression()?;
                    self.expect(Token::RBracket)?;
                    left = AstNode::Index { obj: Box::new(left), index: Box::new(index), loc };
                }
                // obj.field or obj.method()
                Some(Token::Dot) => {
                    self.advance()?;
                    let field = self.expect_ident()?.to_string();
                    if self.peek() == Some(Token::LParen) {
                        // obj.method(args)
                        self.advance()?;
                        let args = self.parse_call_args()?;
                        left = AstNode::MethodCall {
                            obj: Box::new(left),
                            method: field,
                            args,
                            loc,
                        };
                    } else {
                        left = AstNode::Field { obj: Box::new(left), name: field, loc };
                    }
                }
                _ => break,
            }
        }
        Ok(left)
    }

    //  Primary expressions

    fn parse_primary(&mut self) -> Result<AstNode, JitError> {
        let loc = self.loc();
        match self.advance()? {
            Token::LParen => {
                let inner = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(inner)
            }
            Token::Number(n) => Ok(AstNode::Number(n, loc)),
            Token::Bool(b)   => Ok(AstNode::Bool(b, loc)),
            Token::Nil       => Ok(AstNode::Nil(loc)),
            Token::String(s) => Ok(AstNode::Str(unescape_string(s), loc)),
            Token::Template(s) => self.parse_template(s, loc),
            Token::Identifier(id) => Ok(AstNode::Ident(id.to_string(), loc)),

            // List literal
            Token::LBracket => self.parse_list_lit(loc),
            // Object literal
            Token::LBrace => self.parse_object_lit(loc),
            // Closure
            Token::Await => {
                // Token already consumed by match self.advance()? above
                let expr = self.parse_expression()?;
                Ok(AstNode::Await(Box::new(expr), loc))
            }
            Token::Pipe => self.parse_closure(false, loc),
            Token::Move => {
                self.expect(Token::Pipe)?;
                self.parse_closure(true, loc)
            }
            t => Err(JitError::parsing(
                format!("Expected expression, found {:?}", t),
                loc.line as usize, loc.col as usize,
            )),
        }
    }

    //  List literal

    fn parse_list_lit(&mut self, loc: Loc) -> Result<AstNode, JitError> {
        self.stream.skip_newlines();
        if self.peek() == Some(Token::RBracket) {
            self.advance()?;
            return Ok(AstNode::ListLit(vec![], loc));
        }
        let first = self.parse_expression()?;
        self.stream.skip_newlines();
        // Check for [val; count]
        if self.peek() == Some(Token::Semicolon) {
            self.advance()?;
            self.stream.skip_newlines();
            let count = self.parse_expression()?;
            self.stream.skip_newlines();
            self.expect(Token::RBracket)?;
            return Ok(AstNode::ListRepeat { val: Box::new(first), count: Box::new(count), loc });
        }
        let mut elems = vec![first];
        if self.peek() == Some(Token::Comma) {
            self.advance()?;
            self.stream.skip_newlines();
            if self.peek() != Some(Token::RBracket) {
                loop {
                    elems.push(self.parse_expression()?);
                    self.stream.skip_newlines();
                    if self.peek() == Some(Token::Comma) {
                        self.advance()?;
                        if self.peek() == Some(Token::RBracket) { break; }
                    } else { break; }
                }
            }
        }
        self.expect(Token::RBracket)?;
        Ok(AstNode::ListLit(elems, loc))
    }

    //  Object literal

    fn parse_object_lit(&mut self, loc: Loc) -> Result<AstNode, JitError> {
        let mut fields = Vec::new();
        self.stream.skip_newlines();
        if self.peek() != Some(Token::RBrace) {
            loop {
                self.stream.skip_newlines();
                let name = self.expect_ident()?.to_string();
                self.stream.skip_newlines();
                self.expect(Token::Colon)?;
                let val = self.parse_expression()?;
                fields.push((name, val));
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Comma) {
                    self.advance()?;
                    if self.peek() == Some(Token::RBrace) { break; }
                } else { break; }
            }
        }
        self.expect(Token::RBrace)?;
        Ok(AstNode::ObjectLit(fields, loc))
    }

    //  Closure

    fn parse_closure(&mut self, is_move: bool, loc: Loc) -> Result<AstNode, JitError> {
        let params = self.parse_params_until(Token::Pipe)?;
        self.expect(Token::Pipe)?; // consume closing '|'
        let body = if self.peek() == Some(Token::LBrace) {
            self.advance()?; // consume '{'
            self.parse_block()?
        } else {
            self.parse_expression()?
        };
        Ok(AstNode::Closure { params, body: Box::new(body), is_move, loc })
    }

    //  Template literals

    fn parse_template(&self, s: &'source str, loc: Loc) -> Result<AstNode, JitError> {
        // Templates are handled the same way as the existing parser.
        // For now, treat them as plain string literals (simplified).
        Ok(AstNode::Str(s.to_string(), loc))
    }

    //  Call args

    fn parse_call_args(&mut self) -> Result<Vec<AstNode>, JitError> {
        let mut args = Vec::new();
        self.stream.skip_newlines();
        if self.peek() != Some(Token::RParen) {
            loop {
                args.push(self.parse_expression()?);
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Comma) {
                    self.advance()?;
                } else { break; }
            }
        }
        self.expect(Token::RParen)?;
        Ok(args)
    }
}
