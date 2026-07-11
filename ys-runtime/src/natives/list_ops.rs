//! List operations: map, filter, reduce, find, some, every, each, etc.
//!
//! All operations accept a closure argument and call it for each element.
//! They rely on [`Context::call_closure`] to dispatch closures.
//!
//! These are part of the prelude — no import needed.

use crate::context::{Context, NativeFn};
use crate::heap::ManagedObject;
use std::pin::Pin;
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

fn get_list(args: &[Value], name: &str, loc: Loc, ctx: &Context) -> Result<Vec<Value>, JitError> {
    let val = args.first().copied().unwrap_or(Value::from_bits(0));
    let oid = val.as_obj_id()
        .ok_or_else(|| JitError::runtime(format!("{}: expected a list", name), loc.line as usize, loc.col as usize))?;
    let objects = ctx.heap.objects.get();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::List(elems)) => Ok(elems.clone()),
        _ => Err(JitError::runtime(format!("{}: expected a list", name), loc.line as usize, loc.col as usize)),
    }
}

pub fn register(fns: &mut rustc_hash::FxHashMap<String, NativeFn>) {
    fns.insert("map".into(),         Arc::new(native_map));
    fns.insert("filter".into(),      Arc::new(native_filter));
    fns.insert("reduce".into(),      Arc::new(native_reduce));
    fns.insert("each".into(),        Arc::new(native_each));
    fns.insert("find".into(),        Arc::new(native_find));
    fns.insert("some".into(),        Arc::new(native_some));
    fns.insert("every".into(),       Arc::new(native_every));
    fns.insert("includes".into(),    Arc::new(native_includes));
    fns.insert("index_of".into(),    Arc::new(native_index_of));
    fns.insert("sorted".into(),      Arc::new(native_sorted));
    fns.insert("reversed".into(),    Arc::new(native_reversed));
    fns.insert("slice".into(),       Arc::new(native_slice));
    fns.insert("concat".into(),      Arc::new(native_concat));
    fns.insert("flatten".into(),     Arc::new(native_flatten));
    fns.insert("flat_map".into(),    Arc::new(native_flat_map));
    fns.insert("take".into(),        Arc::new(native_take));
    fns.insert("drop".into(),        Arc::new(native_drop));
    fns.insert("unique".into(),      Arc::new(native_unique));
}

fn native_map(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "map", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let mut out = Vec::with_capacity(elems.len());
        for v in elems {
            out.push(Context::call_closure(&ctx, closure, vec![v], loc).await?);
        }
        Ok(ctx.alloc(ManagedObject::List(out)))
    })
}

fn native_filter(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "filter", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let mut out = Vec::new();
        for v in elems {
            if Context::call_closure(&ctx, closure, vec![v], loc).await?.is_truthy() { out.push(v); }
        }
        Ok(ctx.alloc(ManagedObject::List(out)))
    })
}

fn native_reduce(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "reduce", loc, &ctx)?;
        let initial = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let closure = args.get(2).copied().unwrap_or(Value::from_bits(0));
        let mut acc = initial;
        for v in elems {
            acc = Context::call_closure(&ctx, closure, vec![acc, v], loc).await?;
        }
        Ok(acc)
    })
}

fn native_each(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "each", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        for v in elems { Context::call_closure(&ctx, closure, vec![v], loc).await?; }
        Ok(args.first().copied().unwrap_or(Value::from_bits(0)))
    })
}

fn native_find(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "find", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        for v in elems {
            if Context::call_closure(&ctx, closure, vec![v], loc).await?.is_truthy() { return Ok(v); }
        }
        Ok(Value::from_bits(0))
    })
}

fn native_some(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "some", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        for v in elems {
            if Context::call_closure(&ctx, closure, vec![v], loc).await?.is_truthy() { return Ok(Value::bool(true)); }
        }
        Ok(Value::bool(false))
    })
}

fn native_every(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "every", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        for v in elems {
            if !Context::call_closure(&ctx, closure, vec![v], loc).await?.is_truthy() { return Ok(Value::bool(false)); }
        }
        Ok(Value::bool(true))
    })
}

fn native_includes(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = get_list(&args, "includes", loc, &ctx);
    let target = args.get(1).copied().unwrap_or(Value::from_bits(0));
    match elems {
        Ok(e) => Box::pin(async move { Ok(Value::bool(e.iter().any(|v| v.to_bits() == target.to_bits()))) }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

fn native_index_of(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = get_list(&args, "index_of", loc, &ctx);
    let target = args.get(1).copied().unwrap_or(Value::from_bits(0));
    match elems {
        Ok(e) => Box::pin(async move { Ok(Value::number(e.iter().position(|v| v.to_bits() == target.to_bits()).map(|i| i as f64).unwrap_or(-1.0))) }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

fn native_sorted(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let mut elems = match get_list(&args, "sorted", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    for i in 1..elems.len() {
        let mut j = i;
        while j > 0 {
            let (a, b) = (elems[j-1].as_number(), elems[j].as_number());
            if let (Some(a), Some(b)) = (a, b) { if a <= b { break; } elems.swap(j-1, j); } else { break; }
            j -= 1;
        }
    }
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems))) })
}

fn native_reversed(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "reversed", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems.into_iter().rev().collect()))) })
}

fn native_slice(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "slice", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let end = args.get(2).and_then(|v| v.as_number()).map(|n| n as usize).unwrap_or(elems.len());
    let (s, e) = (start.min(elems.len()), end.min(elems.len()));
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems[s..e].to_vec()))) })
}

fn native_concat(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let mut elems = match get_list(&args, "concat", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    if let Some(other) = args.get(1).and_then(|v| v.as_obj_id()) {
        let objects = ctx.heap.objects.get();
        if let Some(ManagedObject::List(other_list)) = objects.get(other as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
            elems.extend_from_slice(other_list);
        }
    }
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems))) })
}

fn native_flatten(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "flatten", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    let mut out = Vec::new();
    for v in elems {
        if let Some(oid) = v.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(ManagedObject::List(inner)) = objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                out.extend_from_slice(inner); continue;
            }
        }
        out.push(v);
    }
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(out))) })
}

fn native_flat_map(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    Box::pin(async move {
        let elems = get_list(&args, "flat_map", loc, &ctx)?;
        let closure = args.get(1).copied().unwrap_or(Value::from_bits(0));
        let mut out = Vec::new();
        for v in elems {
            let mapped = Context::call_closure(&ctx, closure, vec![v], loc).await?;
            if let Some(oid) = mapped.as_obj_id() {
                let objects = ctx.heap.objects.get();
                if let Some(ManagedObject::List(inner)) = objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                    out.extend_from_slice(inner); continue;
                }
            }
            out.push(mapped);
        }
        Ok(ctx.alloc(ManagedObject::List(out)))
    })
}

fn native_take(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "take", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    let n = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems[..n.min(elems.len())].to_vec()))) })
}

fn native_drop(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "drop", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    let n = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(elems[n.min(elems.len())..].to_vec()))) })
}

fn native_unique(ctx: Arc<Context>, args: Vec<Value>, loc: Loc) -> Pin<Box<dyn std::future::Future<Output = Result<Value, JitError>> + Send>> {
    let elems = match get_list(&args, "unique", loc, &ctx) {
        Ok(e) => e, Err(e) => return Box::pin(async move { Err(e) }),
    };
    let mut out = Vec::with_capacity(elems.len());
    for v in elems {
        if !out.iter().any(|existing: &Value| existing.to_bits() == v.to_bits()) { out.push(v); }
    }
    Box::pin(async move { Ok(ctx.alloc(ManagedObject::List(out))) })
}
