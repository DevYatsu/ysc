//! String operations: upper, lower, trim, starts_with, ends_with,
//! contains, replace, split, repeat, slice, index_of, to_number,
//! is_empty, chars.
//!
//! Each function takes the receiver string as its first argument
//! (the value that was piped into it).

use crate::context::{Context, NativeFn};
use crate::heap::ManagedObject;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// Extract the string value from `args[0]` using `value_as_string`.
fn get_string(ctx: &Context, args: &[Value], name: &str) -> Result<String, JitError> {
    let val = args.first().copied().unwrap_or(Value::from_bits(0));
    ctx.value_as_string(val).ok_or_else(|| {
        JitError::runtime(format!("{}: expected a string as first argument", name), 0, 0)
    })
}

/// Allocate a string value, preferring SSO when it fits.
fn alloc_string(ctx: &Context, s: &str) -> Value {
    Value::sso(s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s.to_string()))))
}

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("upper".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "upper")?;
        Ok(alloc_string(ctx, &s.to_uppercase()))
    }));

    fns.insert("lower".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "lower")?;
        Ok(alloc_string(ctx, &s.to_lowercase()))
    }));

    fns.insert("trim".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "trim")?;
        Ok(alloc_string(ctx, s.trim()))
    }));

    fns.insert("starts_with".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "starts_with")?;
        let pattern = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        Ok(Value::bool(s.starts_with(&pattern)))
    }));

    fns.insert("ends_with".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "ends_with")?;
        let pattern = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        Ok(Value::bool(s.ends_with(&pattern)))
    }));

    fns.insert("contains".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "contains")?;
        let pattern = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        Ok(Value::bool(s.contains(&pattern)))
    }));

    fns.insert("replace".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "replace")?;
        let from = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        let to = args.get(2).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        Ok(alloc_string(ctx, &s.replace(&from, &to)))
    }));

    fns.insert("split".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "split")?;
        let delim = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        let parts: Vec<Value> = if delim.is_empty() {
            s.chars().map(|c| alloc_string(ctx, &c.to_string())).collect()
        } else {
            s.split(&delim).map(|part| alloc_string(ctx, part)).collect()
        };
        Ok(ctx.alloc(ManagedObject::List(parts)))
    }));

    fns.insert("repeat".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "repeat")?;
        let n = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        Ok(alloc_string(ctx, &s.repeat(n)))
    }));

    fns.insert("slice".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "slice")?;
        let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let end = args.get(2).and_then(|v| v.as_number()).map(|n| n as usize).unwrap_or(s.len());
        let (start, end) = (start.min(s.len()), end.min(s.len()));
        Ok(alloc_string(ctx, &s[start..end]))
    }));

    fns.insert("index_of".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "index_of")?;
        let pattern = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        Ok(Value::number(s.find(&pattern).map(|i| i as f64).unwrap_or(-1.0)))
    }));

    fns.insert("to_number".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "to_number")?;
        Ok(Value::number(s.parse::<f64>().unwrap_or(0.0)))
    }));

    fns.insert("is_empty".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "is_empty")?;
        Ok(Value::bool(s.is_empty()))
    }));

    fns.insert("chars".into(), Arc::new(|ctx, args| {
        let s = get_string(ctx, args, "chars")?;
        let chars: Vec<Value> = s.chars().map(|c| alloc_string(ctx, &c.to_string())).collect();
        Ok(ctx.alloc(ManagedObject::List(chars)))
    }));
}
