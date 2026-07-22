use super::AstParser;
use crate::ast::*;
use rustc_hash::FxHashMap;
use crate::compiler::Loc;
use crate::error::JitError;
use crate::lexer::Token;
use crate::unescape::unescape_string;

// ---------------------------------------------------------------------------
// Fallthrough (or / except) — lowest precedence
// ---------------------------------------------------------------------------

pub(super) fn parse_fallthrough_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut lhs = parse_range_expr(parser)?;

    // Pipe operator |> — allows newlines before each pipe
    while {
        parser.stream.skip_newlines();
        parser.peek() == Some(Token::PipeForward)
    } {
        let loc = parser.loc();
        parser.advance()?; // consume |>
        let rhs = parse_pipe_rhs(parser)?;
        match rhs {
            AstNode::FunCall {
                name,
                mut args,
                named: _,
                loc,
            } => {
                args.insert(0, lhs);
                lhs = AstNode::FunCall { name, args, named: rustc_hash::FxHashMap::default(), loc };
            }
            _ => {
                return Err(JitError::parsing(
                    "Expected a function call after `|>`",
                    loc.as_error_pos(),
                ));
            }
        }
    }

    let loc = parser.loc();

    // `or` — inline fallback for failures
    if parser.peek() == Some(Token::Or) {
        parser.advance()?;
        let rhs = parse_fallthrough_expr(parser)?;
        return Ok(AstNode::Fallback {
            expr: Box::new(lhs),
            default: Box::new(rhs),
            loc,
        });
    }

    // `except` — pattern matching on failure types
    if parser.peek() == Some(Token::Except) {
        parser.advance()?;
        parser.stream.skip_newlines();
        parser.expect(Token::LBrace)?;
        let mut arms = Vec::new();
        loop {
            parser.stream.skip_newlines();
            if matches!(parser.peek(), None | Some(Token::RBrace)) {
                break;
            }
            parser.expect(Token::Pipe)?;
            parser.stream.skip_newlines();
            let type_name = if parser.peek() == Some(Token::Identifier("_")) {
                parser.advance()?;
                String::new()
            } else {
                parser.expect_ident()?.to_string()
            };
            parser.stream.skip_newlines();
            parser.expect(Token::Arrow)?;
            parser.stream.skip_newlines();
            let body = if parser.peek() == Some(Token::LBrace) {
                parser.parse_block_stmts()?
            } else {
                vec![parser.parse_expression()?]
            };
            arms.push(ExceptArm { type_name, body });
        }
        parser.expect(Token::RBrace)?;
        return Ok(AstNode::Except {
            expr: Box::new(lhs),
            arms,
            loc,
        });
    }

    Ok(lhs)
}

// ---------------------------------------------------------------------------
// Range `..` (lowest‑precedence binary)
// ---------------------------------------------------------------------------

pub(super) fn parse_range_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let lhs = parse_or_expr(parser)?;
    if parser.peek() == Some(Token::Range) {
        let loc = parser.loc();
        parser.advance()?; // '..'
        let rhs = parse_or_expr(parser)?;
        // Check for .step(N) after the range
        if parser.peek() == Some(Token::Dot) {
            // Lookahead: peek at the next token to see if it's "step"
            let saved = parser.stream.pos;
            parser.advance()?; // '.'
            if parser.peek() == Some(Token::Identifier("step")) {
                parser.advance()?; // 'step'
                if parser.peek() == Some(Token::LParen) {
                    parser.advance()?; // '('
                    let step = parser.parse_expression()?;
                    parser.expect(Token::RParen)?;
                    return Ok(AstNode::Range {
                        start: Box::new(lhs),
                        end: Box::new(rhs),
                        step: Some(Box::new(step)),
                        loc,
                    });
                }
            }
            // Not .step(N) — backtrack: restore position
            parser.stream.pos = saved;
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

// ---------------------------------------------------------------------------
// or (short-circuit)
// ---------------------------------------------------------------------------

pub(super) fn parse_or_expr<'source>(parser: &mut AstParser<'source>) -> Result<AstNode, JitError> {
    let mut lhs = parse_and_expr(parser)?;
    let loc = parser.loc();
    while parser.peek() == Some(Token::Or) {
        parser.advance()?;
        let rhs = parse_and_expr(parser)?;
        lhs = AstNode::Binary {
            op: BinOp::Or,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            loc,
        };
    }
    Ok(lhs)
}

// ---------------------------------------------------------------------------
// and (short-circuit)
// ---------------------------------------------------------------------------

pub(super) fn parse_and_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut lhs = parse_comp_expr(parser)?;
    let loc = parser.loc();
    while parser.peek() == Some(Token::And) {
        parser.advance()?;
        let rhs = parse_comp_expr(parser)?;
        lhs = AstNode::Binary {
            op: BinOp::And,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            loc,
        };
    }
    Ok(lhs)
}

