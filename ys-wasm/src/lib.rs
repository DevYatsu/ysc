// This crate is only built for WASM targets via wasm-pack.
// `cargo build` without --target wasm32 will fail — that's expected.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;

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
        if output.is_empty() { error } else { format!("{}\n{}", output.trim_end(), error) }
    }
}
