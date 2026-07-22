//! Recursive‑descent parser that builds an AST instead of emitting bytecode.
//! The grammar is identical to the direct‑emit parser in [`parser.rs`].

use crate::ast::*;
use crate::compiler::Loc;
use crate::error::JitError;
use crate::lexer::Token;
use crate::token_stream::TokenStream;
use rustc_hash::FxHashMap;

pub(crate) mod expr;
pub(crate) mod stmt;

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

/// Parse a comma-separated list of parameters up to a closing delimiter token.
/// Supports optional `: TypeName` type annotations (parsed but ignored at runtime).
pub(crate) fn parse_params_until<'source>(
    parser: &mut AstParser<'source>,
    end: Token<'source>,
) -> Result<Vec<FuncParam>, JitError> {
    let mut params = Vec::new();
    loop {
        parser.stream.skip_newlines();
        if parser.peek() == Some(end) {
            break;
        }
        if !params.is_empty() {
            parser.expect(Token::Comma)?;
            parser.stream.skip_newlines();
        }
        if parser.peek() == Some(end) {
            break;
        }
        // `.name` = rest positional, `..name` = kwargs
        let (is_rest, is_kwargs) = match parser.peek() {
            Some(Token::Range) => { parser.advance()?; (false, true) }
            Some(Token::Dot) => { parser.advance()?; (true, false) }
            _ => (false, false),
        };
        let name = parser.expect_ident()?.to_string();
        // Only regular params can have defaults/type annotations
        let default = if !is_rest && !is_kwargs && parser.peek() == Some(Token::Colon) {
            parser.advance()?;
            Some(Box::new(parser.parse_expression()?))
        } else {
            None
        };
        params.push(FuncParam { name, default, is_rest, is_kwargs });
    }
    Ok(params)
}

/// Recursive‑descent parser that produces an AST.
pub struct AstParser<'source> {
    pub stream: TokenStream<'source>,
}

impl<'source> AstParser<'source> {
    pub fn new(input: &'source str) -> Result<Self, JitError> {
        let tokens = TokenStream::lex_all(input)?;
        Ok(Self {
            stream: TokenStream::new(tokens),
        })
    }

    //  Entry point

    pub fn parse_program(&mut self) -> Result<AstBlock, JitError> {
        let mut stmts = Vec::new();
        loop {
            self.stream.skip_newlines();
            if self.stream.peek().is_none() {
                break;
            }
            if let Some(s) = self.parse_statement()? {
                stmts.push(s);
            }
        }
        Ok(stmts)
    }

    //  Helpers

    fn loc(&self) -> Loc {
        self.stream.loc()
    }

    fn advance(&mut self) -> Result<Token<'source>, JitError> {
        self.stream.advance()
    }

    fn peek(&self) -> Option<Token<'source>> {
        self.stream.peek()
    }

    fn expect(&mut self, t: Token<'source>) -> Result<(), JitError> {
        self.stream.expect(t)
    }

    fn expect_ident(&mut self) -> Result<&'source str, JitError> {
        let loc = self.loc();
        match self.advance()? {
            Token::Identifier(id) => Ok(id),
            t => Err(JitError::parsing(
                format!("Expected identifier, found {:?}", t),
                loc.as_error_pos(),
            )),
        }
    }

    /// Parse a `{ … }` block and return its statement list.
    /// If the result is a single-node block, it is flattened to a `Vec`.
    fn parse_block_stmts(&mut self) -> Result<AstBlock, JitError> {
        match stmt::parse_block(self)? {
            AstNode::Block(s, _) => Ok(s),
            other => Ok(vec![other]),
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
            Token::Newline => {
                self.advance()?;
                self.parse_statement()
            } // skip blank lines
            Token::At => {
                self.advance()?;
                let name = self.expect_ident()?.to_string();
                let (args, named) = if self.peek() == Some(Token::LParen) {
                    self.advance()?;
                    expr::parse_call_args(self)?
                } else {
                    (Vec::new(), FxHashMap::default())
                };
                let inner = self.parse_statement()?.ok_or_else(|| {
                    JitError::parsing("Expected function after decorator", loc.as_error_pos())
                })?;
                Ok(Some(AstNode::Decorator { name, args, named, inner: Box::new(inner), loc }))
            }
            Token::Fun => {
                self.advance()?;
                stmt::parse_fun_decl(self, false).map(Some)
            }
            Token::Exp => {
                self.advance()?;
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Fun) {
                    self.advance()?;
                    stmt::parse_fun_decl(self, true).map(Some)
                } else {
                    Err(JitError::parsing(
                        "Expected 'fun' declaration after 'exp'",
                        loc.as_error_pos(),
                    ))
                }
            }
            Token::Ret => {
                self.advance()?;
                let value = if is_stmt_end(self.peek()) {
                    None
                } else {
                    Some(Box::new(self.parse_expression()?))
                };
                Ok(Some(AstNode::Return { value, loc }))
            }
            Token::Yield => {
                self.advance()?;
                let value = self.parse_expression()?;
                Ok(Some(AstNode::Yield(Box::new(value), loc)))
            }
            Token::Switch => {
                self.advance()?;
                stmt::parse_switch(self).map(Some)
            }
            Token::Break => {
                self.advance()?;
                Ok(Some(AstNode::Break(loc)))
            }
            Token::Async => {
                self.advance()?;
                self.stream.skip_newlines();
                if self.peek() == Some(Token::Fun) {
                    self.advance()?;
                    stmt::parse_async_fun(self).map(Some)
                } else {
                    Err(JitError::parsing(
                        "expected 'fun' after 'async'",
                        loc.as_error_pos(),
                    ))
                }
            }
            Token::If => {
                self.advance()?;
                stmt::parse_if_stmt(self).map(Some)
            }
            Token::While => {
                self.advance()?;
                stmt::parse_while_loop(self).map(Some)
            }
            Token::For => {
                self.advance()?;
                stmt::parse_for_loop(self).map(Some)
            }
            Token::Use => {
                self.advance()?;
                stmt::parse_use_stmt(self).map(Some)
            }
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
                        if matches!(self.peek(), None | Some(Token::RBrace)) {
                            break;
                        }
                        self.expect(Token::Pipe)?;
                        self.stream.skip_newlines();
                        variants.push(self.expect_ident()?.to_string());
                        self.stream.skip_newlines();
                    }
                    self.expect(Token::RBrace)?;
                    if variants.is_empty() {
                        return Err(JitError::parsing(
                            "error enum must have at least one variant",
                            loc.as_error_pos(),
                        ));
                    }
                    Ok(Some(AstNode::ErrorEnum {
                        name,
                        variants,
                        loc,
                    }))
                } else {
                    // error Foo
                    Ok(Some(AstNode::ErrorDecl { name, loc }))
                }
            }
            Token::LBrace => {
                self.advance()?;
                Ok(Some(stmt::parse_block(self)?))
            }
            Token::RBrace => Ok(None),

            // Identifier → might be assignment or expression statement
            Token::Identifier(id) => {
                if stmt::is_assignment_start(self) {
                    self.advance()?; // consume id
                    stmt::parse_assignment(self, id)
                } else {
                    self.parse_expression().map(Some)
                }
            }
            _ => self.parse_expression().map(Some),
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    //  Expression parsing (recursive descent with precedence)
    // ══════════════════════════════════════════════════════════════════════

    fn parse_expression(&mut self) -> Result<AstNode, JitError> {
        expr::parse_fallthrough_expr(self)
    }
}