// ---------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------

pub(super) fn parse_comp_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut lhs = parse_add_expr(parser)?;
    loop {
        let loc = parser.loc();
        let op = match parser.peek() {
            Some(Token::Eq) => {
                parser.advance()?;
                Some(BinOp::Eq)
            }
            Some(Token::Ne) => {
                parser.advance()?;
                Some(BinOp::Ne)
            }
            Some(Token::Lt) => {
                parser.advance()?;
                Some(BinOp::Lt)
            }
            Some(Token::Le) => {
                parser.advance()?;
                Some(BinOp::Le)
            }
            Some(Token::Gt) => {
                parser.advance()?;
                Some(BinOp::Gt)
            }
            Some(Token::Ge) => {
                parser.advance()?;
                Some(BinOp::Ge)
            }
            _ => None,
        };
        match op {
            Some(op) => {
                let rhs = parse_add_expr(parser)?;
                lhs = AstNode::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            None => break,
        }
    }
    Ok(lhs)
}

// ---------------------------------------------------------------------------
// Additive
// ---------------------------------------------------------------------------

pub(super) fn parse_add_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut lhs = parse_mul_expr(parser)?;
    loop {
        let loc = parser.loc();
        match parser.peek() {
            Some(Token::Plus) => {
                parser.advance()?;
                let rhs = parse_mul_expr(parser)?;
                lhs = AstNode::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            Some(Token::Minus) => {
                parser.advance()?;
                let rhs = parse_mul_expr(parser)?;
                lhs = AstNode::Binary {
                    op: BinOp::Sub,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            _ => break,
        }
    }
    Ok(lhs)
}

// ---------------------------------------------------------------------------
// Multiplicative
// ---------------------------------------------------------------------------

pub(super) fn parse_mul_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut lhs = parse_unary_expr(parser)?;
    loop {
        let loc = parser.loc();
        match parser.peek() {
            Some(Token::Mul) => {
                parser.advance()?;
                let rhs = parse_unary_expr(parser)?;
                lhs = AstNode::Binary {
                    op: BinOp::Mul,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            Some(Token::Div) => {
                parser.advance()?;
                let rhs = parse_unary_expr(parser)?;
                lhs = AstNode::Binary {
                    op: BinOp::Div,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            Some(Token::Mod) => {
                parser.advance()?;
                let rhs = parse_unary_expr(parser)?;
                lhs = AstNode::Binary {
                    op: BinOp::Mod,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    loc,
                };
            }
            _ => break,
        }
    }
    Ok(lhs)
}

// ---------------------------------------------------------------------------
// Unary
// ---------------------------------------------------------------------------

pub(super) fn parse_unary_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    match parser.peek() {
        Some(Token::Not) => {
            parser.advance()?;
            let expr = parse_unary_expr(parser)?;
            Ok(AstNode::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
                loc,
            })
        }
        Some(Token::Minus) => {
            parser.advance()?;
            let expr = parse_unary_expr(parser)?;
            Ok(AstNode::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
                loc,
            })
        }
        Some(Token::Fail) => {
            parser.advance()?;
            let mut path = vec![parser.expect_ident()?.to_string()];
            while parser.peek() == Some(Token::Dot) {
                parser.advance()?;
                path.push(parser.expect_ident()?.to_string());
            }
            let type_name = path.join(".");
            Ok(AstNode::Fail { type_name, loc })
        }
        _ => parse_postfix_expr(parser),
    }
}

// ---------------------------------------------------------------------------
// Postfix (calls, indexing, field access, ranges)
// ---------------------------------------------------------------------------

pub(super) fn parse_postfix_expr<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let mut left = parse_primary(parser)?;
    loop {
        let loc = parser.loc();
        match parser.peek() {
            // obj(args) → dynamic call
            Some(Token::LParen) => {
                parser.advance()?;
                let (args, named) = parse_call_args(parser)?;
                if let AstNode::Ident(name, _) = &left {
                    left = AstNode::FunCall { name: name.clone(), args, named, loc };
                } else {
                    left = AstNode::DynamicCall { callee: Box::new(left), args, named, loc };
                }
            }
            // obj[index]
            Some(Token::LBracket) => {
                parser.advance()?;
                let index = parser.parse_expression()?;
                parser.expect(Token::RBracket)?;
                left = AstNode::Index {
                    obj: Box::new(left),
                    index: Box::new(index),
                    loc,
                };
            }
            // obj.field (method calls via dot are removed — use pipe)
            Some(Token::Dot) => {
                parser.advance()?;
                let field = parser.expect_ident()?.to_string();
                if parser.peek() == Some(Token::LParen) {
                    return Err(JitError::parsing(
                        "Method calls with dot notation are not supported.\n\
                         Use the pipe operator `|>` instead:\n  obj |> method()",
                        loc.as_error_pos(),
                    ));
                } else {
                    left = AstNode::Field {
                        obj: Box::new(left),
                        name: field,
                        loc,
                    };
                }
            }
            _ => break,
        }
    }
    Ok(left)
}

// ---------------------------------------------------------------------------
// Primary expressions
// ---------------------------------------------------------------------------

pub(super) fn parse_primary<'source>(parser: &mut AstParser<'source>) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    match parser.advance()? {
        Token::LParen => {
            let inner = parser.parse_expression()?;
            parser.expect(Token::RParen)?;
            Ok(inner)
        }
        Token::Number(n) => Ok(AstNode::Number(n, loc)),
        Token::Bool(b) => Ok(AstNode::Bool(b, loc)),
        Token::Nil => Ok(AstNode::Nil(loc)),
        Token::String(s) => Ok(AstNode::Str(unescape_string(s), loc)),
        Token::Template(s) => parse_template(parser, s, loc),
        Token::Identifier(id) => Ok(AstNode::Ident(id.to_string(), loc)),

        // List literal
        Token::LBracket => parse_list_lit(parser, loc),
        // Object literal
        Token::LBrace => parse_object_lit(parser, loc),
        // Closure
        Token::Await => {
            let expr = parser.parse_expression()?;
            Ok(AstNode::Await(Box::new(expr), loc))
        }
        Token::Pipe => parse_closure(parser, false, loc),
        Token::Move => {
            parser.expect(Token::Pipe)?;
            parse_closure(parser, true, loc)
        }
        t => Err(JitError::parsing(
            format!("Expected expression, found {:?}", t),
            loc.as_error_pos(),
        )),
    }
}

