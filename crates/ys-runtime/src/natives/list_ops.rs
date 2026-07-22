//! List operations: map, filter, reduce, find, some, every, each, etc.
//!
//! All operations accept a closure argument and call it for each element.
//! They rely on [`Context::call_closure`] to dispatch closures.
//!
//! These are part of the prelude — no import needed.

#[cfg(feature = "parallel")]
use rayon::prelude::*;
#[cfg(feature = "parallel")]
const PARALLEL_THRESHOLD: usize = 10000;

use crate::context::NativeCtx;
use crate::heap::ManagedObject;
use crate::natives::NativeRegistry;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

fn get_list(args: &[Value], name: &str, ctx: &NativeCtx) -> Result<Vec<Value>, JitError> {
    let val = args.first().copied().unwrap_or(Value::nil());
    let oid = val
        .as_obj_id()
        .ok_or_else(|| JitError::runtime(format!("{}: expected a list", name), (0, 0)))?;
    let objects = ctx.heap_objects();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::List(elems)) => Ok(elems.clone()),
        _ => Err(JitError::runtime(
            format!("{}: expected a list", name),
            (0, 0),
        )),
    }
}

/// Borrow list elements from the heap — no clone. For read-only operations.
fn get_list_ref<'a>(
    args: &'a [Value],
    name: &str,
    ctx: &'a NativeCtx,
) -> Result<&'a [Value], JitError> {
    let val = args.first().copied().unwrap_or(Value::nil());
    let oid = val
        .as_obj_id()
        .ok_or_else(|| JitError::runtime(format!("{}: expected a list", name), (0, 0)))?;
    let objects = ctx.heap_objects();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::List(elems)) => Ok(elems.as_slice()),
        _ => Err(JitError::runtime(
            format!("{}: expected a list", name),
            (0, 0),
        )),
    }
}

pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("map", native_map);
    reg.insert("filter", native_filter);
    reg.insert("reduce", native_reduce);
    reg.insert("each", native_each);
    reg.insert("find", native_find);
    reg.insert("some", native_some);
    reg.insert("every", native_every);
    reg.insert("includes", native_includes);
    reg.insert("index_of", native_index_of);
    reg.insert("sorted", native_sorted);
    reg.insert("reversed", native_reversed);
    reg.insert("slice", native_slice);
    reg.insert("concat", native_concat);
    reg.insert("flatten", native_flatten);
    reg.insert("flat_map", native_flat_map);
    reg.insert("take", native_take);
    reg.insert("drop", native_drop);
    reg.insert("step", native_step);
    reg.insert("unique", native_unique);
    reg.insert("len", native_len);
    reg.insert("push", native_push);
}

fn native_step(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let range_val = args.first().copied().unwrap_or(Value::nil());
    let step_val = args.get(1).copied().unwrap_or(Value::nil());
    let oid = range_val
        .as_obj_id()
        .ok_or_else(|| JitError::runtime("step: expected a range as first argument", (0, 0)))?;
    let step_num = step_val
        .as_number()
        .ok_or_else(|| JitError::runtime("step: step must be a number", (0, 0)))?;
    let objects = ctx.heap_objects();
    let guard = objects.get(oid as usize).and_then(|o| o.as_ref());
    match guard.map(|o| &o.obj) {
        Some(ManagedObject::Range {
            start,
            end,
            step: _old_step,
        }) => Ok(ctx.alloc(ManagedObject::Range {
            start: *start,
            end: *end,
            step: step_num,
        })),
        _ => Err(JitError::runtime("step: expected a range", (0, 0))),
    }
}

fn native_map(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "map", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    let mut out = Vec::with_capacity(elems.len());
    for v in elems {
        out.push(ctx.call_closure(
            closure,
            &[v],
            Loc { line: 0, col: 0 },
        )?);
    }
    Ok(ctx.alloc(ManagedObject::List(out)))
}

fn native_filter(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "filter", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    let mut out = Vec::new();
    for v in elems {
        if ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?.is_truthy() {
            out.push(v);
        }
    }
    Ok(ctx.alloc(ManagedObject::List(out)))
}

fn native_reduce(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "reduce", ctx)?;
    let initial = args.get(1).copied().unwrap_or(Value::nil());
    let closure = args.get(2).copied().unwrap_or(Value::nil());
    let mut acc = initial;
    for v in elems {
        acc = ctx.call_closure( closure, &[acc, v], Loc { line: 0, col: 0 })?;
    }
    Ok(acc)
}

fn native_each(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "each", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    for v in elems {
        ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?;
    }
    Ok(args.first().copied().unwrap_or(Value::nil()))
}

fn native_find(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "find", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    for v in elems {
        if ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?.is_truthy() {
            return Ok(v);
        }
    }
    Ok(Value::nil())
}

fn native_some(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "some", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    for v in elems {
        if ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?.is_truthy() {
            return Ok(Value::bool(true));
        }
    }
    Ok(Value::bool(false))
}

fn native_every(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "every", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    for v in elems {
        if !ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?.is_truthy() {
            return Ok(Value::bool(false));
        }
    }
    Ok(Value::bool(true))
}

fn native_includes(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "includes", ctx)?;
    let target = args.get(1).copied().unwrap_or(Value::nil());
    #[cfg(feature = "parallel")]
    let found = if elems.len() > PARALLEL_THRESHOLD {
        elems.par_iter().any(|v| v.to_bits() == target.to_bits())
    } else {
        elems.iter().any(|v| v.to_bits() == target.to_bits())
    };
    #[cfg(not(feature = "parallel"))]
    let found = elems.iter().any(|v| v.to_bits() == target.to_bits());
    Ok(Value::bool(found))
}

