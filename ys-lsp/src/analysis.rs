//! Optimized source analysis for YatsuScript LSP.
//!
//! Performs a single-pass scan of the token stream to build an index of
//! high-level declarations (functions, variables) and collect diagnostics.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range, Position, SymbolKind};
use ys_core::lexer::Token;
use logos::Logos as _;

/// A found declaration in the source code.
#[derive(Debug, Clone)]
pub struct Declaration {
    pub name:  String,
    pub kind:  SymbolKind,
    pub range: Range,
}

/// Results of a document analysis.
#[derive(Debug, Default)]
pub struct AnalysisResults {
    pub declarations: Vec<Declaration>,
    pub diagnostics:  Vec<Diagnostic>,
    pub tokens:       Vec<LspToken>,
}

/// A token carry-along for the LSP, representing its variant and position.
#[derive(Debug, Clone, Copy)]
pub struct LspToken {
    pub variant: LspTokenVariant,
    pub line:    u32,
    pub char:    u32,
    pub len:     u32,
}

/// Enum discriminant for semantic classification of tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspTokenVariant {
    Keyword, Function, Variable, Operator, Comment, String, Number, Other,
}

/// Analyze a source string, returning declarations and diagnostics.
pub fn analyze_source(source: &str) -> AnalysisResults {
    let mut results = AnalysisResults::default();
    let mut lexer   = Token::lexer(source);
    
    // Line-start map for calculating positions efficiently.
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect();

    let get_pos = |offset: usize| {
        let line = line_starts.iter().rposition(|&s| s <= offset).unwrap_or(0);
        Position::new(line as u32, (offset - line_starts[line]) as u32)
    };

    while let Some(token_res) = lexer.next() {
        let span  = lexer.span();
        let pos   = get_pos(span.start);
        let len   = (span.end - span.start) as u32;

        match token_res {
            Ok(token) => {
                // Classify token for semantic highlighting.
                let variant = match &token {
                    Token::Fun | Token::Return | Token::Continue | Token::If | Token::Else
                    | Token::For | Token::While | Token::In | Token::Use | Token::Super
                    | Token::Exp | Token::Move | Token::And | Token::Or
                        => LspTokenVariant::Keyword,
                    
                    Token::Identifier(_) => LspTokenVariant::Variable,
                    
                    Token::Plus | Token::Minus | Token::Mul | Token::Div | Token::Mod
                    | Token::Eq | Token::Ne | Token::Lt | Token::Le 
                    | Token::Gt | Token::Ge | Token::Not | Token::Dot | Token::Range 
                        => LspTokenVariant::Operator,
                    
                    Token::String(_) | Token::Template(_) => LspTokenVariant::String,
                    Token::Number(_) => LspTokenVariant::Number,
                    
                    Token::LineComment => LspTokenVariant::Comment,
                    _ => LspTokenVariant::Other,
                };
                
                results.tokens.push(LspToken { variant, line: pos.line, char: pos.character, len });

                // Identify declarations.
                match token {
                    Token::Fun => {
                        if let Some(Ok(Token::Identifier(name))) = lexer.next() {
                            let s = lexer.span();
                            let p = get_pos(s.start);
                            results.declarations.push(Declaration {
                                name: name.to_string(),
                                kind: SymbolKind::FUNCTION,
                                range: Range::new(p, get_pos(s.end)),
                            });
                            results.tokens.push(LspToken { 
                                variant: LspTokenVariant::Function, 
                                line: p.line, 
                                char: p.character, 
                                len: (s.end - s.start) as u32 
                            });
                        }
                    }
                    _ => {}
                }
            }
            Err(_) => {
                results.diagnostics.push(Diagnostic {
                    range: Range::new(pos, get_pos(span.end)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "Unexpected character".into(),
                    source: Some("yatsuscript".into()),
                    ..Default::default()
                });
            }
        }
    }

    results
}