// ---------------------------------------------------------------------------
// List literal
// ---------------------------------------------------------------------------

pub(super) fn parse_list_lit<'source>(
    parser: &mut AstParser<'source>,
    loc: Loc,
) -> Result<AstNode, JitError> {
    parser.stream.skip_newlines();
    if parser.peek() == Some(Token::RBracket) {
        parser.advance()?;
        return Ok(AstNode::ListLit(vec![], loc));
    }
    let first = parser.parse_expression()?;
    parser.stream.skip_newlines();
    // Check for [val; count]
    if parser.peek() == Some(Token::Semicolon) {
        parser.advance()?;
        parser.stream.skip_newlines();
        let count = parser.parse_expression()?;
        parser.stream.skip_newlines();
        parser.expect(Token::RBracket)?;
        return Ok(AstNode::ListRepeat {
            val: Box::new(first),
            count: Box::new(count),
            loc,
        });
    }
    let mut elems = vec![first];
    if parser.peek() == Some(Token::Comma) {
        parser.advance()?;
        parser.stream.skip_newlines();
        if parser.peek() != Some(Token::RBracket) {
            loop {
                elems.push(parser.parse_expression()?);
                parser.stream.skip_newlines();
                if parser.peek() == Some(Token::Comma) {
                    parser.advance()?;
                    if parser.peek() == Some(Token::RBracket) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }
    parser.expect(Token::RBracket)?;
    Ok(AstNode::ListLit(elems, loc))
}

// ---------------------------------------------------------------------------
// Object literal
// ---------------------------------------------------------------------------

pub(super) fn parse_object_lit<'source>(
    parser: &mut AstParser<'source>,
    loc: Loc,
) -> Result<AstNode, JitError> {
    let mut fields = Vec::new();
    parser.stream.skip_newlines();
    if parser.peek() != Some(Token::RBrace) {
        loop {
            parser.stream.skip_newlines();
            let name = parser.expect_ident()?.to_string();
            parser.stream.skip_newlines();
            parser.expect(Token::Colon)?;
            let val = parser.parse_expression()?;
            fields.push((name, val));
            parser.stream.skip_newlines();
            if parser.peek() == Some(Token::Comma) {
                parser.advance()?;
                if parser.peek() == Some(Token::RBrace) {
                    break;
                }
            } else {
                break;
            }
        }
    }
    parser.expect(Token::RBrace)?;
    Ok(AstNode::ObjectLit(fields, loc))
}

// ---------------------------------------------------------------------------
// Closure
// ---------------------------------------------------------------------------

pub(super) fn parse_closure<'source>(
    parser: &mut AstParser<'source>,
    is_move: bool,
    loc: Loc,
) -> Result<AstNode, JitError> {
    let params = super::parse_params_until(parser, Token::Pipe)?;
    parser.expect(Token::Pipe)?; // consume closing '|'
    let body = if parser.peek() == Some(Token::LBrace) {
        parser.advance()?; // consume '{'
        super::stmt::parse_block(parser)?
    } else {
        parser.parse_expression()?
    };
    Ok(AstNode::Closure {
        params,
        body: Box::new(body),
        is_move,
        loc,
    })
}

// ---------------------------------------------------------------------------
// Template literals
// ---------------------------------------------------------------------------

pub(super) fn parse_template<'source>(
    _parser: &AstParser<'source>,
    s: &'source str,
    loc: Loc,
) -> Result<AstNode, JitError> {
    // Templates are handled the same way as the existing parser.
    // For now, treat them as plain string literals (simplified).
    Ok(AstNode::Str(s.to_string(), loc))
}

