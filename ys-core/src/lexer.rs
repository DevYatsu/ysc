//! Lexical analysis for the ysc language.
//!
//! This module is built on top of the [`logos`] crate, which generates a
//! highly optimised DFA-based scanner from the token definitions below.
//!
//! The lexer produces a flat stream of [`Token`]s.  The [`parser`][crate::parser]
//! then drives the lexer and converts the token stream into bytecode.
//!
//! # Character set
//!
//! ysc source files are expected to be valid ASCII.  Non-ASCII characters
//! (e.g. Unicode letters) produce a [`LexingError::NonAsciiCharacter`] error.
//!
//! # Comments
//!
//! - **Line comments** start with `//` and run to the end of the line.
//! - **Block comments** are delimited by `/*` … `*/` and may not be nested.
//!
//! Both are emitted as [`Token::LineComment`] / skipped respectively; the
//! parser discards them.

use std::fmt;
use std::num::{ParseFloatError, ParseIntError};

use logos::Logos;

/// Errors that can arise during the lexical analysis phase.
///
/// These are produced by the [`logos`] scanner and subsequently wrapped in
/// [`JitError::Lexing`][crate::error::JitError::Lexing] by the parser.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum LexingError {
    /// An integer literal could not be parsed (e.g. overflow).
    InvalidInteger,
    /// A floating-point literal could not be parsed (e.g. overflow).
    InvalidFloat,
    /// A character that is not part of the ysc character set was encountered.
    /// The offending `char` is captured for use in diagnostics.
    NonAsciiCharacter(char),
    /// A catch-all for any other lexer error (should be rare in practice).
    #[default]
    Other,
}

impl LexingError {
    /// Called by the logos scanner when it encounters an unrecognised input.
    /// Inspects the raw slice to determine whether the culprit is a non-ASCII
    /// character or something else entirely.
    fn from_lexer<'a>(lex: &mut logos::Lexer<'a, Token<'a>>) -> Self {
        match lex.slice().chars().next() {
            Some(c) => LexingError::NonAsciiCharacter(c),
            None => LexingError::Other,
        }
    }
}

