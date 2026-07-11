//! Abstract Syntax Tree for YatsuScript.
//!
//! The parser produces an AST; the optimizer walks and transforms it;
//! the codegen walks it to emit bytecode instructions.

use crate::compiler::Loc;

pub type AstBlock = Vec<AstNode>;

// ── Operators ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp { Neg, Not }

// ── AST Node ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AstNode {
    // ── Literals ─
    Number(f64, Loc),
    Bool(bool, Loc),
    Nil(Loc),
    Str(String, Loc),
    Template { parts: Vec<TemplatePart>, loc: Loc },

    // ── Variables ─
    Ident(String, Loc),
    Assign { target: Box<AstNode>, value: Box<AstNode>, loc: Loc },

    // ── Expressions ─
    Binary { op: BinOp, lhs: Box<AstNode>, rhs: Box<AstNode>, loc: Loc },
    Unary  { op: UnaryOp, expr: Box<AstNode>, loc: Loc },

    // ── Control flow ─
    Block(AstBlock, Loc),
    If {
        cond:       Box<AstNode>,
        then_block: AstBlock,
        else_block: AstBlock,
        loc:        Loc,
    },
    While { cond: Box<AstNode>, body: AstBlock, loc: Loc },
    For   { var: String, iter: Box<AstNode>, body: AstBlock, loc: Loc },
    Return { value: Option<Box<AstNode>>, loc: Loc },

    // ── Calls ─
    FunCall { name: String, args: Vec<AstNode>, loc: Loc },
    MethodCall { obj: Box<AstNode>, method: String, args: Vec<AstNode>, loc: Loc },
    DynamicCall { callee: Box<AstNode>, args: Vec<AstNode>, loc: Loc },

    // ── Functions / Closures ─
    FunDecl {
        name:     String,
        params:   Vec<String>,
        body:     AstBlock,
        exported: bool,
        loc:      Loc,
    },
    Closure { params: Vec<String>, body: Box<AstNode>, is_move: bool, loc: Loc },

    // ── Collections ─
    ListLit(Vec<AstNode>, Loc),
    ListRepeat { val: Box<AstNode>, count: Box<AstNode>, loc: Loc },
    ObjectLit(Vec<(String, AstNode)>, Loc),
    Index  { obj: Box<AstNode>, index: Box<AstNode>, loc: Loc },
    Field  { obj: Box<AstNode>, name: String, loc: Loc },

    // ── Ranges ─
    Range {
        start: Box<AstNode>,
        end:   Box<AstNode>,
        step:  Option<Box<AstNode>>,
        loc:   Loc,
    },

    // ── Modules ─
    Use { path: Vec<String>, loc: Loc },
}

// ── Template parts ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TemplatePart {
    Text(String),
    Expr(Box<AstNode>),
}
