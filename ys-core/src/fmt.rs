use crate::lexer::Token;

pub fn format_source(source: &str) -> String {
    let lexer = <Token as logos::Logos>::lexer(source);
    let mut output = String::with_capacity(source.len());
    let mut indent: usize = 0;
    let mut line_start = true;
    let mut pending_newlines = 0;

    for result in lexer {
        let token = match result {
            Ok(t) => t,
            Err(_) => continue,
        };
        match token {
            Token::LBrace => {
                flush_newlines(&mut output, &mut line_start, &mut pending_newlines, indent);
                if !line_start { output.push(' '); }
                output.push('{'); output.push('\n');
                indent += 1; line_start = true;
            }
            Token::RBrace => {
                indent = indent.saturating_sub(1);
                flush_newlines(&mut output, &mut line_start, &mut pending_newlines, indent);
                if !line_start { output.push('\n'); }
                output.push_str(&"  ".repeat(indent));
                output.push('}'); output.push('\n');
                line_start = true;
            }
            Token::Newline => {
                pending_newlines += 1;
            }
            Token::LineComment(text) => {
                flush_newlines(&mut output, &mut line_start, &mut pending_newlines, indent);
                output.push_str(text);
                output.push('\n');
                line_start = true;
            }
            _ => {
                flush_newlines(&mut output, &mut line_start, &mut pending_newlines, indent);
                if line_start {
                    output.push_str(&"  ".repeat(indent));
                    line_start = false;
                } else {
                    let prev = output.as_bytes().last().copied().unwrap_or(0);
                    let needs_space = prev != b' ' && prev != b'(' && prev != b'['
                        && !matches!(token, Token::LParen | Token::RParen
                            | Token::LBracket | Token::RBracket
                            | Token::Comma | Token::Range | Token::Pipe);
                    if needs_space { output.push(' '); }
                }
                output.push_str(&token_display(&token));
            }
        }
    }
    output
}

fn flush_newlines(output: &mut String, line_start: &mut bool, pending: &mut usize, _indent: usize) {
    if *pending > 0 {
        let extra = if *line_start { *pending - 1 } else { *pending };
        if extra > 0 {
            output.push('\n');
            if extra > 1 { output.push('\n'); }
        }
        *line_start = true;
        *pending = 0;
    }
}

fn token_display(t: &Token<'_>) -> String {
    match t {
        Token::Fun => "fun".into(),   Token::Ret => "ret".into(),
        Token::If => "if".into(),    Token::Else => "else".into(),
        Token::For => "for".into(),   Token::While => "while".into(),
        Token::In => "in".into(),    Token::And => "and".into(),
        Token::Or => "or".into(),    Token::Nil => "nil".into(),
        Token::Exp => "exp".into(),   Token::Use => "use".into(),
        Token::Super => "super".into(), Token::Move => "move".into(),
        Token::Async => "async".into(), Token::Await => "await".into(),
        Token::Switch => "switch".into(),Token::Break => "break".into(),
        Token::Except => "except".into(),Token::Fail => "fail".into(),
        Token::Error => "error".into(), Token::Continue => "continue".into(),
        Token::Yield => "yield".into(),
        Token::Bool(true) => "true".into(),  Token::Bool(false) => "false".into(),
        Token::Plus => "+".into(), Token::Minus => "-".into(), Token::Mul => "*".into(),
        Token::Div => "/".into(), Token::Mod => "%".into(),
        Token::Eq => "==".into(), Token::Ne => "!=".into(),
        Token::Lt => "<".into(),  Token::Le => "<=".into(),
        Token::Gt => ">".into(),  Token::Ge => ">=".into(),
        Token::Equals => "=".into(),  Token::Not => "!".into(),
        Token::Dot => ".".into(),  Token::Range => "..".into(), Token::Arrow => "->".into(),
        Token::Pipe => "|".into(),  Token::Colon => ":".into(), Token::Comma => ",".into(),
        Token::Semicolon => ";".into(),
        Token::PlusEq => "+=".into(), Token::MinusEq => "-=".into(), Token::MulEq => "*=".into(),
        Token::DivEq => "/=".into(), Token::ModEq => "%=".into(),
        Token::LParen => "(".into(), Token::RParen => ")".into(),
        Token::LBrace => "{".into(), Token::RBrace => "}".into(),
        Token::LBracket => "[".into(), Token::RBracket => "]".into(),
        Token::Number(n) => n.to_string(),
        Token::String(s) => format!("\"{}\"", s),
        Token::Template(s) => format!("`{}`", s),
        Token::Identifier(s) => s.to_string(),
        Token::LineComment(s) => s.to_string(),
        Token::Newline => "\n".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_simple() {
        let input = "fun add(a,b){ret a+b}";
        let out = format_source(input);
        assert!(out.contains("fun add(a, b) {"));
        assert!(out.contains("  ret a + b"));
    }
}