/// Every terminal symbol in the ysc grammar.
///
/// The lifetime `'source` is tied to the original source string, allowing
/// string/identifier tokens to borrow their slices without copying.
///
/// # Skipping rules
///
/// The lexer automatically skips:
/// - Horizontal whitespace (space, tab, form-feed).
/// - Block comments (`/* … */`).
///
/// Newlines and line comments are *not* skipped — newlines act as statement
/// terminators and are significant in the grammar.
#[derive(Logos, Debug, PartialEq, Clone, Copy)]
#[logos(error(LexingError, LexingError::from_lexer))]
#[logos(skip r"[ \t\f]+")]
#[logos(skip r"/\*(?:[^*]|\*[^/])*\*/")]
#[logos(skip(r"#[^\n]*", allow_greedy = true))]
pub enum Token<'source> {
    // -- Keywords -----------------------------------------------------------

    /// 'and' boolean AND operator.
    #[token("and")]
    And,
    /// 'continue' keyword (reserved for future use).
    #[token("continue")]
    Continue,
    /// 'else' keyword.
    #[token("else")]
    Else,
    /// 'error' keyword for declaring error kinds.
    #[token("error")]
    Error,
    /// 'exp' export visibility modifier.
    #[token("exp")]
    Exp,
    /// 'except' keyword for failure pattern matching.
    #[token("except")]
    Except,
    /// 'for' loop keyword.
    #[token("for")]
    For,
    /// 'fail' keyword for producing tagged failures.
    #[token("fail")]
    Fail,
    /// 'fun' function declaration keyword.
    #[token("fun")]
    Fun,
    /// 'async' keyword for async functions.
    #[token("async")]
    Async,
    /// 'await' keyword for awaiting promises.
    #[token("await")]
    Await,
    /// 'switch' keyword.
    #[token("switch")]
    Switch,
    /// 'break' keyword (inside switch/match).
    #[token("break")]
    Break,
    /// 'if' keyword.
    #[token("if")]
    If,
    /// 'in' keyword for iterators.
    #[token("in")]
    In,
    /// 'move' closure capture-by-value keyword.
    #[token("move")]
    Move,
    /// 'nil' null value literal.
    #[token("nil")]
    Nil,
    /// 'or' boolean OR operator.
    #[token("or")]
    Or,
    /// 'ret' keyword (returns a value from a function or closure).
    #[token("ret")]
    Ret,
    /// 'super' parent module path.
    #[token("super")]
    Super,
    /// 'use' module import keyword.
    #[token("use")]
    Use,
    /// 'while' loop keyword.
    #[token("while")]
    While,
    /// 'yield' keyword for generator functions.
    #[token("yield")]
    Yield,

    // -- Punctuation / operators --------------------------------------------
    // Multi-character tokens must precede single-character tokens that share
    // a prefix so that logos matches the longer variant first.

    /// '..' range operator (before '.').
    #[token("..")]
    Range,
    /// ':' separator.
    #[token(":")]
    Colon,
    /// '\n' line separator.
    #[token("\n")]
    Newline,
    /// '{' opening brace.
    #[token("{")]
    LBrace,
    /// '}' closing brace.
    #[token("}")]
    RBrace,
    /// '(' opening parenthesis.
    #[token("(")]
    LParen,
    /// ')' closing parenthesis.
    #[token(")")]
    RParen,
    /// '[' opening bracket.
    #[token("[")]
    LBracket,
    /// ']' closing bracket.
    #[token("]")]
    RBracket,
    /// ',' separator.
    #[token(",")]
    Comma,
    /// ';' separator for list repetition syntax `[val; count]`.
    #[token(";")]
    Semicolon,
    /// '.' operator for object property access.
    #[token(".")]
    Dot,
    /// '+=' compound assignment (before '+').
    #[token("+=")]
    PlusEq,
    /// '+' operator.
    #[token("+")]
    Plus,
    /// '-=' compound assignment (before '-').
    #[token("-=")]
    MinusEq,
    /// '->' arrow for return type annotations (before '-').
    #[token("->")]
    Arrow,
    /// '-' operator.
    #[token("-")]
    Minus,
    /// '*=' compound assignment (before '*').
    #[token("*=")]
    MulEq,
    /// '*' operator.
    #[token("*")]
    Mul,
    /// '/=' compound assignment (before '/').
    #[token("/=")]
    DivEq,
    /// '/' operator.
    #[token("/")]
    Div,
    /// '%=' compound assignment (before '%').
    #[token("%=")]
    ModEq,
    /// '%' modulus operator.
    #[token("%")]
    Mod,
    /// '==' equality operator (before '=').
    #[token("==")]
    Eq,
    /// '=' assignment operator.
    #[token("=")]
    Equals,
    /// '!=' not-equal operator (before '!').
    #[token("!=")]
    Ne,
    /// '<=' less-or-equal operator (before '<').
    #[token("<=")]
    Le,
    /// '<' operator.
    #[token("<")]
    Lt,
    /// '>=' greater-or-equal operator (before '>').
    #[token(">=")]
    Ge,
    /// '>' operator.
    #[token(">")]
    Gt,
    /// '!' logical NOT operator.
    #[token("!")]
    Not,
    /// '|' pipe delimiter for closure parameter lists.
    #[token("|")]
    Pipe,

    // -- Literals -----------------------------------------------------------

    /// Boolean literals.
    #[token("false", |_| false)]
    #[token("true", |_| true)]
    Bool(bool),

    /// Numeric literals (integers and floats).
    #[regex(
        r"-?(?:0|[1-9]\d*)(?:_\d+)*(?:\.(?:\d+(?:_\d+)*))?(?:[eE][+-]?\d+(?:_\d+)*)?",
        |lex| lex.slice().replace("_", "").parse::<f64>()
    )]
    Number(f64),

    /// String literals enclosed in double quotes.
    #[regex(r#""([^"\\\x00-\x1F]|\\(["\\bnfrt/]|u[a-fA-F0-9]{4}))*""#, |lex| {
        let  s = lex.slice();
        &s[1..s.len()-1]
    })]
    String(&'source str),

    /// Identifier names.  Note: keywords like `and` and `or` are matched
    /// before this regex by virtue of their `#[token(…)]` definitions above.
    #[regex(r"[[:alpha:]_][[:alpha:]0-9_]*", |lex| lex.slice())]
    Identifier(&'source str),

    /// Template literals enclosed in backticks.
    #[regex(r#"`([^`\\]|\\.)*`"#, |lex| {
        let s = lex.slice();
        &s[1..s.len()-1]
    })]
    Template(&'source str),

    /// Double-slash line comments.
    #[regex(r"//[^\n]*", allow_greedy = true)]
    LineComment(&'source str),
}

impl fmt::Display for LexingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexingError::InvalidInteger => write!(f, "Invalid integer"),
            LexingError::InvalidFloat => write!(f, "Invalid float"),
            LexingError::NonAsciiCharacter(c) => write!(f, "Non-ASCII character: {}", c),
            LexingError::Other => write!(f, "Unknown lexing error"),
        }
    }
}