// ---------------------------------------------------------------------------
// Call args
// ---------------------------------------------------------------------------

/// Parse call arguments, returning `(positional_args, named_args)`.
/// Named args use `name: expr` syntax: `f(1, verbose: true)`.
pub(super) fn parse_call_args<'source>(
    parser: &mut AstParser<'source>,
) -> Result<(Vec<AstNode>, FxHashMap<String, AstNode>), JitError> {
    let mut args = Vec::new();
    let mut named = FxHashMap::default();
    parser.stream.skip_newlines();
    if parser.peek() != Some(Token::RParen) {
        loop {
            parser.stream.skip_newlines();
            // Parse an expression; if it's a bare `Ident` followed by `:`,
            // treat it as a named argument.
            let expr = parser.parse_expression()?;
            if matches!(&expr, AstNode::Ident(..)) && parser.peek() == Some(Token::Colon) {
                let name = match &expr { AstNode::Ident(n, _) => n.clone(), _ => unreachable!() };
                parser.advance()?; // consume `:`
                let value = parser.parse_expression()?;
                named.insert(name, value);
            } else {
                args.push(expr);
            }
            parser.stream.skip_newlines();
            if parser.peek() == Some(Token::Comma) {
                parser.advance()?;
            } else {
                break;
            }
        }
    }
    parser.expect(Token::RParen)?;
    Ok((args, named))
}

// ---------------------------------------------------------------------------
// Pipe RHS
// ---------------------------------------------------------------------------

/// Parse the right-hand side of a `|>` pipe: `ident(args)`.
/// Returns a `FunCall` **without** the piped value prepended.
pub(super) fn parse_pipe_rhs<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    let name = parser.expect_ident()?.to_string();
    parser.expect(Token::LParen)?;
    let (args, named) = parse_call_args(parser)?;
    Ok(AstNode::FunCall { name, args, named, loc })
}

// ---------------------------------------------------------------------------
// String literal helper
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(super) fn expect_str<'source>(parser: &mut AstParser<'source>) -> Result<String, JitError> {
    match parser.advance()? {
        Token::String(s) => Ok(unescape_string(s)),
        t => Err(JitError::parsing(
            format!("Expected string, found {:?}", t),
            parser.loc().as_error_pos(),
        )),
    }
}
