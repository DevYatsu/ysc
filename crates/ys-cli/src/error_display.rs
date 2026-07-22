use colored::Colorize;
use logos::Logos;
use ys_core::error::ErrorLoc;
use ys_core::error::JitError;
use ys_core::lexer::Token;

const CONTEXT_LINES: usize = 2;
const MAX_LINE_LEN: usize = 120;

struct DisplayError {
    code: &'static str,
    title: &'static str,
    explanation: String,
    severity: Severity,
    line: usize,
    col: usize,
    #[allow(dead_code)]
    span_len: usize,
    hints: Vec<Hint>,
}

#[derive(PartialEq)]
#[allow(dead_code)]
enum Severity {
    Error,
    Warning,
    Note,
}

struct Hint {
    message: String,
    kind: HintKind,
}

enum HintKind {
    Help,
    Note,
    Suggestion,
    Location,
}

fn token_span_at(source: &str, line: usize, col: usize) -> usize {
    let lines: Vec<&str> = source.lines().collect();
    if line == 0 || line > lines.len() {
        return 1;
    }
    let line_str = lines[line - 1];
    let byte_col = col.saturating_sub(1);
    if byte_col >= line_str.len() {
        return 1;
    }
    let lexer = Token::lexer(line_str);
    for (tok, span) in lexer.spanned() {
        if let Ok(_t) = tok {
            let (start, end) = (span.start, span.end);
            if byte_col >= start && byte_col < end {
                return (end - start).max(1);
            }
        }
    }
    1
}

/// Smart hint generator — tries to guess what the user meant.
fn generate_hints(err: &JitError, source: &str) -> Vec<Hint> {
    match err {
        JitError::Parsing { msg, loc } => parse_error_hints(msg, loc, source),
        JitError::Runtime { msg, loc } => runtime_error_hints(msg, loc, source),
        JitError::Lexing { .. } => vec![],
        JitError::UnknownVariable { msg, .. } => unknown_var_hints(msg, source),
        JitError::RedefinitionOfImmutableVariable {
            msg: _,
            loc: _,
            orig_line,
        } => vec![
            Hint {
                message: "Variables can be reassigned in ysc".into(),
                kind: HintKind::Note,
            },
            Hint {
                message: format!("Originally defined on line {}", orig_line),
                kind: HintKind::Location,
            },
        ],
    }
}

