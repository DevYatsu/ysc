//! # ys-cli fmt
//!
//! A simple YatsuScript code formatter.

use std::path::{Path, PathBuf};
use std::fs;
use logos::Logos;
use colored::Colorize;

use ys_core::lexer::Token;

/// Format all YatsuScript files in a directory or a single file.
pub fn format_all(input: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut files = Vec::<PathBuf>::new();

    if input.is_file() {
        if input.extension().is_some_and(|ext| ext == "ys") {
            files.push(input.to_path_buf());
        }
    } else {
        for entry in fs::read_dir(input)? {
            let entry = entry?;
            let path  = entry.path();
            if path.extension().is_some_and(|ext| ext == "ys") {
                files.push(path);
            }
        }
    }

    for path in files {
        let source    = fs::read_to_string(&path)?;
        let formatted = format_source(&source);
        fs::write(&path, formatted)?;
        println!("{} {}", "Formatted".green(), path.display());
    }

    Ok(())
}

fn format_source(source: &str) -> String {
    let lexer = Token::lexer(source);
    let tokens: Vec<_> = lexer.flatten().collect();
    format_tokens(&tokens)
}

fn format_tokens(tokens: &[Token]) -> String {
    let mut output = String::with_capacity(tokens.len() * 4);
    let mut indent: usize = 0;
    let mut line_start = true;

    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::LBrace => {
                if !line_start { output.push(' '); }
                output.push('{');
                output.push('\n');
                indent += 1;
                line_start = true;
            }
            Token::RBrace => {
                indent = indent.saturating_sub(1);
                if !line_start { output.push('\n'); }
                output.push_str(&"  ".repeat(indent));
                output.push('}');
                output.push('\n');
                line_start = true;
            }
            Token::Newline => {
                if !line_start {
                    output.push('\n');
                    line_start = true;
                }
            }
            _ => {
                if line_start {
                    output.push_str(&"  ".repeat(indent));
                    line_start = false;
                } else if i > 0 && !matches!(tokens[i-1], Token::Dot) && token != &Token::RParen && token != &Token::RBracket && token != &Token::Comma {
                    output.push(' ');
                }
                output.push_str(&format_token(token));
            }
        }
    }
    output
}

fn format_token(token: &Token) -> String {
    match token {
        Token::Identifier(id) => id.to_string(),
        Token::String(s)     => format!("\"{}\"", s),
        Token::Number(n)     => n.to_string(),
        Token::Bool(b)       => b.to_string(),
        Token::Plus          => "+".to_string(),
        Token::Minus         => "-".to_string(),
        Token::Mul           => "*".to_string(),
        Token::Div           => "/".to_string(),
        Token::Mod           => "%".to_string(),
        Token::Eq            => "==".to_string(),
        Token::Ne            => "!=".to_string(),
        Token::Lt           => "<".to_string(),
        Token::Le           => "<=".to_string(),
        Token::Gt           => ">".to_string(),
        Token::Ge           => ">=".to_string(),
        Token::Not           => "!".to_string(),
        Token::Colon         => ":".to_string(),
        Token::Comma         => ",".to_string(),
        Token::Semicolon     => ";".to_string(),
        Token::Dot           => ".".to_string(),
        Token::LParen        => "(".to_string(),
        Token::RParen        => ")".to_string(),
        Token::LBracket      => "[".to_string(),
        Token::RBracket      => "]".to_string(),
        Token::Range         => "..".to_string(),
        Token::If            => "if".to_string(),
        Token::Else          => "else".to_string(),
        Token::For           => "for".to_string(),
        Token::While         => "while".to_string(),
        Token::In            => "in".to_string(),
        Token::Return        => "return".to_string(),
        Token::Fun           => "fun".to_string(),
        Token::Continue      => "continue".to_string(),
        Token::Equals        => "=".to_string(),
        Token::Pipe          => "|".to_string(),
        Token::Arrow         => "->".to_string(),
        Token::Use           => "use".to_string(),
        Token::Super         => "super".to_string(),
        Token::Exp           => "exp".to_string(),
        Token::Move          => "move".to_string(),
        Token::And           => "and".to_string(),
        Token::Or            => "or".to_string(),
        _                    => "".to_string(),
    }
}