fn native_index_of(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "index_of", ctx)?;
    let target = args.get(1).copied().unwrap_or(Value::nil());
    Ok(Value::number(
        elems
            .iter()
            .position(|v| v.to_bits() == target.to_bits())
            .map(|i| i as f64)
            .unwrap_or(-1.0),
    ))
}

fn native_sorted(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let mut elems = get_list(args, "sorted", ctx)?;
    #[cfg(feature = "parallel")]
    if elems.len() > PARALLEL_THRESHOLD {
        elems.par_sort_unstable_by(|a, b| match (a.as_number(), b.as_number()) {
            (Some(an), Some(bn)) => an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        });
    } else {
        insertion_sort_by_key(&mut elems);
    }
    #[cfg(not(feature = "parallel"))]
    insertion_sort_by_key(&mut elems);
    Ok(ctx.alloc(ManagedObject::List(elems)))
}

/// Simple insertion sort fallback (used when rayon isn't available).
fn insertion_sort_by_key(elems: &mut [Value]) {
    for i in 1..elems.len() {
        let mut j = i;
        while j > 0 {
            let (a, b) = (elems[j - 1].as_number(), elems[j].as_number());
            if let (Some(a), Some(b)) = (a, b) {
                if a <= b {
                    break;
                }
                elems.swap(j - 1, j);
            } else {
                break;
            }
            j -= 1;
        }
    }
}

fn native_reversed(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "reversed", ctx)?;
    Ok(ctx.alloc(ManagedObject::List(elems.iter().rev().copied().collect())))
}

fn native_slice(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "slice", ctx)?;
    let start = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n.max(0.0) as usize)
        .unwrap_or(0);
    let end = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .unwrap_or(elems.len());
    let (s, e) = (start.min(elems.len()), end.min(elems.len()));
    Ok(ctx.alloc(ManagedObject::List(elems[s..e].to_vec())))
}

fn native_concat(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let mut elems = get_list(args, "concat", ctx)?;
    if let Some(other) = args.get(1).and_then(|v| v.as_obj_id()) {
        let objects = ctx.heap_objects();
        if let Some(ManagedObject::List(other_list)) = objects
            .get(other as usize)
            .and_then(|o| o.as_ref())
            .map(|o| &o.obj)
        {
            elems.extend_from_slice(other_list);
        }
    }
    Ok(ctx.alloc(ManagedObject::List(elems)))
}

fn native_flatten(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "flatten", ctx)?;
    let mut out = Vec::new();
    for v in elems {
        if let Some(oid) = v.as_obj_id() {
            let objects = ctx.heap_objects();
            if let Some(ManagedObject::List(inner)) = objects
                .get(oid as usize)
                .and_then(|o| o.as_ref())
                .map(|o| &o.obj)
            {
                out.extend_from_slice(inner);
                continue;
            }
        }
        out.push(v);
    }
    Ok(ctx.alloc(ManagedObject::List(out)))
}

fn native_flat_map(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list(args, "flat_map", ctx)?;
    let closure = args.get(1).copied().unwrap_or(Value::nil());
    let mut out = Vec::new();
    for v in elems {
        let mapped = ctx.call_closure( closure, &[v], Loc { line: 0, col: 0 })?;
        if let Some(oid) = mapped.as_obj_id() {
            let objects = ctx.heap_objects();
            if let Some(ManagedObject::List(inner)) = objects
                .get(oid as usize)
                .and_then(|o| o.as_ref())
                .map(|o| &o.obj)
            {
                out.extend_from_slice(inner);
                continue;
            }
        }
        out.push(mapped);
    }
    Ok(ctx.alloc(ManagedObject::List(out)))
}

fn native_take(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "take", ctx)?;
    let n = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n.max(0.0) as usize)
        .unwrap_or(0);
    Ok(ctx.alloc(ManagedObject::List(elems[..n.min(elems.len())].to_vec())))
}

fn native_drop(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "drop", ctx)?;
    let n = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n.max(0.0) as usize)
        .unwrap_or(0);
    Ok(ctx.alloc(ManagedObject::List(elems[n.min(elems.len())..].to_vec())))
}

fn native_len(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let val = args.first().copied().unwrap_or(Value::nil());
    let oid = val
        .as_obj_id()
        .ok_or_else(|| JitError::runtime("len: expected a list or object", (0, 0)))?;
    let objects = ctx.heap_objects();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::List(elems)) => Ok(Value::number(elems.len() as f64)),
        Some(ManagedObject::Object(d)) => Ok(Value::number(d.map.len() as f64)),
        _ => Err(JitError::runtime("len: expected a list or object", (0, 0))),
    }
}

fn native_push(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let list_val = args.first().copied().unwrap_or(Value::nil());
    let elem = args.get(1).copied().unwrap_or(Value::nil());
    let oid = list_val
        .as_obj_id()
        .ok_or_else(|| JitError::runtime("push: expected a list", (0, 0)))?;
    let objects = ctx.as_inner().heap.objects.get_mut();
    if let Some(Some(obj)) = objects.get_mut(oid as usize) {
        if let ManagedObject::List(elems) = &mut obj.obj {
            elems.push(elem);
            return Ok(list_val);
        }
    }
    Err(JitError::runtime("push: expected a list", (0, 0)))
}

fn native_unique(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let elems = get_list_ref(args, "unique", ctx)?;
    let mut out = Vec::with_capacity(elems.len());
    for &v in elems {
        if !out
            .iter()
            .any(|existing: &Value| existing.to_bits() == v.to_bits())
        {
            out.push(v);
        }
    }
    Ok(ctx.alloc(ManagedObject::List(out)))
}