fn parse_error_hints(msg: &str, loc: &ErrorLoc, source: &str) -> Vec<Hint> {
    let line_str = source.lines().nth(loc.line.saturating_sub(1)).unwrap_or("");

    // ── Missing closing brace `}` ───────────────────────
    if msg.contains("RBrace") && (msg.contains("Expected") || msg.contains("expected"))
        || msg.contains("Expected expression") && line_str.trim() == "else"
    {
        let mut hints = vec![Hint {
            message: "you might be missing a closing '}' — every '{' must have a matching '}'"
                .into(),
            kind: HintKind::Help,
        }];
        // Check if an opening brace exists without a matching close
        let open_count = source.matches('{').count();
        let close_count = source.matches('}').count();
        if open_count > close_count {
            hints.push(Hint {
                message: format!(
                    "this block is unclosed ({} `{{` vs {} `}}`)",
                    open_count, close_count
                ),
                kind: HintKind::Note,
            });
        }
        return hints;
    }

    // ── Missing opening brace `{` ──────────────────────
    if msg.contains("Expected '")
        && (line_str.contains("fun ")
            || line_str.contains("if ")
            || line_str.contains("else ")
            || line_str.contains("for ")
            || line_str.contains("while "))
    {
        let what = if line_str.contains("fun ") {
            "function body"
        } else if line_str.contains("for ") {
            "for-loop body"
        } else if line_str.contains("while ") {
            "while-loop body"
        } else {
            "block body"
        };
        return vec![
            Hint {
                message: format!("'{}' bodies must be wrapped in '{{' '}}'", what),
                kind: HintKind::Help,
            },
            Hint {
                message: "try adding '{' after the condition/parameters".to_string(),
                kind: HintKind::Help,
            },
        ];
    }

    // ── Missing `)` ────────────────────────────────────
    if msg.contains("Expected") && (msg.contains("RParen") || msg.contains("','")) {
        // Check if there are more '(' than ')'
        let open = source.matches('(').count();
        let close = source.matches(')').count();
        if open > close {
            return vec![
                Hint {
                    message: "you might be missing a closing ')'".into(),
                    kind: HintKind::Help,
                },
                Hint {
                    message: format!("{} opening '(' vs {} closing ')'", open, close),
                    kind: HintKind::Note,
                },
            ];
        }
    }

    // ── Expected `in` after `for` ──────────────────────
    if msg.contains("Expected 'in'")
        || (msg.contains("Expected expression") && line_str.contains("for"))
    {
        return vec![
            Hint {
                message: "for-loop syntax: for var in iterable { ... }".into(),
                kind: HintKind::Suggestion,
            },
            Hint {
                message: "example: for i in 0..10 { print(i) }".into(),
                kind: HintKind::Help,
            },
        ];
    }

    // ── Missing comma between arguments ────────────────
    if msg.contains("Expected identifier")
        && (line_str.contains(',')
            || (loc.col > 1 && line_str.chars().nth(loc.col.saturating_sub(2)) == Some(' ')))
    {
        return vec![Hint {
            message: "function arguments must be separated by commas".into(),
            kind: HintKind::Help,
        }];
    }

    // ── Expected expression after operator ─────────────
    if msg.contains("Expected expression") {
        let before = &line_str[..loc.col.saturating_sub(1).min(line_str.len())];
        if before.ends_with('+')
            || before.ends_with('-')
            || before.ends_with('*')
            || before.ends_with('/')
        {
            return vec![Hint {
                message: "binary operator needs a right-hand side expression".into(),
                kind: HintKind::Help,
            }];
        }
    }

    // ── `exp` without `fun` ────────────────────────────
    if msg.contains("exp") && msg.contains("fun") {
        return vec![
            Hint {
                message: "'exp' currently only works on function declarations".into(),
                kind: HintKind::Note,
            },
            Hint {
                message: "use: exp fun name() { ... }".into(),
                kind: HintKind::Help,
            },
        ];
    }

    // ── Unexpected EOF — check for unclosed blocks ────
    if msg.contains("Unexpected EOF") {
        let mut hints = Vec::new();
        let open_b = source.matches('{').count();
        let close_b = source.matches('}').count();
        let open_p = source.matches('(').count();
        let close_p = source.matches(')').count();
        if open_b > close_b {
            hints.push(Hint {
                message: format!(
                    "reached end of file with unclosed blocks ({} `{{` vs {} `}}`)",
                    open_b, close_b
                ),
                kind: HintKind::Help,
            });
            hints.push(Hint {
                message: "add a matching '}' to close each open block".into(),
                kind: HintKind::Help,
            });
        }
        if open_p > close_p {
            hints.push(Hint {
                message: format!(
                    "reached end of file with unclosed parentheses ({} `(` vs {} `)`)",
                    open_p, close_p
                ),
                kind: HintKind::Help,
            });
            hints.push(Hint {
                message: "add a matching ')'".into(),
                kind: HintKind::Help,
            });
        }
        if hints.is_empty() {
            hints.push(Hint {
                message: "the source ended while the parser was still expecting more tokens".into(),
                kind: HintKind::Note,
            });
            hints.push(Hint {
                message: "check for missing closing braces '}', parentheses ')', or keywords"
                    .into(),
                kind: HintKind::Help,
            });
        }
        return hints;
    }

    vec![]
}

