//! Number operations: to_string, ceil, floor, round, abs, sqrt,
//! pow, is_integer, to_int.
//!
//! Each function takes the receiver number as its first argument
//! (the value that was piped into it).

use crate::context::{Context, NativeFn};
use crate::heap::ManagedObject;
use crate::value_fmt::stringify_value;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// Extract the number value from `args[0]`.
fn get_number(args: &[Value], name: &str) -> Result<f64, JitError> {
    args.first().copied().unwrap_or(Value::from_bits(0)).as_number().ok_or_else(|| {
        JitError::runtime(format!("{}: expected a number as first argument", name), 0, 0)
    })
}

/// Allocate a string value, preferring SSO when it fits.
fn alloc_string(ctx: &Context, s: &str) -> Value {
    Value::sso(s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s.to_string()))))
}

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("to_string".into(), Arc::new(|ctx, args| {
        let val = args.first().copied().unwrap_or(Value::from_bits(0));
        let s = stringify_value(ctx.as_ref(), val);
        Ok(alloc_string(ctx, &s))
    }));

    fns.insert("ceil".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "ceil")?;
        Ok(Value::number(n.ceil()))
    }));

    fns.insert("floor".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "floor")?;
        Ok(Value::number(n.floor()))
    }));

    fns.insert("round".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "round")?;
        Ok(Value::number(n.round()))
    }));

    fns.insert("abs".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "abs")?;
        Ok(Value::number(n.abs()))
    }));

    fns.insert("sqrt".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "sqrt")?;
        Ok(Value::number(n.sqrt()))
    }));

    fns.insert("pow".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "pow")?;
        let exp = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
        Ok(Value::number(n.powf(exp)))
    }));

    fns.insert("is_integer".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "is_integer")?;
        Ok(Value::bool(n.fract() == 0.0))
    }));

    fns.insert("to_int".into(), Arc::new(|_ctx, args| {
        let n = get_number(args, "to_int")?;
        Ok(Value::number(n.trunc() as i64 as f64))
    }));
}
