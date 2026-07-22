use super::AstParser;
use crate::ast::*;
use crate::error::JitError;
use crate::lexer::Token;

// ---------------------------------------------------------------------------
// Block
// ---------------------------------------------------------------------------

pub(super) fn parse_block<'source>(parser: &mut AstParser<'source>) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    // The opening `{` must already be consumed by the caller.
    let mut stmts = Vec::new();
    loop {
        parser.stream.skip_newlines();
        if matches!(parser.peek(), None | Some(Token::RBrace)) {
            break;
        }
        if let Some(s) = parser.parse_statement()? {
            stmts.push(s);
        }
    }
    parser.expect(Token::RBrace)?;
    Ok(AstNode::Block(stmts, loc))
}

// ---------------------------------------------------------------------------
// Assignment helpers
// ---------------------------------------------------------------------------

/// Determine whether the current identifier starts an assignment.
/// Peek past type-annotations, dots, and brackets to see if `=` follows.
pub(super) fn is_assignment_start<'source>(parser: &AstParser<'source>) -> bool {
    let mut p = parser.stream.pos + 1;
    let tokens = &parser.stream.tokens;
    loop {
        let Some(td) = tokens.get(p) else {
            return false;
        };
        match td.token {
            Token::Newline | Token::LineComment(_) => p += 1,
            Token::Colon => {
                p += 1;
                // skip the type name
                if let Some(td2) = tokens.get(p)
                    && matches!(td2.token, Token::Identifier(_))
                {
                    p += 1;
                }
            }
            Token::LBracket => {
                // skip to matching ]
                p += 1;
                let mut depth = 1;
                while let Some(td) = tokens.get(p) {
                    match td.token {
                        Token::LBracket => depth += 1,
                        Token::RBracket => {
                            depth -= 1;
                            if depth == 0 {
                                p += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    p += 1;
                }
            }
            Token::Dot => p += 1,
            Token::Equals
            | Token::PlusEq
            | Token::MinusEq
            | Token::MulEq
            | Token::DivEq
            | Token::ModEq => return true,
            _ => return false,
        }
    }
}

pub(super) fn parse_assignment<'source>(
    parser: &mut AstParser<'source>,
    id: &'source str,
) -> Result<Option<AstNode>, JitError> {
    let loc = parser.loc();
    // Optional type annotation
    if parser.peek() == Some(Token::Colon) {
        parser.advance()?; // ':'
        parser.expect_ident()?; // type name, ignored
    }
    // Build the target (handle dot/bracket accessors)
    let mut target = AstNode::Ident(id.to_string(), loc);
    loop {
        match parser.peek() {
            Some(Token::LBracket) => {
                parser.advance()?;
                let index = parser.parse_expression()?;
                parser.expect(Token::RBracket)?;
                target = AstNode::Index {
                    obj: Box::new(target),
                    index: Box::new(index),
                    loc: parser.loc(),
                };
            }
            Some(Token::Dot) => {
                parser.advance()?;
                let field = parser.expect_ident()?;
                target = AstNode::Field {
                    obj: Box::new(target),
                    name: field.to_string(),
                    loc: parser.loc(),
                };
            }
            _ => break,
        }
    }
    // Determine the assignment operator type
    let op = match parser.peek() {
        Some(Token::Equals) => {
            parser.advance()?;
            None
        }
        Some(Token::PlusEq) => {
            parser.advance()?;
            Some(BinOp::Add)
        }
        Some(Token::MinusEq) => {
            parser.advance()?;
            Some(BinOp::Sub)
        }
        Some(Token::MulEq) => {
            parser.advance()?;
            Some(BinOp::Mul)
        }
        Some(Token::DivEq) => {
            parser.advance()?;
            Some(BinOp::Div)
        }
        Some(Token::ModEq) => {
            parser.advance()?;
            Some(BinOp::Mod)
        }
        _ => {
            return Err(JitError::parsing(
                "Expected assignment operator",
                loc.as_error_pos(),
            ));
        }
    };
    let rhs = parser.parse_expression()?;
    let value = if let Some(bop) = op {
        // x += y  →  x = x + y
        AstNode::Binary {
            op: bop,
            lhs: Box::new(target.clone()),
            rhs: Box::new(rhs),
            loc,
        }
    } else {
        rhs
    };
    Ok(Some(AstNode::Assign {
        target: Box::new(target),
        value: Box::new(value),
        loc,
    }))
}

// ---------------------------------------------------------------------------
// Function declaration
// ---------------------------------------------------------------------------

pub(super) fn parse_fun_decl<'source>(
    parser: &mut AstParser<'source>,
    exported: bool,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    let name = parser.expect_ident()?.to_string();
    parser.expect(Token::LParen)?;
    let params = super::parse_params_until(parser, Token::RParen)?;
    parser.expect(Token::RParen)?;
    // Optional return type and error kind
    parser.stream.skip_newlines();
    let mut error_kind = None;
    if parser.peek() == Some(Token::Arrow) {
        parser.advance()?; // '->'
        parser.expect_ident()?; // return type (ignored at runtime)
        parser.stream.skip_newlines();
        if parser.peek() == Some(Token::Not) {
            // '!' as error kind separator
            parser.advance()?;
            error_kind = Some(parser.expect_ident()?.to_string());
        }
    }
    parser.stream.skip_newlines();
    parser.expect(Token::LBrace)?;
    let body = parser.parse_block_stmts()?;
    Ok(AstNode::FunDecl {
        name,
        params,
        body,
        exported,
        loc,
        error_kind,
    })
}

// ---------------------------------------------------------------------------
// If / else
// ---------------------------------------------------------------------------

pub(super) fn parse_if_stmt<'source>(parser: &mut AstParser<'source>) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    parser.stream.skip_newlines();
    let cond = parser.parse_expression()?;
    parser.stream.skip_newlines();
    parser.expect(Token::LBrace)?;
    let then_block = parser.parse_block_stmts()?;
    parser.stream.skip_newlines();
    let else_block = if parser.peek() == Some(Token::Else) {
        parser.advance()?; // 'else'
        parser.stream.skip_newlines();
        if parser.peek() == Some(Token::If) {
            parser.advance()?;
            vec![parse_if_stmt(parser)?]
        } else {
            parser.stream.skip_newlines();
            parser.expect(Token::LBrace)?;
            parser.parse_block_stmts()?
        }
    } else {
        Vec::new()
    };
    Ok(AstNode::If {
        cond: Box::new(cond),
        then_block,
        else_block,
        loc,
    })
}