fn runtime_error_hints(msg: &str, loc: &ErrorLoc, source: &str) -> Vec<Hint> {
    let line_str = source.lines().nth(loc.line.saturating_sub(1)).unwrap_or("");
    let mut hints = Vec::new();

    // ── Division / modulo by zero ──────────────────────
    if msg.contains("DivisionByZero")
        || msg.contains("division by zero")
        || msg.contains("ModByZero")
    {
        hints.push(Hint {
            message: "division by zero is undefined".into(),
            kind: HintKind::Help,
        });
        hints.push(Hint {
            message: "check that the divisor is not zero before dividing".into(),
            kind: HintKind::Help,
        });
        if let Some(pos) = line_str.find('/') {
            let divisor = line_str[pos + 1..]
                .trim()
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !divisor.is_empty() {
                hints.push(Hint {
                    message: format!("the divisor is '{}' — is it guaranteed non-zero?", divisor),
                    kind: HintKind::Note,
                });
            }
        }
        return hints;
    }

    // ── Index out of bounds ────────────────────────────
    if msg.contains("IndexOutOfBounds")
        || msg.contains("index out")
        || msg.contains("index") && msg.contains("bounds")
    {
        hints.push(Hint {
            message: "index was outside the valid range".into(),
            kind: HintKind::Note,
        });
        hints.push(Hint {
            message: "use .len() to check the length before indexing".into(),
            kind: HintKind::Help,
        });
        return hints;
    }

    // ── Type errors ────────────────────────────────────
    if msg.contains("TypeError") || msg.contains("Add error") {
        hints.push(Hint {
            message: "the '+' operator works on numbers (addition) or strings (concatenation)"
                .into(),
            kind: HintKind::Help,
        });
        if let Some(pos) = line_str.find('+') {
            let left = line_str[..pos]
                .split_whitespace()
                .last()
                .unwrap_or("");
            let right = line_str[pos + 1..]
                .split_whitespace()
                .next()
                .unwrap_or("");
            hints.push(Hint {
                message: format!(
                    "left side: '{}', right side: '{}' — are they the same type?",
                    left, right
                ),
                kind: HintKind::Note,
            });
        }
        return hints;
    }

    // ── Arity mismatch ─────────────────────────────────
    if msg.contains("arity mismatch") {
        hints.push(Hint {
            message: "the number of arguments doesn't match the function's parameter count".into(),
            kind: HintKind::Note,
        });
        // Extract expected and got numbers from message
        let parts: Vec<&str> = msg
            .split(|c: char| !c.is_numeric())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() >= 2 {
            hints.push(Hint {
                message: format!("expected {} arguments, got {}", parts[0], parts[1]),
                kind: HintKind::Note,
            });
        }
        return hints;
    }

    // ── Unknown function ────────────────────────────────
    if msg.contains("Unknown function") || msg.contains("No method") {
        let name_start = msg.find('\'').map(|i| i + 1);
        let name_end = name_start.and_then(|s| msg[s..].find('\'').map(|e| s + e));
        if let (Some(s), Some(e)) = (name_start, name_end) {
            let name = &msg[s..e];
            // Search the source for similar function names
            let mut suggestions: Vec<String> = Vec::new();
            for token in Token::lexer(source).spanned().filter_map(|(t, _)| t.ok()) {
                if let Token::Identifier(id) = token {
                    let dist = levenshtein(name, id);
                    if dist > 0 && dist <= 3 {
                        suggestions.push(id.to_string());
                    }
                }
            }
            suggestions.sort();
            suggestions.dedup();
            suggestions.truncate(3);
            if !suggestions.is_empty() {
                hints.push(Hint {
                    message: format!("did you mean '{}'?", suggestions.join("', '")),
                    kind: HintKind::Suggestion,
                });
            }
        }
        hints.push(Hint {
            message: "check that the function name is spelled correctly and defined before use"
                .into(),
            kind: HintKind::Help,
        });
        return hints;
    }

    // ── Closure arity ──────────────────────────────────
    if msg.contains("Closure") {
        hints.push(Hint {
            message: "a closure is being called with the wrong number of arguments".into(),
            kind: HintKind::Note,
        });
        return hints;
    }

    // ── Expected a closure ─────────────────────────────
    if msg.contains("Expected a closure") {
        hints.push(Hint {
            message: "the second argument to map/filter/reduce/etc should be a closure".into(),
            kind: HintKind::Help,
        });
        hints.push(Hint {
            message: "use |param| expression syntax, e.g. |x| x * 2".into(),
            kind: HintKind::Suggestion,
        });
        return hints;
    }

    // ── len() expects string or list ───────────────────
    if msg.contains("len() expects") {
        hints.push(Hint {
            message: "len() works on strings, lists, and objects".into(),
            kind: HintKind::Note,
        });
        return hints;
    }

    // ── Cannot call (not callable) ──────────────────────
    if msg.contains("Cannot call") {
        hints.push(Hint {
            message: "only functions and closures can be called".into(),
            kind: HintKind::Note,
        });
        return hints;
    }

    hints
}

