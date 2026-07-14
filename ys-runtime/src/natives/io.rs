//! I/O built-ins: `print`, `str`.

use crate::context::NativeFn;
use crate::heap::ManagedObject;
use crate::value_fmt::stringify_value;
use rustc_hash::FxHashMap;
use std::io::Write as _;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

// ═══════════════════════════════════════════════════════════════
//  Native target — print() goes straight to stdout, zero overhead
// ═══════════════════════════════════════════════════════════════
#[cfg(not(target_arch = "wasm32"))]
pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("print".into(), Arc::new(|ctx, args| {
        for (i, val) in args.iter().enumerate() {
            if i > 0 { print!(" "); }
            print!("{}", stringify_value(ctx.as_ref(), *val));
        }
        println!();
        let _ = std::io::stdout().flush();
        Ok(Value::from_bits(0))
    }));

    fns.insert("str".into(), Arc::new(|ctx, args| {
        let [val] = args else {
            return Err(JitError::runtime("str() expects 1 argument", 0, 0));
        };
        let s = stringify_value(ctx.as_ref(), *val);
        if let Some(sso) = Value::sso(&s) {
            Ok(sso)
        } else {
            Ok(ctx.alloc(ManagedObject::String(Arc::from(s))))
        }
    }));
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
    if let Some(b) = buf { *guard = b; } else { guard.clear(); }
}

/// Take the captured output since the last `set_print_buf`.
#[cfg(target_arch = "wasm32")]
pub fn take_print_buf() -> Vec<u8> {
    let mut guard = PRINT_BUF.lock().unwrap();
    std::mem::take(&mut *guard)
}

#[cfg(target_arch = "wasm32")]
pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("print".into(), Arc::new(|ctx, args| {
        let mut buf = PRINT_BUF.lock().unwrap();
        for (i, val) in args.iter().enumerate() {
            if i > 0 { buf.push(b' '); }
            buf.extend_from_slice(stringify_value(ctx.as_ref(), *val).as_bytes());
        }
        buf.push(b'\n');
        Ok(Value::from_bits(0))
    }));

    fns.insert("str".into(), Arc::new(|ctx, args| {
        let [val] = args else {
            return Err(JitError::runtime("str() expects 1 argument", 0, 0));
        };
        let s = stringify_value(ctx.as_ref(), *val);
        if let Some(sso) = Value::sso(&s) {
            Ok(sso)
        } else {
            Ok(ctx.alloc(ManagedObject::String(Arc::from(s))))
        }
    }));
}
