// This crate is only built for WASM targets via wasm-pack.
// `cargo build` without --target wasm32 will fail — that's expected.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;

/// A structured result returned by all WASM functions.
#[wasm_bindgen]
pub struct YsResult {
    success: bool,
    output: String,
    error: String,
}

#[wasm_bindgen]
impl YsResult {
    pub fn ok(output: String) -> Self {
        Self { success: true, output, error: String::new() }
    }
    pub fn err(msg: String) -> Self {
        Self { success: false, output: String::new(), error: msg }
    }

    #[wasm_bindgen(getter)]
    pub fn success(&self) -> bool { self.success }
    #[wasm_bindgen(getter)]
    pub fn output(&self) -> String { self.output.clone() }
    #[wasm_bindgen(getter)]
    pub fn error(&self) -> String { self.error.clone() }
}

// ═══════════════════════════════════════════════════════════════
//  AST parser
// ═══════════════════════════════════════════════════════════════

/// Parse source code and return its AST as a formatted string.
#[wasm_bindgen(js_name = _parseAst)]
pub fn parse_ast(source: &str) -> YsResult {
    std::panic::catch_unwind(|| {
        let mut parser = match ys_core::ast_parser::AstParser::new(source) {
            Ok(p) => p,
            Err(e) => return YsResult::err(e.to_string()),
        };
        match parser.parse_program() {
            Ok(ast) => YsResult::ok(format_ast_block(&ast, 0)),
            Err(e) => YsResult::err(e.to_string()),
        }
    })
    .unwrap_or_else(|_| YsResult::err("Panic during AST parsing".into()))
}

fn format_ast_block(block: &[ys_core::ast::AstNode], depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut out = String::new();
    out.push_str(&format!("{}[\n", indent));
    for node in block {
        out.push_str(&format_ast_node(node, depth + 1));
        out.push_str(",\n");
    }
    out.push_str(&format!("{}]", indent));
    out
}

fn format_ast_node(node: &ys_core::ast::AstNode, depth: usize) -> String {
    use ys_core::ast::*;
    let indent = "  ".repeat(depth);
    match node {
        AstNode::Number(n, _) => format!("{}Num({})", indent, n),
        AstNode::Bool(b, _) => format!("{}Bool({})", indent, b),
        AstNode::Nil(_) => format!("{}Nil", indent),
        AstNode::Str(s, _) => format!("{}Str({:?})", indent, s),
        AstNode::Ident(s, _) => format!("{}Ident({})", indent, s),
        AstNode::Assign { target, value, .. } =>
            format!("{}Assign(\n{}  target: {},\n{}  value: {},\n{})",
                indent, indent, format_ast_node(target, depth+2),
                indent, format_ast_node(value, depth+2), indent),
        AstNode::Binary { op, lhs, rhs, .. } =>
            format!("{}Binary({:?},\n{}  {},\n{}  {})",
                indent, op, indent, format_ast_node(lhs, depth+2),
                indent, format_ast_node(rhs, depth+2)),
        AstNode::Unary { op, expr, .. } =>
            format!("{}Unary({:?}, {})", indent, op, format_ast_node(expr, depth+1)),
        AstNode::Block(stmts, _) => format_ast_block(stmts, depth),
        AstNode::If { cond, then_block, else_block, .. } => {
            let mut s = format!("{}If(\n{}  cond: {},\n", indent, indent, format_ast_node(cond, depth+2));
            s.push_str(&format!("{}  then: {},\n", indent, format_ast_block(then_block, depth+2)));
            if !else_block.is_empty() {
                s.push_str(&format!("{}  else: {},\n", indent, format_ast_block(else_block, depth+2)));
            }
            s.push_str(&format!("{})", indent)); s
        }
        AstNode::While { cond, body, .. } =>
            format!("{}While(\n{}  cond: {},\n{}  body: {},\n{})",
                indent, indent, format_ast_node(cond, depth+2),
                indent, format_ast_block(body, depth+2), indent),
        AstNode::For { var, iter, body, .. } =>
            format!("{}For({},\n{}  iter: {},\n{}  body: {},\n{})",
                indent, var, indent, format_ast_node(iter, depth+2),
                indent, format_ast_block(body, depth+2), indent),
        AstNode::Return { value, .. } => match value {
            Some(v) => format!("{}Ret({})", indent, format_ast_node(v, depth+1)),
            None => format!("{}Ret", indent),
        },
        AstNode::Yield(v, _) => format!("{}Yield({})", indent, format_ast_node(v, depth+1)),
        AstNode::FunCall { name, args, .. } => {
            let a: Vec<_> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}Call({}, [{}])", indent, name, a.join(", "))
        }
        AstNode::MethodCall { obj, method, args, .. } => {
            let a: Vec<_> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}MethodCall({}.{}({}))", indent, format_ast_node(obj, depth+2), method, a.join(", "))
        }
        AstNode::DynamicCall { callee, args, .. } => {
            let a: Vec<_> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}DynCall({}, [{}])", indent, format_ast_node(callee, depth+2), a.join(", "))
        }
        AstNode::FunDecl { name, params, body, .. } =>
            format!("{}Fun({}({}) {})", indent, name, params.join(", "), format_ast_block(body, depth+1)),
        AstNode::Closure { params, body, .. } =>
            format!("{}Closure(|{}| {})", indent, params.join(", "), format_ast_node(body, depth+1)),
        AstNode::ListLit(elems, _) => {
            let a: Vec<_> = elems.iter().map(|e| format_ast_node(e, depth+1)).collect();
            format!("{}List[{}]", indent, a.join(", "))
        }
        AstNode::ObjectLit(fields, _) => {
            let a: Vec<_> = fields.iter().map(|(k, v)| format!("{}: {}", k, format_ast_node(v, depth+1))).collect();
            format!("{}Obj{{{}}}", indent, a.join(", "))
        }
        AstNode::Index { obj, index, .. } =>
            format!("{}Index({}, {})", indent, format_ast_node(obj, depth+2), format_ast_node(index, depth+2)),
        AstNode::Field { obj, name, .. } =>
            format!("{}Field({}.{})", indent, format_ast_node(obj, depth+2), name),
        AstNode::Range { start, end, step, .. } => match step {
            Some(s) => format!("{}Range({}..{}.step({}))", indent,
                format_ast_node(start, depth+2), format_ast_node(end, depth+2), format_ast_node(s, depth+2)),
            None => format!("{}Range({}..{})", indent,
                format_ast_node(start, depth+2), format_ast_node(end, depth+2)),
        },
        AstNode::Await(expr, _) => format!("{}Await({})", indent, format_ast_node(expr, depth+1)),
        AstNode::AsyncFun { name, params, body, .. } =>
            format!("{}AsyncFun({}({}) {})", indent, name, params.join(", "), format_ast_block(body, depth+1)),
        AstNode::Switch { expr, arms, .. } => {
            let a: Vec<_> = arms.iter().map(|arm| {
                let p: Vec<_> = arm.patterns.iter().map(|p| format_ast_node(p, depth+2)).collect();
                format!("{}  {} => {}", indent, p.join(" | "), format_ast_block(&arm.body, depth+2))
            }).collect();
            format!("{}Switch({}, [{}])", indent, format_ast_node(expr, depth+2), a.join(", "))
        }
        AstNode::Break(_) => format!("{}Break", indent),
        AstNode::Fail { type_name, .. } => format!("{}Fail({})", indent, type_name),
        AstNode::Use { path, .. } => format!("{}Use({})", indent, path.join(".")),
        AstNode::ErrorDecl { name, .. } => format!("{}Error({})", indent, name),
        AstNode::ErrorEnum { name, variants, .. } =>
            format!("{}ErrorEnum({}, [{}])", indent, name, variants.join(", ")),
        AstNode::Fallback { expr, default, .. } =>
            format!("{}Fallback({}, {})", indent, format_ast_node(expr, depth+2), format_ast_node(default, depth+2)),
        AstNode::Except { expr, arms, .. } => {
            let a: Vec<_> = arms.iter().map(|arm| {
                format!("{}  |{}| {}", indent, arm.type_name, format_ast_block(&arm.body, depth+2))
            }).collect();
            format!("{}Except({}, [{}])", indent, format_ast_node(expr, depth+2), a.join(", "))
        }
        AstNode::ListRepeat { val, count, .. } =>
            format!("{}ListRepeat({}, {})", indent, format_ast_node(val, depth+2), format_ast_node(count, depth+2)),
        AstNode::Template { parts, .. } => format!("{}Template({:?})", indent, parts),
    }
}

