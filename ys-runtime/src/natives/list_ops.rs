//! Higher‑order list operations: `map`, `filter`, `reduce`.
//!
//! These native functions accept a closure argument and call it for each
//! element.  They rely on [`Context::call_closure`] to dispatch closures.

use crate::context::{Context, NativeFn};
use crate::heap::ManagedObject;
use std::pin::Pin;
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

pub fn register(fns: &mut rustc_hash::FxHashMap<String, NativeFn>) {
    fns.insert("map".into(), Arc::new(native_map));
    fns.insert("filter".into(), Arc::new(native_filter));
    fns.insert("reduce".into(), Arc::new(native_reduce));
}

/// `list.map(fn)` — transform each element by calling `fn`.
///
/// `fn` receives one argument (the element) and returns the transformed value.
fn native_map(
    ctx: Arc<Context>,
    args: Vec<Value>,
    loc: Loc,
) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let list_val = args.first().copied().unwrap_or(Value::from_bits(0));
        let closure_val = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let oid = list_val
            .as_obj_id()
            .ok_or_else(|| JitError::runtime("map: expected a list", loc.line as usize, loc.col as usize))?;
        let elems = {
            let objects = ctx.heap.objects.get();
            let o = objects.get(oid as usize).and_then(|o| o.as_ref());
            match o.map(|o| &o.obj) {
                Some(ManagedObject::List(elems)) => elems.clone(),
                _ => {
                    return Err(JitError::runtime(
                        "map: expected a list",
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            }
        };
        let mut result = Vec::with_capacity(elems.len());
        for v in elems {
            let mapped = Context::call_closure(&ctx, closure_val, vec![v], loc).await?;
            result.push(mapped);
        }
        Ok(ctx.alloc(ManagedObject::List(result)))
    })
}

/// `list.filter(fn)` — keep elements where `fn` returns truthy.
///
/// `fn` receives one argument (the element) and returns a boolean (truthy).
fn native_filter(
    ctx: Arc<Context>,
    args: Vec<Value>,
    loc: Loc,
) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let list_val = args.first().copied().unwrap_or(Value::from_bits(0));
        let closure_val = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let oid = list_val
            .as_obj_id()
            .ok_or_else(|| JitError::runtime("filter: expected a list", loc.line as usize, loc.col as usize))?;
        let elems = {
            let objects = ctx.heap.objects.get();
            let o = objects.get(oid as usize).and_then(|o| o.as_ref());
            match o.map(|o| &o.obj) {
                Some(ManagedObject::List(elems)) => elems.clone(),
                _ => {
                    return Err(JitError::runtime(
                        "filter: expected a list",
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            }
        };
        let mut result = Vec::new();
        for v in elems {
            let keep = Context::call_closure(&ctx, closure_val, vec![v], loc).await?;
            if keep.is_truthy() {
                result.push(v);
            }
        }
        Ok(ctx.alloc(ManagedObject::List(result)))
    })
}

/// `list.reduce(initial, fn)` — accumulate a value across the list.
///
/// `fn` receives two arguments (accumulator, element) and returns the new accumulator.
/// If the list is empty, `initial` is returned as-is.
fn native_reduce(
    ctx: Arc<Context>,
    args: Vec<Value>,
    loc: Loc,
) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let list_val = args.first().copied().unwrap_or(Value::from_bits(0));
        let initial = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let closure_val = args.get(2).copied().unwrap_or(Value::from_bits(0));
        let oid = list_val
            .as_obj_id()
            .ok_or_else(|| JitError::runtime("reduce: expected a list", loc.line as usize, loc.col as usize))?;
        let elems = {
            let objects = ctx.heap.objects.get();
            let o = objects.get(oid as usize).and_then(|o| o.as_ref());
            match o.map(|o| &o.obj) {
                Some(ManagedObject::List(elems)) => elems.clone(),
                _ => {
                    return Err(JitError::runtime(
                        "reduce: expected a list",
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            }
        };
        let mut acc = initial;
        for v in elems {
            acc = Context::call_closure(&ctx, closure_val, vec![acc, v], loc).await?;
        }
        Ok(acc)
    })
}