fn unknown_var_hints(msg: &str, source: &str) -> Vec<Hint> {
    let unknown = msg
        .trim()
        .trim_start_matches('\'')
        .split('\'')
        .next()
        .unwrap_or("");
    if unknown.is_empty() {
        return vec![];
    }

    let mut suggestions = Vec::new();
    for token in Token::lexer(source).spanned().filter_map(|(t, _)| t.ok()) {
        if let Token::Identifier(id) = token {
            let dist = levenshtein(unknown, id);
            if dist > 0 && dist <= 3 {
                suggestions.push(id.to_string());
            }
        }
    }
    suggestions.sort();
    suggestions.dedup();
    suggestions.truncate(3);

    let mut hints = Vec::new();
    if !suggestions.is_empty() {
        hints.push(Hint {
            message: format!("did you mean '{}'?", suggestions.join("', '")),
            kind: HintKind::Suggestion,
        });
    }
    hints.push(Hint {
        message: "variables must be defined before use".into(),
        kind: HintKind::Note,
    });
    hints
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in dp.iter_mut().enumerate().take(a.len() + 1) {
        row[0] = i;
    }
    for (j, val) in dp[0].iter_mut().enumerate().take(b.len() + 1) {
        *val = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[a.len()][b.len()]
}

#[allow(unused_assignments)]
fn highlight_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 32);
    let mut in_string = false;
    let mut in_comment = false;
    let mut i = 0;
    let chars: Vec<char> = line.chars().collect();
    while i < chars.len() {
        if in_comment {
            out.push_str(&chars[i].to_string().dimmed().to_string());
            i += 1;
            continue;
        }
        if chars[i] == '"' && !in_string {
            in_string = true;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            out.push_str(&line[start..i].yellow().to_string());
            in_string = false;
            continue;
        }
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            in_comment = true;
            out.push_str(&line[i..].dimmed().to_string());
            break;
        }
        let rest: String = chars[i..].iter().collect();
        let keywords = [
            "fun", "ret", "if", "else", "for", "while", "in", "and", "or", "not", "nil", "true",
            "false", "let", "async", "await", "exp", "use", "switch", "break", "move", "except",
            "fail", "error",
        ];
        if let Some(kw) = keywords.iter().find(|kw| {
            rest.starts_with(*kw)
                && (rest.len() == kw.len()
                    || !rest
                        .as_bytes()
                        .get(kw.len())
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_'))
        }) {
            out.push_str(&kw.cyan().to_string());
            i += kw.len();
            continue;
        }
        if chars[i].is_ascii_digit()
            || (chars[i] == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            if chars[i] == '-' {
                i += 1;
            }
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            out.push_str(&line[start..i].magenta().to_string());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn determine_span(source: &str, line: usize, col: usize, msg: &str) -> usize {
    let token_span = token_span_at(source, line, col);
    if msg.contains("Expected") && msg.contains("'") {
        let lines: Vec<&str> = source.lines().collect();
        if line > 0 && line <= lines.len() {
            let line_str = lines[line - 1];
            let byte_col = col.saturating_sub(1).min(line_str.len());
            let remaining = &line_str[byte_col..];
            let expr_end = remaining
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(remaining.len());
            return expr_end.max(1);
        }
    }
    token_span.max(1)
}

fn classify_parse_error(msg: &str) -> (&'static str, &'static str) {
    if msg.contains("Expected expression") {
        ("E002", "expected expression")
    } else if msg.contains("Expected identifier") {
        ("E002", "expected identifier")
    } else if msg.contains("Expected '") {
        ("E002", "expected token")
    } else if msg.contains("expected 'fun'") {
        ("E002", "expected function declaration")
    } else if msg.contains("Unclosed") {
        ("E002", "unclosed delimiter")
    } else if msg.contains("duplicate") {
        ("E002", "duplicate definition")
    } else if msg.contains("Unknown") {
        ("E004", "unknown name")
    } else {
        ("E002", "syntax error")
    }
}

fn build_display(err: &JitError, source: &str) -> DisplayError {
    let hints = generate_hints(err, source);
    match err {
        JitError::Lexing { err: le, loc } => {
            let (title, explanation) = match le {
                ys_core::lexer::LexingError::InvalidInteger => (
                    "invalid integer literal",
                    "Integer overflow or malformed integer literal.".into(),
                ),
                ys_core::lexer::LexingError::InvalidFloat => (
                    "invalid float literal",
                    "Floating-point overflow or malformed literal.".into(),
                ),
                ys_core::lexer::LexingError::NonAsciiCharacter(c) => (
                    "non-ASCII character",
                    format!(
                        "ysc only supports ASCII source files. Character '{}' is not valid.",
                        c
                    ),
                ),
                ys_core::lexer::LexingError::Other => (
                    "unknown lexer error",
                    "An unrecognised character or token was encountered.".into(),
                ),
            };
            DisplayError {
                code: "E001",
                title,
                explanation,
                severity: Severity::Error,
                line: loc.line,
                col: loc.col,
                span_len: 1,
                hints,
            }
        }
        JitError::Parsing { msg, loc } => {
            let (code, title) = classify_parse_error(msg);
            DisplayError {
                code,
                title,
                explanation: msg.clone(),
                severity: Severity::Error,
                line: loc.line,
                col: loc.col,
                span_len: token_span_at(source, loc.line, loc.col),
                hints,
            }
        }
        JitError::Runtime { msg, loc } => DisplayError {
            code: "E003",
            title: "runtime error",
            explanation: msg.clone(),
            severity: Severity::Error,
            line: loc.line,
            col: loc.col,
            span_len: token_span_at(source, loc.line, loc.col),
            hints,
        },
        JitError::UnknownVariable { msg, loc } => DisplayError {
            code: "E004",
            title: "unknown variable",
            explanation: msg.clone(),
            severity: Severity::Error,
            line: loc.line,
            col: loc.col,
            span_len: token_span_at(source, loc.line, loc.col),
            hints,
        },
        JitError::RedefinitionOfImmutableVariable {
            msg,
            loc,
            orig_line: _,
        } => DisplayError {
            code: "E005",
            title: "redefinition of immutable variable",
            explanation: format!("cannot reassign `{}` (redeclared)", msg),
            severity: Severity::Error,
            line: loc.line,
            col: loc.col,
            span_len: token_span_at(source, loc.line, loc.col),
            hints,
        },
    }
}

pub fn display_error(err: &JitError, source: &str) {
    let info = build_display(err, source);
    let line = info.line;
    let col = info.col;
    let span_len = determine_span(source, line, col, &info.explanation);

    let lines: Vec<&str> = source.lines().collect();
    let line_num_width = (lines.len().max(1)).to_string().len();

    // Header
    let severity_str = match info.severity {
        Severity::Error => "error".red().bold(),
        Severity::Warning => "warning".yellow().bold(),
        Severity::Note => "note".cyan().bold(),
    };
    let code_str = format!("[{}]", info.code).dimmed();
    println!("\n  {}{} {}", severity_str, code_str, info.title);
    println!("  {}", info.explanation.dimmed());

    // Source context
    let start_line = if line > CONTEXT_LINES {
        line - CONTEXT_LINES
    } else {
        1
    };
    let end_line = (line + CONTEXT_LINES).min(lines.len());

    for current_line in start_line..=end_line {
        let line_str = lines[current_line - 1];
        let display_line = if line_str.len() > MAX_LINE_LEN {
            &line_str[..MAX_LINE_LEN]
        } else {
            line_str
        };
        let gutter = format!("{:>width$}", current_line, width = line_num_width);

        if current_line == line {
            println!(
                "  {} {} {}{}",
                gutter.red().bold(),
                "|".red().bold(),
                ">".red().bold(),
                highlight_line(display_line).red().bold(),
            );
            let col_byte = col.saturating_sub(1).min(display_line.len());
            let padding = " ".repeat(line_num_width + 3 + col_byte);
            let ulen = span_len
                .min(display_line.len().saturating_sub(col_byte))
                .max(1);
            println!(
                "{}{} {}",
                padding,
                "^".repeat(ulen).red().bold(),
                info.title.dimmed()
            );
        } else {
            println!(
                "  {} {} {}",
                gutter.dimmed(),
                "|".dimmed(),
                highlight_line(display_line).dimmed(),
            );
        }
    }

    // Hints
    for hint in &info.hints {
        let prefix = match hint.kind {
            HintKind::Help => "help:".green().bold(),
            HintKind::Note => "note:".cyan().bold(),
            HintKind::Suggestion => "suggestion:".yellow().bold(),
            HintKind::Location => "-->".cyan().bold(),
        };
        println!("  {} {}", prefix, hint.message);
    }
}
