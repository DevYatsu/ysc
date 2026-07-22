//! I/O built-ins: `print`, `str`.

use crate::context::NativeCtx;
use crate::natives::{NativeRegistry, alloc_string};
use crate::value_fmt::stringify_value;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write as _;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// Get the source location of the current `print()` call, if available.
#[cfg(target_arch = "wasm32")]
fn print_loc_str() -> String {
    crate::vm::get_call_loc()
        .map(|(line, _)| format!(" [l.{}]", line))
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════
//  Native target — print() goes straight to stdout, zero overhead
// ═══════════════════════════════════════════════════════════════
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("print", |ctx: &NativeCtx, args| {
        for (i, val) in args.iter().enumerate() {
            if i > 0 {
                print!(" ");
            }
            print!("{}", stringify_value(ctx.as_ref(), *val));
        }
        println!();
        let _ = std::io::stdout().flush();
        Ok(Value::nil())
    });

    reg.insert("str", |ctx: &NativeCtx, args| {
        let [val] = args else {
            return Err(JitError::runtime("str() expects 1 argument", (0, 0)));
        };
        let s = stringify_value(ctx.as_ref(), *val);
        Ok(alloc_string(ctx.as_ref(), s))
    });
}

// ═══════════════════════════════════════════════════════════════
//  WASM target — capture print() output so it can be returned
//  to JavaScript via the eval() function
// ═══════════════════════════════════════════════════════════════
#[cfg(target_arch = "wasm32")]
use std::sync::Mutex;

#[cfg(target_arch = "wasm32")]
static PRINT_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Set (or clear) the print output capture buffer.  Call before `eval()`.
#[cfg(target_arch = "wasm32")]
pub fn set_print_buf(buf: Option<Vec<u8>>) {
    let mut guard = PRINT_BUF.lock().unwrap();
    if let Some(b) = buf {
        *guard = b;
    } else {
        guard.clear();
    }
}

/// Take the captured output since the last `set_print_buf`.
#[cfg(target_arch = "wasm32")]
pub fn take_print_buf() -> Vec<u8> {
    let mut guard = PRINT_BUF.lock().unwrap();
    std::mem::take(&mut *guard)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("print", |ctx: &NativeCtx, args| {
        let mut buf = PRINT_BUF.lock().unwrap();
        let loc = print_loc_str();
        if !loc.is_empty() {
            buf.extend_from_slice(loc.as_bytes());
        }
        for (i, val) in args.iter().enumerate() {
            if i > 0 {
                buf.push(b' ');
            }
            buf.extend_from_slice(stringify_value(ctx.as_ref(), *val).as_bytes());
        }
        buf.push(b'\n');
        Ok(Value::nil())
    });

    reg.insert("str", |ctx, args| {
        let [val] = args else {
            return Err(JitError::runtime("str() expects 1 argument", (0, 0)));
        };
        let s = stringify_value(ctx.as_ref(), *val);
        Ok(alloc_string(ctx.as_ref(), s))
    });
}
