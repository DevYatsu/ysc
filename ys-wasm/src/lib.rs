// This crate is only built for WASM targets via wasm-pack.
// `cargo build` without --target wasm32 will fail — that's expected.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════
//  Eval — run code, return print lines as structured list
// ═══════════════════════════════════════════════════════════════

/// Evaluate source, capture print output as structured lines.
#[wasm_bindgen(js_name = _eval)]
pub fn eval(source: &str) -> JsValue {
    ys_runtime::natives::io::set_print_buf(Some(Vec::new()));

    let result = std::panic::catch_unwind(|| {
        match ys_core::codegen::Codegen::compile(source) {
            Ok(program) => match ys_runtime::vm::run_interpreter(program) {
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

    let raw = String::from_utf8_lossy(&ys_runtime::natives::io::take_print_buf()).into_owned();
    let lines = raw.split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            // Each line is "[l.N] value" or just "value"
            let obj = js_sys::Object::new();
            if let Some(pos) = l.find("] ") {
                let tag = &l[..pos+1]; // "[l.N]"
                let val = &l[pos+2..];
                js_sys::Reflect::set(&obj, &"value".into(), &val.into()).ok();
                let line_str = tag.trim_start_matches("[l.").trim_end_matches("]");
                if let Ok(n) = line_str.parse::<u32>() {
                    js_sys::Reflect::set(&obj, &"line".into(), &JsValue::from(n)).ok();
                }
            } else {
                js_sys::Reflect::set(&obj, &"value".into(), &l.into()).ok();
            }
            obj.into()
        })
        .collect::<Vec<JsValue>>();

    let out = js_sys::Object::new();
    match error {
        Some(e) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(false)).ok();
            js_sys::Reflect::set(&out, &"error".into(), &e.into()).ok();
            js_sys::Reflect::set(&out, &"lines".into(), &JsValue::from(lines)).ok();
        }
        None => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(true)).ok();
            js_sys::Reflect::set(&out, &"lines".into(), &JsValue::from(lines)).ok();
        }
    }
    out.into()
}

// ═══════════════════════════════════════════════════════════════
//  AST — return as a structured JSON tree
// ═══════════════════════════════════════════════════════════════

#[wasm_bindgen(js_name = _parseAst)]
pub fn parse_ast(source: &str) -> JsValue {
    let out = js_sys::Object::new();
    let result = std::panic::catch_unwind(|| {
        let mut parser = ys_core::ast_parser::AstParser::new(source)?;
        parser.parse_program()
    });
    match result {
        Ok(Ok(ast)) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(true)).ok();
            js_sys::Reflect::set(&out, &"data".into(), &ast_block_to_js(&ast)).ok();
        }
        Ok(Err(e)) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(false)).ok();
            js_sys::Reflect::set(&out, &"error".into(), &e.to_string().into()).ok();
        }
        Err(_) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(false)).ok();
            js_sys::Reflect::set(&out, &"error".into(), &"Panic during AST parsing".into()).ok();
        }
    }
    out.into()
}

fn ast_block_to_js(block: &[ys_core::ast::AstNode]) -> JsValue {
    let arr = js_sys::Array::new();
    for node in block {
        arr.push(&ast_node_to_js(node));
    }
    arr.into()
}

fn add_loc(o: &js_sys::Object, node: &ys_core::ast::AstNode) {
    if let Some(loc) = node_loc(node) {
        js_sys::Reflect::set(o, &"line".into(), &JsValue::from(loc.line)).ok();
        js_sys::Reflect::set(o, &"col".into(), &JsValue::from(loc.col)).ok();
    }
}

fn node_loc(node: &ys_core::ast::AstNode) -> Option<ys_core::compiler::Loc> {
    use ys_core::ast::*;
    match node {
        AstNode::Number(_, l) => Some(*l),
        AstNode::Bool(_, l) => Some(*l),
        AstNode::Nil(l) => Some(*l),
        AstNode::Str(_, l) => Some(*l),
        AstNode::Ident(_, l) => Some(*l),
        AstNode::Break(l) => Some(*l),
        AstNode::Block(_, l) => Some(*l),
        AstNode::ListLit(_, l) => Some(*l),
        AstNode::ObjectLit(_, l) => Some(*l),
        AstNode::Yield(_, l) => Some(*l),
        AstNode::Await(_, l) => Some(*l),
        AstNode::Assign { loc, .. } => Some(*loc),
        AstNode::Unary { loc, .. } => Some(*loc),
        AstNode::If { loc, .. } => Some(*loc),
        AstNode::While { loc, .. } => Some(*loc),
        AstNode::For { loc, .. } => Some(*loc),
        AstNode::Return { loc, .. } => Some(*loc),
        AstNode::FunCall { loc, .. } => Some(*loc),
        AstNode::DynamicCall { loc, .. } => Some(*loc),
        AstNode::FunDecl { loc, .. } => Some(*loc),
        AstNode::AsyncFun { loc, .. } => Some(*loc),
        AstNode::Closure { loc, .. } => Some(*loc),
        AstNode::Index { loc, .. } => Some(*loc),
        AstNode::Field { loc, .. } => Some(*loc),
        AstNode::Range { loc, .. } => Some(*loc),
        AstNode::Switch { loc, .. } => Some(*loc),
        AstNode::Fail { loc, .. } => Some(*loc),
        AstNode::Use { loc, .. } => Some(*loc),
        AstNode::ErrorDecl { loc, .. } => Some(*loc),
        AstNode::ErrorEnum { loc, .. } => Some(*loc),
        AstNode::Fallback { loc, .. } => Some(*loc),
        AstNode::Except { loc, .. } => Some(*loc),
        AstNode::ListRepeat { loc, .. } => Some(*loc),
        AstNode::Template { loc, .. } => Some(*loc),
        AstNode::Binary { loc, .. } => Some(*loc),
    }
}

