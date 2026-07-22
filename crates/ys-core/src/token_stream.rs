//! Token stream management for the parser.
//!
//! Wraps the raw token vector produced by the lexer and provides
//! convenience methods for advancing, peeking, and skipping.

use crate::compiler::Loc;
use crate::error::JitError;
use crate::lexer::Token;
use logos::Logos;

#[derive(Clone, Copy)]
pub struct VarInfo {
    pub idx: usize,
    pub is_mut: bool,
    pub is_global: bool,
    pub first_line: usize,
}

pub struct TokenData<'source> {
    pub token: Token<'source>,
    pub loc: Loc,
}

/// A cursor over the lexed token stream.
///
/// Owns the token vector and current position. The parser
/// holds one of these and delegates navigation to it.
pub struct TokenStream<'source> {
    pub(crate) tokens: Vec<TokenData<'source>>,
    pub(crate) pos: usize,
}

impl<'source> TokenStream<'source> {
    pub fn new(tokens: Vec<TokenData<'source>>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Lex source code into a vector of token data.
    pub fn lex_all(input: &'source str) -> Result<Vec<TokenData<'source>>, JitError> {
        let mut lexer = Token::lexer(input);
        let mut tokens = Vec::with_capacity(input.len() / 4);
        let mut line = 1;
        let mut line_start = 0;
        let mut last_span_end = 0;

        while let Some(res) = lexer.next() {
            let span = lexer.span();

            // Count newlines in the gap (skipped whitespace/comments)
            let gap = &input[last_span_end..span.start];
            for (i, c) in gap.char_indices() {
                if c == '\n' {
                    line += 1;
                    line_start = last_span_end + i + 1;
                }
            }

            let loc = Loc {
                line: line as u32,
                col: (span.start - line_start + 1) as u32,
            };

            match res {
                Ok(t) => {
                    if t == Token::Newline {
                        tokens.push(TokenData { token: t, loc });
                        line += 1;
                        line_start = span.end;
                    } else if !matches!(t, Token::LineComment(_)) {
                        tokens.push(TokenData { token: t, loc });
                    }
                }
                Err(e) => {
                    return Err(JitError::Lexing {
                        err: e,
                        loc: crate::error::ErrorLoc::new(line, span.start - line_start + 1),
                    });
                }
            }
            last_span_end = span.end;
        }
        Ok(tokens)
    }

    /// Advance the stream and return the current token.
    /// Returns a parsing error on EOF.
    pub fn advance(&mut self) -> Result<Token<'source>, JitError> {
        if let Some(td) = self.tokens.get(self.pos) {
            self.pos += 1;
            Ok(td.token)
        } else {
            let loc = self.loc();
            let msg = if loc.line > 0 {
                "Unexpected EOF — reached end of file with unclosed blocks or expressions"
                    .to_string()
            } else {
                "Unexpected EOF — empty source or all tokens skipped".to_string()
            };
            Err(JitError::parsing(msg, loc.as_error_pos()))
        }
    }

    /// Peek the current token without advancing.
    #[inline(always)]
    pub fn peek(&self) -> Option<Token<'source>> {
        self.tokens.get(self.pos).map(|td| td.token)
    }

    /// Peek n tokens ahead without advancing.
    #[inline(always)]
    pub fn peek_n(&self, n: usize) -> Option<Token<'source>> {
        self.tokens.get(self.pos + n).map(|td| td.token)
    }

    /// Get the location of the current position.
    /// Falls back to the last token's location, or `Loc { line: 1, col: 1 }`.
    #[inline(always)]
    pub fn loc(&self) -> Loc {
        self.tokens
            .get(self.pos)
            .map(|td| td.loc)
            .unwrap_or_else(|| {
                self.tokens
                    .last()
                    .map(|td| td.loc)
                    .unwrap_or(Loc { line: 1, col: 1 })
            })
    }

    /// Skip over consecutive [`Token::Newline`] and [`Token::LineComment`] tokens.
    pub fn skip_newlines(&mut self) {
        while matches!(
            self.peek(),
            Some(Token::Newline) | Some(Token::LineComment(_))
        ) {
            self.advance().ok();
        }
    }

    /// Advance and assert the current token matches `expected`.
    /// Returns a parsing error on mismatch.
    pub fn expect(&mut self, expected: Token<'source>) -> Result<(), JitError> {
        let loc = self.loc();
        let t = self.advance()?;
        if t == expected {
            Ok(())
        } else {
            Err(JitError::parsing(
                format!("Expected {:?}, found {:?}", expected, t),
                loc.as_error_pos(),
            ))
        }
    }
}