// ═══════════════════════════════════════════════════════════════
//  Bytecode disassembler
// ═══════════════════════════════════════════════════════════════

/// Compile source and return disassembled bytecode.
#[wasm_bindgen(js_name = _disassemble)]
pub fn disassemble(source: &str) -> YsResult {
    std::panic::catch_unwind(|| {
        match ys_core::codegen::Codegen::compile(source) {
            Ok(program) => {
                use ys_core::compiler::Instruction;
                let mut out = String::new();
                out.push_str(&format!("Functions: {}\n", program.functions.len()));
                out.push_str(&format!("Locals: {}\n\n", program.locals_count));

                for (fi, func) in program.functions.iter().enumerate() {
                    let dflt = std::sync::Arc::from("?");
                    let name = program.string_pool.get(func.name_id as usize).unwrap_or(&dflt);
                    out.push_str(&format!("--- fn {}: {} (p={}, l={}) ---\n", fi, name, func.params_count, func.locals_count));
                    for (i, ins) in func.instructions.iter().enumerate() {
                        out.push_str(&format!("  {:>4}: {:?}\n", i, ins));
                    }
                    out.push('\n');
                }
                out.push_str("--- main ---\n");
                for (i, ins) in program.instructions.iter().enumerate() {
                    out.push_str(&format!("  {:>4}: {:?}\n", i, ins));
                }
                YsResult::ok(out)
            }
            Err(e) => YsResult::err(e.to_string()),
        }
    })
    .unwrap_or_else(|_| YsResult::err("Panic during disassembly".into()))
}

// ═══════════════════════════════════════════════════════════════
//  Eval — run code and capture print output
// ═══════════════════════════════════════════════════════════════

/// Evaluate source, capture print output, return structured result.
#[wasm_bindgen(js_name = _eval)]
pub fn eval(source: &str) -> YsResult {
    ys_runtime::natives::io::set_print_buf(Some(Vec::new()));

    let result = std::panic::catch_unwind(|| {
        use ys_core::codegen::Codegen;
        use ys_runtime::vm::run_interpreter;

        match Codegen::compile(source) {
            Ok(program) => match run_interpreter(program) {
                Ok(_) => None,
                Err(e) => Some(e.to_string()),
            },
            Err(e) => Some(e.to_string()),
        }
    });

    let error = match result {
        Ok(Some(msg)) => Some(msg),
        Ok(None) => None,
        Err(_) => Some("Panic during evaluation".into()),
    };

    let output = String::from_utf8_lossy(&ys_runtime::natives::io::take_print_buf())
        .trim_end()
        .to_string();

    match error {
        Some(e) => YsResult { success: false, output, error: e },
        None => YsResult::ok(output),
    }
}