// ---------------------------------------------------------------------------
// While loop
// ---------------------------------------------------------------------------

pub(super) fn parse_while_loop<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    parser.stream.skip_newlines();
    let cond = parser.parse_expression()?;
    parser.stream.skip_newlines();
    parser.expect(Token::LBrace)?;
    let body = parser.parse_block_stmts()?;
    Ok(AstNode::While {
        cond: Box::new(cond),
        body,
        loc,
    })
}

// ---------------------------------------------------------------------------
// For loop
// ---------------------------------------------------------------------------

pub(super) fn parse_for_loop<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    parser.stream.skip_newlines();
    let var = parser.expect_ident()?.to_string();
    parser.expect(Token::In)?;
    let iter = parser.parse_expression()?;
    parser.stream.skip_newlines();
    parser.expect(Token::LBrace)?;
    let body = parser.parse_block_stmts()?;
    Ok(AstNode::For {
        var,
        iter: Box::new(iter),
        body,
        loc,
    })
}

// ---------------------------------------------------------------------------
// Use statement
// ---------------------------------------------------------------------------

pub(super) fn parse_use_stmt<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    let mut path = vec![parser.expect_ident()?.to_string()];
    loop {
        if parser.peek() == Some(Token::Dot) {
            parser.advance()?;
            path.push(parser.expect_ident()?.to_string());
        } else {
            break;
        }
    }
    Ok(AstNode::Use { path, loc })
}

// ---------------------------------------------------------------------------
// Switch statement
// ---------------------------------------------------------------------------

pub(super) fn parse_switch<'source>(parser: &mut AstParser<'source>) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    parser.stream.skip_newlines();
    let expr = parser.parse_expression()?;
    parser.stream.skip_newlines();
    parser.expect(Token::LBrace)?;
    let mut arms = Vec::new();
    loop {
        parser.stream.skip_newlines();
        if matches!(parser.peek(), None | Some(Token::RBrace)) {
            break;
        }
        let _arm_loc = parser.loc();
        // Parse patterns (value | value | ...)
        let mut patterns = Vec::new();
        if parser.peek() == Some(Token::Identifier("_")) {
            parser.advance()?; // wildcard — empty patterns = default
        } else {
            loop {
                patterns.push(parser.parse_expression()?);
                parser.stream.skip_newlines();
                if parser.peek() == Some(Token::Pipe) {
                    parser.advance()?;
                } else {
                    break;
                }
            }
        }
        parser.stream.skip_newlines();
        parser.expect(Token::Arrow)?;
        // Body: either a block or an expression
        parser.stream.skip_newlines();
        let body = if parser.peek() == Some(Token::LBrace) {
            parser.parse_block_stmts()?
        } else {
            vec![parser.parse_expression()?]
        };
        arms.push(SwitchArm { patterns, body });
    }
    parser.expect(Token::RBrace)?;
    Ok(AstNode::Switch {
        expr: Box::new(expr),
        arms,
        loc,
    })
}

// ---------------------------------------------------------------------------
// Async function
// ---------------------------------------------------------------------------

pub(super) fn parse_async_fun<'source>(
    parser: &mut AstParser<'source>,
) -> Result<AstNode, JitError> {
    let loc = parser.loc();
    let name = parser.expect_ident()?.to_string();
    parser.expect(Token::LParen)?;
    let params = super::parse_params_until(parser, Token::RParen)?;
    parser.expect(Token::RParen)?;
    parser.stream.skip_newlines();
    if parser.peek() == Some(Token::Arrow) {
        parser.advance()?;
        parser.expect_ident()?;
    }
    parser.expect(Token::LBrace)?;
    let body = parser.parse_block_stmts()?;
    Ok(AstNode::AsyncFun {
        name,
        params,
        body,
        loc,
    })
}
