// This crate is only built for WASM targets via wasm-pack.
// `cargo build` without --target wasm32 will fail — that's expected.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;

/// Parse source code and return its AST as a formatted string.
/// Useful for debugging and education in the playground.
#[wasm_bindgen(js_name = _parse_ast)]
pub fn parse_ast(source: &str) -> String {
    let result = std::panic::catch_unwind(|| {
        let mut parser = match ys_core::ast_parser::AstParser::new(source) {
            Ok(p) => p,
            Err(e) => return format!("ERROR:{}", e),
        };
        match parser.parse_program() {
            Ok(ast) => format_ast_block(&ast, 0),
            Err(e) => format!("ERROR:{}", e),
        }
    });
    result.unwrap_or_else(|_| "Panic during AST parsing".to_string())
}

/// Recursively format an AST block with indentation.
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
        AstNode::Assign { target, value, .. } => {
            format!("{}Assign(\n{}  target: {},\n{}  value: {},\n{})",
                indent, indent, format_ast_node(target, depth+2),
                indent, format_ast_node(value, depth+2), indent)
        }
        AstNode::Binary { op, lhs, rhs, .. } => {
            format!("{}Binary({:?},\n{}  {},\n{}  {})",
                indent, op, indent, format_ast_node(lhs, depth+2),
                indent, format_ast_node(rhs, depth+2))
        }
        AstNode::Unary { op, expr, .. } => {
            format!("{}Unary({:?}, {})", indent, op, format_ast_node(expr, depth+1))
        }
        AstNode::Block(stmts, _) => format_ast_block(stmts, depth),
        AstNode::If { cond, then_block, else_block, .. } => {
            let mut s = format!("{}If(\n{}  cond: {},\n", indent, indent, format_ast_node(cond, depth+2));
            s.push_str(&format!("{}  then: {},\n", indent, format_ast_block(then_block, depth+2)));
            if !else_block.is_empty() {
                s.push_str(&format!("{}  else: {},\n", indent, format_ast_block(else_block, depth+2)));
            }
            s.push_str(&format!("{})", indent));
            s
        }
        AstNode::While { cond, body, .. } => {
            format!("{}While(\n{}  cond: {},\n{}  body: {},\n{})",
                indent, indent, format_ast_node(cond, depth+2),
                indent, format_ast_block(body, depth+2), indent)
        }
        AstNode::For { var, iter, body, .. } => {
            format!("{}For({},\n{}  iter: {},\n{}  body: {},\n{})",
                indent, var, indent, format_ast_node(iter, depth+2),
                indent, format_ast_block(body, depth+2), indent)
        }
        AstNode::Return { value, .. } => {
            match value {
                Some(v) => format!("{}Ret({})", indent, format_ast_node(v, depth+1)),
                None => format!("{}Ret", indent),
            }
        }
        AstNode::Yield(v, _) => format!("{}Yield({})", indent, format_ast_node(v, depth+1)),
        AstNode::FunCall { name, args, .. } => {
            let args_str: Vec<String> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}Call({}, [{}])", indent, name, args_str.join(", "))
        }
        AstNode::MethodCall { obj, method, args, .. } => {
            let args_str: Vec<String> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}MethodCall({}.{}({}))", indent, format_ast_node(obj, depth+2), method, args_str.join(", "))
        }
        AstNode::DynamicCall { callee, args, .. } => {
            let args_str: Vec<String> = args.iter().map(|a| format_ast_node(a, depth+1)).collect();
            format!("{}DynCall({}, [{}])", indent, format_ast_node(callee, depth+2), args_str.join(", "))
        }
        AstNode::FunDecl { name, params, body, .. } => {
            format!("{}Fun({}({}) {})", indent, name, params.join(", "), format_ast_block(body, depth+1))
        }
        AstNode::Closure { params, body, .. } => {
            format!("{}Closure(|{}| {})", indent, params.join(", "), format_ast_node(body, depth+1))
        }
        AstNode::ListLit(elems, _) => {
            let elems_str: Vec<String> = elems.iter().map(|e| format_ast_node(e, depth+1)).collect();
            format!("{}List[{}]", indent, elems_str.join(", "))
        }
        AstNode::ObjectLit(fields, _) => {
            let f_str: Vec<String> = fields.iter().map(|(k, v)| {
                format!("{}: {}", k, format_ast_node(v, depth+1))
            }).collect();
            format!("{}Obj{{{}}}", indent, f_str.join(", "))
        }
        AstNode::Index { obj, index, .. } => {
            format!("{}Index({}, {})", indent, format_ast_node(obj, depth+2), format_ast_node(index, depth+2))
        }
        AstNode::Field { obj, name, .. } => {
            format!("{}Field({}.{})", indent, format_ast_node(obj, depth+2), name)
        }
        AstNode::Range { start, end, step, .. } => {
            match step {
                Some(s) => format!("{}Range({}..{}.step({}))", indent,
                    format_ast_node(start, depth+2), format_ast_node(end, depth+2), format_ast_node(s, depth+2)),
                None => format!("{}Range({}..{})", indent,
                    format_ast_node(start, depth+2), format_ast_node(end, depth+2)),
            }
        }
        AstNode::Await(expr, _) => format!("{}Await({})", indent, format_ast_node(expr, depth+1)),
        AstNode::AsyncFun { name, params, body, .. } => {
            format!("{}AsyncFun({}({}) {})", indent, name, params.join(", "), format_ast_block(body, depth+1))
        }
        AstNode::Switch { expr, arms, .. } => {
            let arms_str: Vec<String> = arms.iter().map(|arm| {
                let pats: Vec<String> = arm.patterns.iter().map(|p| format_ast_node(p, depth+2)).collect();
                format!("{}  {} => {}", indent, pats.join(" | "), format_ast_block(&arm.body, depth+2))
            }).collect();
            format!("{}Switch({}, [{}])", indent, format_ast_node(expr, depth+2), arms_str.join(", "))
        }
        AstNode::Break(_) => format!("{}Break", indent),
        AstNode::Fail { type_name, .. } => format!("{}Fail({})", indent, type_name),
        AstNode::Use { path, .. } => format!("{}Use({})", indent, path.join(".")),
        AstNode::ErrorDecl { name, .. } => format!("{}Error({})", indent, name),
        AstNode::ErrorEnum { name, variants, .. } => {
            format!("{}ErrorEnum({}, [{}])", indent, name, variants.join(", "))
        }
        AstNode::Fallback { expr, default, .. } => {
            format!("{}Fallback({}, {})", indent, format_ast_node(expr, depth+2), format_ast_node(default, depth+2))
        }
        AstNode::Except { expr, arms, .. } => {
            let arms_str: Vec<String> = arms.iter().map(|arm| {
                format!("{}  |{}| {}", indent, arm.type_name, format_ast_block(&arm.body, depth+2))
            }).collect();
            format!("{}Except({}, [{}])", indent, format_ast_node(expr, depth+2), arms_str.join(", "))
        }
        AstNode::ListRepeat { val, count, .. } => {
            format!("{}ListRepeat({}, {})", indent, format_ast_node(val, depth+2), format_ast_node(count, depth+2))
        }
        AstNode::Template { parts, .. } => format!("{}Template({:?})", indent, parts),
    }
}