fn ast_node_to_js(node: &ys_core::ast::AstNode) -> JsValue {
    use ys_core::ast::*;
    let o = js_sys::Object::new();
    add_loc(&o, node);
    match node {
        AstNode::Number(n, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"number".into()).ok();
            js_sys::Reflect::set(&o, &"value".into(), &JsValue::from(*n)).ok();
        }
        AstNode::Bool(b, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"bool".into()).ok();
            js_sys::Reflect::set(&o, &"value".into(), &JsValue::from(*b)).ok();
        }
        AstNode::Nil(_) => {
            js_sys::Reflect::set(&o, &"type".into(), &"nil".into()).ok();
        }
        AstNode::Str(s, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"str".into()).ok();
            js_sys::Reflect::set(&o, &"value".into(), &s.into()).ok();
        }
        AstNode::Ident(s, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"ident".into()).ok();
            js_sys::Reflect::set(&o, &"name".into(), &s.into()).ok();
        }
        AstNode::Assign { target, value, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"assign".into()).ok();
            js_sys::Reflect::set(&o, &"target".into(), &ast_node_to_js(target)).ok();
            js_sys::Reflect::set(&o, &"value".into(), &ast_node_to_js(value)).ok();
        }
        AstNode::Binary { op, lhs, rhs, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"binary".into()).ok();
            js_sys::Reflect::set(&o, &"op".into(), &format!("{:?}", op).into()).ok();
            js_sys::Reflect::set(&o, &"left".into(), &ast_node_to_js(lhs)).ok();
            js_sys::Reflect::set(&o, &"right".into(), &ast_node_to_js(rhs)).ok();
        }
        AstNode::Block(stmts, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"block".into()).ok();
            js_sys::Reflect::set(&o, &"statements".into(), &ast_block_to_js(stmts)).ok();
        }
        AstNode::FunCall { name, args, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"call".into()).ok();
            js_sys::Reflect::set(&o, &"name".into(), &name.into()).ok();
            let arr = js_sys::Array::new();
            for a in args { arr.push(&ast_node_to_js(a)); }
            js_sys::Reflect::set(&o, &"args".into(), &arr.into()).ok();
        }
        AstNode::FunDecl { name, params, body, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"function".into()).ok();
            js_sys::Reflect::set(&o, &"name".into(), &name.into()).ok();
            let p = js_sys::Array::new();
            for param in params { p.push(&param.into()); }
            js_sys::Reflect::set(&o, &"params".into(), &p.into()).ok();
            js_sys::Reflect::set(&o, &"body".into(), &ast_block_to_js(body)).ok();
        }
        AstNode::Return { value, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"return".into()).ok();
            if let Some(v) = value {
                js_sys::Reflect::set(&o, &"value".into(), &ast_node_to_js(v)).ok();
            }
        }
        // Shorthand for other literal-like nodes
        AstNode::ListLit(elems, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"list".into()).ok();
            let arr = js_sys::Array::new();
            for e in elems { arr.push(&ast_node_to_js(e)); }
            js_sys::Reflect::set(&o, &"elements".into(), &arr.into()).ok();
        }
        AstNode::ObjectLit(fields, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"object".into()).ok();
            let arr = js_sys::Array::new();
            for (k, v) in fields {
                let pair = js_sys::Object::new();
                js_sys::Reflect::set(&pair, &"key".into(), &k.into()).ok();
                js_sys::Reflect::set(&pair, &"value".into(), &ast_node_to_js(v)).ok();
                arr.push(&pair.into());
            }
            js_sys::Reflect::set(&o, &"fields".into(), &arr.into()).ok();
        }
        AstNode::For { var, iter, body, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"for".into()).ok();
            js_sys::Reflect::set(&o, &"var".into(), &var.into()).ok();
            js_sys::Reflect::set(&o, &"iter".into(), &ast_node_to_js(iter)).ok();
            js_sys::Reflect::set(&o, &"body".into(), &ast_block_to_js(body)).ok();
        }
        AstNode::If { cond, then_block, else_block, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"if".into()).ok();
            js_sys::Reflect::set(&o, &"cond".into(), &ast_node_to_js(cond)).ok();
            js_sys::Reflect::set(&o, &"then".into(), &ast_block_to_js(then_block)).ok();
            if !else_block.is_empty() {
                js_sys::Reflect::set(&o, &"else".into(), &ast_block_to_js(else_block)).ok();
            }
        }
        AstNode::While { cond, body, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"while".into()).ok();
            js_sys::Reflect::set(&o, &"cond".into(), &ast_node_to_js(cond)).ok();
            js_sys::Reflect::set(&o, &"body".into(), &ast_block_to_js(body)).ok();
        }
        AstNode::Closure { params, body, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"closure".into()).ok();
            let p = js_sys::Array::new();
            for param in params { p.push(&param.into()); }
            js_sys::Reflect::set(&o, &"params".into(), &p.into()).ok();
            js_sys::Reflect::set(&o, &"body".into(), &ast_node_to_js(body)).ok();
        }
        AstNode::Index { obj, index, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"index".into()).ok();
            js_sys::Reflect::set(&o, &"object".into(), &ast_node_to_js(obj)).ok();
            js_sys::Reflect::set(&o, &"index".into(), &ast_node_to_js(index)).ok();
        }
        AstNode::Field { obj, name, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"field".into()).ok();
            js_sys::Reflect::set(&o, &"object".into(), &ast_node_to_js(obj)).ok();
            js_sys::Reflect::set(&o, &"name".into(), &name.into()).ok();
        }
        AstNode::Range { start, end, step, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"range".into()).ok();
            js_sys::Reflect::set(&o, &"start".into(), &ast_node_to_js(start)).ok();
            js_sys::Reflect::set(&o, &"end".into(), &ast_node_to_js(end)).ok();
            if let Some(s) = step {
                js_sys::Reflect::set(&o, &"step".into(), &ast_node_to_js(s)).ok();
            }
        }
        AstNode::Unary { op, expr, .. } => {
            js_sys::Reflect::set(&o, &"type".into(), &"unary".into()).ok();
            js_sys::Reflect::set(&o, &"op".into(), &format!("{:?}", op).into()).ok();
            js_sys::Reflect::set(&o, &"expr".into(), &ast_node_to_js(expr)).ok();
        }
        AstNode::Yield(v, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"yield".into()).ok();
            js_sys::Reflect::set(&o, &"value".into(), &ast_node_to_js(v)).ok();
        }
        AstNode::Await(v, _) => {
            js_sys::Reflect::set(&o, &"type".into(), &"await".into()).ok();
            js_sys::Reflect::set(&o, &"expr".into(), &ast_node_to_js(v)).ok();
        }
        _ => {
            js_sys::Reflect::set(&o, &"type".into(), &format!("{:?}", node).into()).ok();
        }
    }
    o.into()
}

