//! I/O built-ins: `print`, `str`.

use crate::context::NativeFn;
use crate::heap::ManagedObject;
use crate::value_fmt::stringify_value;
use rustc_hash::FxHashMap;
use std::io::Write as _;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

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
            return Err(JitError::runtime(
                "str() expects 1 argument",
                0, 0,
            ));
        };
        let s = stringify_value(ctx.as_ref(), *val);
        if let Some(sso) = Value::sso(&s) {
            Ok(sso)
        } else {
            Ok(ctx.alloc(ManagedObject::String(Arc::from(s))))
        }
    }));
}