/// Compile source code and return the disassembled bytecode as a string.
#[wasm_bindgen(js_name = _disassemble)]
pub fn disassemble(source: &str) -> String {
    let result = std::panic::catch_unwind(|| {
        match ys_core::codegen::Codegen::compile(source) {
            Ok(program) => {
                use ys_core::compiler::Instruction;
                let mut out = String::new();
                out.push_str(&format!("Functions: {}\n", program.functions.len()));
                out.push_str(&format!("Locals: {}\n", program.locals_count));
                out.push_str(&format!("Globals: {}\n\n", program.globals_count));

                for (fi, func) in program.functions.iter().enumerate() {
                    let default_name = std::sync::Arc::from("?");
                    let name = program.string_pool.get(func.name_id as usize).unwrap_or(&default_name);
                    out.push_str(&format!("--- Function {}: {} (params={}, locals={}) ---\n",
                        fi, name, func.params_count, func.locals_count));
                    for (i, instr) in func.instructions.iter().enumerate() {
                        out.push_str(&format!("  {:>4}: {:?}\n", i, instr));
                    }
                    out.push('\n');
                }

                out.push_str("--- Main ---\n");
                for (i, instr) in program.instructions.iter().enumerate() {
                    out.push_str(&format!("  {:>4}: {:?}\n", i, instr));
                }
                out
            }
            Err(e) => format!("ERROR:{}", e),
        }
    });
    result.unwrap_or_else(|_| "Panic during disassembly".to_string())
}

/// Evaluate a YatsuScript source string.
///
/// All `print()` output is captured and returned along with any errors.
#[wasm_bindgen(js_name = _eval)]
pub fn eval(source: &str) -> String {
    use ys_core::codegen::Codegen;
    use ys_runtime::vm::run_interpreter;

    ys_runtime::natives::io::set_print_buf(Some(Vec::new()));

    let result = std::panic::catch_unwind(|| {
        match Codegen::compile(source) {
            Ok(program) => match run_interpreter(program) {
                Ok(_) => None,
                Err(e) => Some(format!("Runtime error: {}", e)),
            },
            Err(e) => Some(format!("Compile error: {}", e)),
        }
    });

    let error = match result {
        Ok(Some(e)) => e,
        Ok(None) => String::new(),
        Err(_) => "Panic during evaluation".to_string(),
    };

    let output = String::from_utf8_lossy(&ys_runtime::natives::io::take_print_buf()).into_owned();

    if error.is_empty() {
        if output.is_empty() { "ok".to_string() } else { output.trim_end().to_string() }
    } else {
        // Prefix errors with "ERROR:" marker so the playground can detect them
        // even when print output is present.
        if output.trim().is_empty() {
            format!("ERROR:{}", error)
        } else {
            format!("{}\nERROR:{}", output.trim_end(), error)
        }
    }
}