impl std::error::Error for LexingError {}

impl From<ParseIntError> for LexingError {
    fn from(_: ParseIntError) -> Self {
        LexingError::InvalidInteger
    }
}

impl From<ParseFloatError> for LexingError {
    fn from(_err: ParseFloatError) -> Self {
        LexingError::InvalidFloat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lexer_keywords() {
        let input = "fun ret continue if else for while in use super exp move and or";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Fun)));
        assert_eq!(lexer.next(), Some(Ok(Token::Ret)));
        assert_eq!(lexer.next(), Some(Ok(Token::Continue)));
        assert_eq!(lexer.next(), Some(Ok(Token::If)));
        assert_eq!(lexer.next(), Some(Ok(Token::Else)));
        assert_eq!(lexer.next(), Some(Ok(Token::For)));
        assert_eq!(lexer.next(), Some(Ok(Token::While)));
        assert_eq!(lexer.next(), Some(Ok(Token::In)));
        assert_eq!(lexer.next(), Some(Ok(Token::Use)));
        assert_eq!(lexer.next(), Some(Ok(Token::Super)));
        assert_eq!(lexer.next(), Some(Ok(Token::Exp)));
        assert_eq!(lexer.next(), Some(Ok(Token::Move)));
        assert_eq!(lexer.next(), Some(Ok(Token::And)));
        assert_eq!(lexer.next(), Some(Ok(Token::Or)));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_literals() {
        let input = "true false 123 123.456 1_000 \"hello world\" identifier";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Bool(true))));
        assert_eq!(lexer.next(), Some(Ok(Token::Bool(false))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(123.0))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(123.456))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(1000.0))));
        assert_eq!(lexer.next(), Some(Ok(Token::String("hello world"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("identifier"))));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_symbols() {
        let input = ": = { } ( ) [ ] , . + - * / == != < <= > >= ! | ->";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Colon)));
        assert_eq!(lexer.next(), Some(Ok(Token::Equals)));
        assert_eq!(lexer.next(), Some(Ok(Token::LBrace)));
        assert_eq!(lexer.next(), Some(Ok(Token::RBrace)));
        assert_eq!(lexer.next(), Some(Ok(Token::LParen)));
        assert_eq!(lexer.next(), Some(Ok(Token::RParen)));
        assert_eq!(lexer.next(), Some(Ok(Token::LBracket)));
        assert_eq!(lexer.next(), Some(Ok(Token::RBracket)));
        assert_eq!(lexer.next(), Some(Ok(Token::Comma)));
        assert_eq!(lexer.next(), Some(Ok(Token::Dot)));
        assert_eq!(lexer.next(), Some(Ok(Token::Plus)));
        assert_eq!(lexer.next(), Some(Ok(Token::Minus)));
        assert_eq!(lexer.next(), Some(Ok(Token::Mul)));
        assert_eq!(lexer.next(), Some(Ok(Token::Div)));
        assert_eq!(lexer.next(), Some(Ok(Token::Eq)));
        assert_eq!(lexer.next(), Some(Ok(Token::Ne)));
        assert_eq!(lexer.next(), Some(Ok(Token::Lt)));
        assert_eq!(lexer.next(), Some(Ok(Token::Le)));
        assert_eq!(lexer.next(), Some(Ok(Token::Gt)));
        assert_eq!(lexer.next(), Some(Ok(Token::Ge)));
        assert_eq!(lexer.next(), Some(Ok(Token::Not)));
        assert_eq!(lexer.next(), Some(Ok(Token::Pipe)));
        assert_eq!(lexer.next(), Some(Ok(Token::Arrow)));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_comments() {
        let input = "fun x = 10 // this is a comment\nlet y = 20";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Fun)));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("x"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Equals)));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(10.0))));
        assert_eq!(lexer.next(), Some(Ok(Token::LineComment("// this is a comment"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Newline)));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("let"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("y"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Equals)));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(20.0))));
        assert_eq!(lexer.next(), None);
    }
}