// ═══════════════════════════════════════════════════════════════
//  Format — format YatsuScript source code via ys-core
// ═══════════════════════════════════════════════════════════════

#[wasm_bindgen(js_name = _format)]
pub fn format(source: &str) -> String {
    ys_core::fmt::format_source(source)
}

// ═══════════════════════════════════════════════════════════════
//  Bytecode — return as structured list of instructions
// ═══════════════════════════════════════════════════════════════

#[wasm_bindgen(js_name = _disassemble)]
pub fn disassemble(source: &str) -> JsValue {
    let out = js_sys::Object::new();
    let result = std::panic::catch_unwind(|| {
        ys_core::codegen::Codegen::compile(source)
    });
    match result {
        Ok(Ok(program)) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(true)).ok();
            let funcs = js_sys::Array::new();
            for (fi, func) in program.functions.iter().enumerate() {
                let f = js_sys::Object::new();
                let default_name = Arc::from("?");
                let name = program.string_pool.get(func.name_id as usize).unwrap_or(&default_name);
                js_sys::Reflect::set(&f, &"index".into(), &JsValue::from(fi as u32)).ok();
                js_sys::Reflect::set(&f, &"name".into(), &name.as_ref().into()).ok();
                js_sys::Reflect::set(&f, &"params".into(), &JsValue::from(func.params_count as u32)).ok();
                js_sys::Reflect::set(&f, &"locals".into(), &JsValue::from(func.locals_count as u32)).ok();
                let instrs = js_sys::Array::new();
                for (_, ins) in func.instructions.iter().enumerate() {
                    instrs.push(&JsValue::from(format!("{:?}", ins)));
                }
                js_sys::Reflect::set(&f, &"instructions".into(), &instrs.into()).ok();
                funcs.push(&f.into());
            }
            js_sys::Reflect::set(&out, &"functions".into(), &funcs.into()).ok();

            let main = js_sys::Array::new();
            for (_, ins) in program.instructions.iter().enumerate() {
                main.push(&JsValue::from(format!("{:?}", ins)));
            }
            js_sys::Reflect::set(&out, &"main".into(), &main.into()).ok();
        }
        Ok(Err(e)) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(false)).ok();
            js_sys::Reflect::set(&out, &"error".into(), &e.to_string().into()).ok();
        }
        Err(_) => {
            js_sys::Reflect::set(&out, &"success".into(), &JsValue::from(false)).ok();
            js_sys::Reflect::set(&out, &"error".into(), &"Panic during disassembly".into()).ok();
        }
    }
    out.into()
}
