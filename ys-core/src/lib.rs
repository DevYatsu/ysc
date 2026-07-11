//! # ys-core
//!
//! The YatsuScript language frontend.
//!
//! This crate contains everything needed to go from source text to compiled
//! bytecode.  It has **no async runtime**, **no I/O**, and **no network** deps.
//!
//! Dependents:
//! - `ys-runtime` — takes the compiled [`Program`] and executes it
//! - `yatsuscript-lsp` — uses the lexer and parser for analysis

pub mod ast;
pub mod ast_parser;
pub mod codegen;
pub mod compiler;
pub mod error;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod token_stream;
pub mod template;
pub mod unescape;

// Re-export the most-used types at the crate root for convenience.
pub use compiler::Program;
pub use error::JitError;
pub use lexer::Token;
pub use parser::Parser;
pub use unescape::unescape_string;
