//! Method dispatch for BoundMethod calls — list, string, number, object, and
//! generator `.next()`.
//!
//! Extracted from `vm/mod.rs` as part of architecture refinement.  Each
//! `dispatch_*_method` function handles all methods for its type.  The main
//! entry point [`dispatch_bound_method`] routes by receiver type.

use crate::context::Context;
use crate::heap::ManagedObject;
use crate::natives::alloc_string;
use crate::value_fmt::stringify_value;
use crate::vm::{PromiseState, execute_bytecode};
use std::borrow::Cow;
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

// ─────────────────────────────────────────────────────────────────────────────
//  Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Dispatch a [`BoundMethod`](ManagedObject::BoundMethod) call.
///
/// Returns `Ok(Some(val))` if a known built-in method was found and executed.
/// Returns `Ok(None)` if the method name did not match any built-in method
/// (the caller should fall through to normal function lookup or user-defined
/// method dispatch).
/// Returns `Err(JitError)` on runtime errors.
pub fn dispatch_bound_method(
    ctx: &Context,
    receiver: Value,
    name_id: u32,
    args_regs: &[usize],
    regs: &[Value],
    loc: Loc,
) -> Result<Option<Value>, JitError> {
    let method = ctx
        .string_pool
        .get(name_id as usize)
        .map(|s| s.as_ref())
        .unwrap_or("");

    // ── List method dispatch ──────────────────────────────────────────────
    // Methods that call closures need ownership of the list to release the heap
    // borrow.  Read-only methods borrow `&[Value]` from the heap directly.
    if let Some(list_oid) = receiver.as_obj_id() {
        let is_list = {
            let objects = ctx.heap.objects.get();
            objects
                .get(list_oid as usize)
                .and_then(|o| o.as_ref())
                .is_some_and(|o| matches!(o.obj, ManagedObject::List(_)))
        };

        if is_list {
            // Read-only methods — borrow from heap, no clone.
            if matches!(
                method,
                "includes"
                    | "index_of"
                    | "reversed"
                    | "sorted"
                    | "slice"
                    | "take"
                    | "drop"
                    | "unique"
                    | "concat"
                    | "flatten"
            ) {
                let objects = ctx.heap.objects.get();
                if let Some(Some(obj)) = objects.get(list_oid as usize)
                    && let ManagedObject::List(elems) = &obj.obj
                {
                    return dispatch_list_method(
                        ctx,
                        method,
                        elems.as_slice(),
                        args_regs,
                        regs,
                        loc,
                        receiver,
                    )
                    .map(Some);
                }
            } else {
                // Closure-using methods — must clone to release heap borrow.
                let elems = {
                    let objects = ctx.heap.objects.get();
                    objects
                        .get(list_oid as usize)
                        .and_then(|o| o.as_ref())
                        .and_then(|o| {
                            if let ManagedObject::List(elems) = &o.obj {
                                Some(elems.clone())
                            } else {
                                None
                            }
                        })
                };
                if let Some(elems) = elems {
                    return dispatch_list_method(
                        ctx, method, &elems, args_regs, regs, loc, receiver,
                    )
                    .map(Some);
                }
            }
        }
    }

    // ── Generator .next() dispatch ────────────────────────────────────────
    if method == "next"
        && let Some(gen_oid) = receiver.as_obj_id()
    {
        let is_gen = {
            let objects = ctx.heap.objects.get();
            objects
                .get(gen_oid as usize)
                .and_then(|o| o.as_ref())
                .is_some_and(|o| matches!(o.obj, ManagedObject::Promise(_)))
        };
        if is_gen {
            return dispatch_generator_next(ctx, gen_oid).map(Some);
        }
    }

    // ── String method dispatch ────────────────────────────────────────────
    if let Some(s) = ctx.value_as_string(receiver) {
        return dispatch_string_method(ctx, method, s, args_regs, regs, loc).map(Some);
    }

    // ── Number method dispatch ────────────────────────────────────────────
    if let Some(n) = receiver.as_number() {
        return dispatch_number_method(ctx, method, n, args_regs, regs, loc).map(Some);
    }

    // ── Object method dispatch ────────────────────────────────────────────
    if let Some(obj_oid) = receiver.as_obj_id() {
        let is_object = {
            let objects = ctx.heap.objects.get();
            objects
                .get(obj_oid as usize)
                .and_then(|o| o.as_ref())
                .is_some_and(|o| matches!(o.obj, ManagedObject::Object(_)))
        };
        if is_object {
            return dispatch_object_method(ctx, method, obj_oid, args_regs, regs, loc).map(Some);
        }
    }

    // ── Not a built-in method ─────────────────────────────────────────────
    Ok(None)
}

// ─────────────────────────────────────────────────────────────────────────────
//  List methods (18)
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_list_method(
    ctx: &Context,
    method: &str,
    elems: &[Value],
    args_regs: &[usize],
    regs: &[Value],
    loc: Loc,
    _receiver: Value,
) -> Result<Value, JitError> {
    let read = |i: usize| args_regs.get(i).map(|&r| regs[r]).unwrap_or(Value::nil());

    match method {
        "map" => {
            let mut out = Vec::with_capacity(elems.len());
            for &v in elems {
                out.push(Context::call_closure(ctx, read(0), &[v], loc)?);
            }
            Ok(ctx.alloc(ManagedObject::List(out)))
        }
        "filter" => {
            let mut out = Vec::new();
            for &v in elems {
                if Context::call_closure(ctx, read(0), &[v], loc)?.is_truthy() {
                    out.push(v);
                }
            }
            Ok(ctx.alloc(ManagedObject::List(out)))
        }
        "reduce" if args_regs.len() >= 2 => {
            let init = read(0);
            let cl = read(1);
            let mut acc = init;
            for &v in elems {
                acc = Context::call_closure(ctx, cl, &[acc, v], loc)?;
            }
            Ok(acc)
        }
        "each" => {
            for &v in elems {
                Context::call_closure(ctx, read(0), &[v], loc)?;
            }
            Ok(_receiver)
        }
        "find" => {
            let mut found = Value::nil();
            for &v in elems {
                if Context::call_closure(ctx, read(0), &[v], loc)?.is_truthy() {
                    found = v;
                    break;
                }
            }
            Ok(found)
        }
        "some" => {
            let mut r = Value::bool(false);
            for &v in elems {
                if Context::call_closure(ctx, read(0), &[v], loc)?.is_truthy() {
                    r = Value::bool(true);
                    break;
                }
            }
            Ok(r)
        }
        "every" => {
            let mut r = Value::bool(true);
            for &v in elems {
                if !Context::call_closure(ctx, read(0), &[v], loc)?.is_truthy() {
                    r = Value::bool(false);
                    break;
                }
            }
            Ok(r)
        }
        "flat_map" => {
            let mut out = Vec::new();
            for &v in elems {
                let mapped = Context::call_closure(ctx, read(0), &[v], loc)?;
                if let Some(oid) = mapped.as_obj_id() {
                    let objects = ctx.heap.objects.get();
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
        "includes" => Ok(Value::bool({
            #[cfg(feature = "parallel")]
            if elems.len() > 10000 {
                elems.par_iter().any(|v| v.to_bits() == read(0).to_bits())
            } else {
                elems.iter().any(|v| v.to_bits() == read(0).to_bits())
            }
            #[cfg(not(feature = "parallel"))]
            elems.iter().any(|v| v.to_bits() == read(0).to_bits())
        })),
        "index_of" => Ok(Value::number(
            elems
                .iter()
                .position(|v| v.to_bits() == read(0).to_bits())
                .map(|i| i as f64)
                .unwrap_or(-1.0),
        )),
        "sorted" => {
            let mut e = elems.to_vec();
            #[cfg(feature = "parallel")]
            if e.len() > 10000 {
                e.par_sort_unstable_by(|a, b| match (a.as_number(), b.as_number()) {
                    (Some(an), Some(bn)) => {
                        an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal)
                    }
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                crate::vm::sort_insertion(&mut e);
            }
            #[cfg(not(feature = "parallel"))]
            crate::vm::sort_insertion(&mut e);
            Ok(ctx.alloc(ManagedObject::List(e)))
        }
        "reversed" => Ok(ctx.alloc(ManagedObject::List(elems.iter().rev().copied().collect()))),
        "slice" => {
            let s = read(0)
                .as_number()
                .map(|n| n.max(0.0) as usize)
                .unwrap_or(0);
            let e = read(1)
                .as_number()
                .map(|n| n as usize)
                .unwrap_or(elems.len());
            let (s, e) = (s.min(elems.len()), e.min(elems.len()));
            Ok(ctx.alloc(ManagedObject::List(elems[s..e].to_vec())))
        }
        "concat" => {
            let mut e: Vec<Value> = elems.to_vec();
            if let Some(oid) = read(0).as_obj_id() {
                let objects = ctx.heap.objects.get();
                if let Some(ManagedObject::List(other)) = objects
                    .get(oid as usize)
                    .and_then(|o| o.as_ref())
                    .map(|o| &o.obj)
                {
                    e.extend_from_slice(other);
                }
            }
            Ok(ctx.alloc(ManagedObject::List(e)))
        }
        "flatten" => {
            let mut out = Vec::new();
            for &v in elems {
                if let Some(oid) = v.as_obj_id() {
                    let objects = ctx.heap.objects.get();
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
        "take" => {
            let n = read(0)
                .as_number()
                .map(|n| n.max(0.0) as usize)
                .unwrap_or(0);
            Ok(ctx.alloc(ManagedObject::List(elems[..n.min(elems.len())].to_vec())))
        }
        "drop" => {
            let n = read(0)
                .as_number()
                .map(|n| n.max(0.0) as usize)
                .unwrap_or(0);
            Ok(ctx.alloc(ManagedObject::List(elems[n.min(elems.len())..].to_vec())))
        }
        "unique" => {
            let mut out = Vec::with_capacity(elems.len());
            for &v in elems {
                if !out.iter().any(|x: &Value| x.to_bits() == v.to_bits()) {
                    out.push(v);
                }
            }
            Ok(ctx.alloc(ManagedObject::List(out)))
        }
        _ => Err(JitError::runtime(
            format!("Unknown list method '{}'", method),
            loc.as_error_pos(),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  String methods (12)
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_string_method(
    ctx: &Context,
    method: &str,
    s: Cow<'_, str>,
    args_regs: &[usize],
    regs: &[Value],
    loc: Loc,
) -> Result<Value, JitError> {
    let read_str = |i: usize| {
        args_regs
            .get(i)
            .map(|&r| regs[r])
            .and_then(|v| ctx.value_as_string(v))
    };
    let read_num = |i: usize| {
        args_regs
            .get(i)
            .map(|&r| regs[r])
            .and_then(|v| v.as_number())
    };

    match method {
        "len" => Ok(Value::number(s.len() as f64)),
        "upper" => {
            let s = s.to_uppercase();
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "lower" => {
            let s = s.to_lowercase();
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "trim" => {
            let s = s.trim().to_string();
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "starts_with" if !args_regs.is_empty() => Ok(Value::bool(
            read_str(0).is_some_and(|p| s.starts_with(p.as_ref())),
        )),
        "ends_with" if !args_regs.is_empty() => Ok(Value::bool(
            read_str(0).is_some_and(|p| s.ends_with(p.as_ref())),
        )),
        "contains" if !args_regs.is_empty() => Ok(Value::bool(
            read_str(0).is_some_and(|p| s.contains(p.as_ref())),
        )),
        "replace" if args_regs.len() >= 2 => {
            let from = read_str(0).unwrap_or_default();
            let to = read_str(1).unwrap_or_default();
            let s = s.replace(from.as_ref(), to.as_ref());
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "split" if !args_regs.is_empty() => {
            let delim = read_str(0).unwrap_or_default();
            let parts: Vec<Value> = if delim.is_empty() {
                s.chars()
                    .map(|c| {
                        let mut buf = [0u8; 4];
                        let encoded = c.encode_utf8(&mut buf);
                        alloc_string(ctx, encoded)
                    })
                    .collect()
            } else {
                s.split(delim.as_ref())
                    .map(|part| alloc_string(ctx, part))
                    .collect()
            };
            Ok(ctx.alloc(ManagedObject::List(parts)))
        }
        "repeat" if !args_regs.is_empty() => {
            let n = read_num(0).map(|n| n.max(0.0) as usize).unwrap_or(0);
            let s = s.repeat(n);
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "slice" => {
            let start = read_num(0).map(|n| n.max(0.0) as usize).unwrap_or(0);
            let end = read_num(1).map(|n| n as usize).unwrap_or(s.len());
            let (start, end) = (start.min(s.len()), end.min(s.len()));
            Ok(alloc_string(ctx, &s[start..end]))
        }
        "index_of" if !args_regs.is_empty() => Ok(Value::number(read_str(0).map_or(-1.0, |p| {
            s.find(p.as_ref()).map(|i| i as f64).unwrap_or(-1.0)
        }))),
        "to_number" => Ok(Value::number(s.parse::<f64>().unwrap_or(0.0))),
        "is_empty" => Ok(Value::bool(s.is_empty())),
        "chars" => {
            let chars: Vec<Value> = s
                .chars()
                .map(|c| {
                    let mut buf = [0u8; 4];
                    let encoded = c.encode_utf8(&mut buf);
                    alloc_string(ctx, encoded)
                })
                .collect();
            Ok(ctx.alloc(ManagedObject::List(chars)))
        }
        _ => Err(JitError::runtime(
            format!("Unknown string method '{}'", method),
            loc.as_error_pos(),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Number methods (9)
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_number_method(
    ctx: &Context,
    method: &str,
    n: f64,
    args_regs: &[usize],
    regs: &[Value],
    loc: Loc,
) -> Result<Value, JitError> {
    let read_num = |i: usize| {
        args_regs
            .get(i)
            .map(|&r| regs[r])
            .and_then(|v| v.as_number())
    };

    match method {
        "to_string" => {
            let s = stringify_value(ctx, Value::number(n));
            Ok(Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s)))))
        }
        "ceil" => Ok(Value::number(n.ceil())),
        "floor" => Ok(Value::number(n.floor())),
        "round" => Ok(Value::number(n.round())),
        "abs" => Ok(Value::number(n.abs())),
        "sqrt" => Ok(Value::number(n.sqrt())),
        "pow" if !args_regs.is_empty() => Ok(Value::number(n.powf(read_num(0).unwrap_or(0.0)))),
        "is_integer" => Ok(Value::bool(n.fract() == 0.0)),
        "to_int" => Ok(Value::number(n.trunc() as i64 as f64)),
        _ => Err(JitError::runtime(
            format!("Unknown number method '{}'", method),
            loc.as_error_pos(),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Object methods (6)
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_object_method(
    ctx: &Context,
    method: &str,
    obj_oid: u32,
    args_regs: &[usize],
    regs: &[Value],
    loc: Loc,
) -> Result<Value, JitError> {
    let read_str = |i: usize| {
        args_regs
            .get(i)
            .map(|&r| regs[r])
            .and_then(|v| ctx.value_as_string(v))
    };

    match method {
        "keys" | "values" | "entries" => {
            let (keys, vals): (Vec<_>, Vec<_>) = {
                let objects = ctx.heap.objects.get();
                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                    if let ManagedObject::Object(ref d) = obj.obj {
                        let mut ks: Vec<Value> = Vec::with_capacity(d.map.len());
                        let mut vs: Vec<Value> = Vec::with_capacity(d.map.len());
                        for (&name_id, v) in d.map.iter() {
                            let name = ctx
                                .string_pool
                                .get(name_id as usize)
                                .map(|s| s.as_ref())
                                .unwrap_or("?");
                            ks.push(Value::sso(name).unwrap_or_else(|| Value::pool(name_id)));
                            vs.push(*v);
                        }
                        (ks, vs)
                    } else {
                        (Vec::new(), Vec::new())
                    }
                } else {
                    (Vec::new(), Vec::new())
                }
            };
            match method {
                "keys" => Ok(ctx.alloc(ManagedObject::List(keys))),
                "values" => Ok(ctx.alloc(ManagedObject::List(vals))),
                _ => {
                    // entries
                    let entries: Vec<Value> = keys
                        .into_iter()
                        .zip(vals)
                        .map(|(k, v)| ctx.alloc(ManagedObject::List(vec![k, v])))
                        .collect();
                    Ok(ctx.alloc(ManagedObject::List(entries)))
                }
            }
        }
        "has" if !args_regs.is_empty() => {
            let key = read_str(0).unwrap_or_default();
            let found = {
                let objects = ctx.heap.objects.get();
                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                    if let ManagedObject::Object(ref d) = obj.obj {
                        ctx.pool_id(&key).is_some_and(|id| d.map.contains_key(&id))
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            Ok(Value::bool(found))
        }
        "len" => {
            let l = {
                let objects = ctx.heap.objects.get();
                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                    if let ManagedObject::Object(ref d) = obj.obj {
                        d.map.len() as f64
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            };
            Ok(Value::number(l))
        }
        _ => Err(JitError::runtime(
            format!("Unknown object method '{}'", method),
            loc.as_error_pos(),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Generator .next() dispatch
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_generator_next(ctx: &Context, gen_oid: u32) -> Result<Value, JitError> {
    // Extract and resume the generator's continuation
    let gen_state = {
        let objs = ctx.heap.objects.get_mut();
        if let Some(Some(slot)) = objs.get_mut(gen_oid as usize) {
            match &mut slot.obj {
                ManagedObject::Promise(ps) => {
                    match std::mem::replace(ps, PromiseState::Resolved(Value::nil())) {
                        PromiseState::Pending { continuation } => (continuation, false),
                        PromiseState::Resolved(_v) => (None, true),
                        PromiseState::Rejected(_) => (None, true),
                        PromiseState::Compound { .. } => (None, true),
                    }
                }
                _ => (None, true),
            }
        } else {
            (None, true)
        }
    };
    let (cont, done) = gen_state;

    if done {
        // Generator exhausted
        return Ok(Value::nil());
    }

    if let Some(frame) = cont {
        // Resume the generator — it will run until next yield
        let result = execute_bytecode(&frame.instructions, ctx, frame.registers, frame.pc)?;
        Ok(result)
    } else {
        Ok(Value::nil())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::heap::ObjectData;
    use std::borrow::Cow;

    /// A default `Loc` value for test calls.
    const TEST_LOC: Loc = Loc { line: 0, col: 0 };

    /// Empty argument / register arrays for methods that take no arguments.
    const NO_ARGS: &[usize] = &[];
    const NO_REGS: &[Value] = &[];

    fn test_ctx() -> Arc<Context> {
        Arc::new(Context::new())
    }

    // ── Number method tests ────────────────────────────────────────────────

    #[test]
    fn test_number_ceil() {
        let ctx = test_ctx();
        let result = dispatch_number_method(&ctx, "ceil", 3.4, NO_ARGS, NO_REGS, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(4.0));
    }

    #[test]
    fn test_number_floor() {
        let ctx = test_ctx();
        let result =
            dispatch_number_method(&ctx, "floor", 3.9, NO_ARGS, NO_REGS, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(3.0));
    }

    #[test]
    fn test_number_round() {
        let ctx = test_ctx();
        assert_eq!(
            dispatch_number_method(&ctx, "round", 3.5, NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_number(),
            Some(4.0)
        );
        assert_eq!(
            dispatch_number_method(&ctx, "round", 3.4, NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_number(),
            Some(3.0)
        );
    }

    #[test]
    fn test_number_abs() {
        let ctx = test_ctx();
        let result = dispatch_number_method(&ctx, "abs", -7.5, NO_ARGS, NO_REGS, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(7.5));
    }

    #[test]
    fn test_number_sqrt() {
        let ctx = test_ctx();
        let result = dispatch_number_method(&ctx, "sqrt", 9.0, NO_ARGS, NO_REGS, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(3.0));
    }

    #[test]
    fn test_number_pow() {
        let ctx = test_ctx();
        let args = [Value::number(3.0)];
        let regs = [Value::number(3.0)];
        let result = dispatch_number_method(&ctx, "pow", 2.0, &[0], &regs, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(8.0));
    }

    #[test]
    fn test_number_is_integer() {
        let ctx = test_ctx();
        assert!(
            dispatch_number_method(&ctx, "is_integer", 5.0, NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert!(
            !dispatch_number_method(&ctx, "is_integer", 5.5, NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_bool()
                .unwrap()
        );
    }

    #[test]
    fn test_number_to_int() {
        let ctx = test_ctx();
        let result =
            dispatch_number_method(&ctx, "to_int", 7.9, NO_ARGS, NO_REGS, TEST_LOC).unwrap();
        assert_eq!(result.as_number(), Some(7.0));
    }

    // ── String method tests ────────────────────────────────────────────────

    #[test]
    fn test_string_len() {
        let ctx = test_ctx();
        let result =
            dispatch_string_method(&ctx, "len", "hello".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap();
        assert_eq!(result.as_number(), Some(5.0));
    }

    #[test]
    fn test_string_upper_lower() {
        let ctx = test_ctx();
        let upper =
            dispatch_string_method(&ctx, "upper", "hello".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap();
        assert_eq!(ctx.value_as_string(upper), Some(Cow::from("HELLO")));
        let lower =
            dispatch_string_method(&ctx, "lower", "HELLO".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap();
        assert_eq!(ctx.value_as_string(lower), Some(Cow::from("hello")));
    }

    #[test]
    fn test_string_trim() {
        let ctx = test_ctx();
        let result =
            dispatch_string_method(&ctx, "trim", "  hi  ".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap();
        assert_eq!(ctx.value_as_string(result), Some(Cow::from("hi")));
    }

    #[test]
    fn test_string_starts_with() {
        let ctx = test_ctx();
        let args = [Value::sso("hel").unwrap()];
        let regs = [Value::sso("hel").unwrap()];
        let result =
            dispatch_string_method(&ctx, "starts_with", "hello".into(), &[0], &regs, TEST_LOC)
                .unwrap();
        assert!(result.as_bool().unwrap());
        let result2 = dispatch_string_method(
            &ctx,
            "starts_with",
            "hello".into(),
            &[0],
            &[Value::sso("x").unwrap()],
            TEST_LOC,
        )
        .unwrap();
        assert!(!result2.as_bool().unwrap());
    }

    #[test]
    fn test_string_ends_with() {
        let ctx = test_ctx();
        let result = dispatch_string_method(
            &ctx,
            "ends_with",
            "hello".into(),
            &[0],
            &[Value::sso("lo").unwrap()],
            TEST_LOC,
        )
        .unwrap();
        assert!(result.as_bool().unwrap());
    }

    #[test]
    fn test_string_contains() {
        let ctx = test_ctx();
        let result = dispatch_string_method(
            &ctx,
            "contains",
            "hello world".into(),
            &[0],
            &[Value::sso("world").unwrap()],
            TEST_LOC,
        )
        .unwrap();
        assert!(result.as_bool().unwrap());
    }

    #[test]
    fn test_string_split() {
        let ctx = test_ctx();
        let result = dispatch_string_method(
            &ctx,
            "split",
            "a,b,c".into(),
            &[0],
            &[Value::sso(",").unwrap()],
            TEST_LOC,
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 3);
                    assert_eq!(ctx.value_as_string(elems[0]), Some(Cow::from("a")));
                    assert_eq!(ctx.value_as_string(elems[1]), Some(Cow::from("b")));
                    assert_eq!(ctx.value_as_string(elems[2]), Some(Cow::from("c")));
                    return;
                }
            }
        }
        panic!("split did not return a list");
    }

    #[test]
    fn test_string_to_number() {
        let ctx = test_ctx();
        let result =
            dispatch_string_method(&ctx, "to_number", "42".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap();
        assert_eq!(result.as_number(), Some(42.0));
    }

    #[test]
    fn test_string_is_empty() {
        let ctx = test_ctx();
        assert!(
            dispatch_string_method(&ctx, "is_empty", "".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert!(
            !dispatch_string_method(&ctx, "is_empty", "x".into(), NO_ARGS, NO_REGS, TEST_LOC)
                .unwrap()
                .as_bool()
                .unwrap()
        );
    }

    // ── List method tests ──────────────────────────────────────────────────

    fn make_list(ctx: &Context, vals: Vec<Value>) -> Value {
        ctx.alloc(ManagedObject::List(vals))
    }

    #[test]
    fn test_list_includes() {
        let ctx = test_ctx();
        let elems = vec![Value::number(1.0), Value::number(2.0), Value::number(3.0)];
        let regs = [Value::number(2.0)];
        let result = dispatch_list_method(
            &ctx,
            "includes",
            &elems,
            &[0],
            &regs,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        assert!(result.as_bool().unwrap());

        let elems = vec![Value::number(1.0), Value::number(2.0), Value::number(3.0)];
        let regs = [Value::number(99.0)];
        let result = dispatch_list_method(
            &ctx,
            "includes",
            &elems,
            &[0],
            &regs,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        assert!(!result.as_bool().unwrap());
    }

    #[test]
    fn test_list_reversed() {
        let ctx = test_ctx();
        let elems = vec![Value::number(1.0), Value::number(2.0), Value::number(3.0)];
        let result = dispatch_list_method(
            &ctx,
            "reversed",
            &elems,
            NO_ARGS,
            NO_REGS,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 3);
                    assert_eq!(elems[0].as_number(), Some(3.0));
                    assert_eq!(elems[1].as_number(), Some(2.0));
                    assert_eq!(elems[2].as_number(), Some(1.0));
                    return;
                }
            }
        }
        panic!("reversed did not return a list");
    }

    #[test]
    fn test_list_sorted() {
        let ctx = test_ctx();
        let elems = vec![Value::number(3.0), Value::number(1.0), Value::number(2.0)];
        let result = dispatch_list_method(
            &ctx,
            "sorted",
            &elems,
            NO_ARGS,
            NO_REGS,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 3);
                    assert_eq!(elems[0].as_number(), Some(1.0));
                    assert_eq!(elems[1].as_number(), Some(2.0));
                    assert_eq!(elems[2].as_number(), Some(3.0));
                    return;
                }
            }
        }
        panic!("sorted did not return a list");
    }

    #[test]
    fn test_list_index_of() {
        let ctx = test_ctx();
        let elems = vec![
            Value::number(10.0),
            Value::number(20.0),
            Value::number(30.0),
        ];
        let regs = [Value::number(20.0)];
        let result = dispatch_list_method(
            &ctx,
            "index_of",
            &elems,
            &[0],
            &regs,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        assert_eq!(result.as_number(), Some(1.0));

        let elems2 = vec![Value::number(10.0), Value::number(20.0)];
        let regs2 = [Value::number(99.0)];
        let result2 = dispatch_list_method(
            &ctx,
            "index_of",
            &elems2,
            &[0],
            &regs2,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        assert_eq!(result2.as_number(), Some(-1.0));
    }

    #[test]
    fn test_list_slice() {
        let ctx = test_ctx();
        let elems = vec![
            Value::number(0.0),
            Value::number(1.0),
            Value::number(2.0),
            Value::number(3.0),
            Value::number(4.0),
        ];
        // slice(1, 3)
        let regs = [Value::number(1.0), Value::number(3.0)];
        let result = dispatch_list_method(
            &ctx,
            "slice",
            &elems,
            &[0, 1],
            &regs,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 2);
                    assert_eq!(elems[0].as_number(), Some(1.0));
                    assert_eq!(elems[1].as_number(), Some(2.0));
                    return;
                }
            }
        }
        panic!("slice did not return a list");
    }

    #[test]
    fn test_list_take() {
        let ctx = test_ctx();
        let elems = vec![Value::number(1.0), Value::number(2.0), Value::number(3.0)];
        let regs = [Value::number(2.0)];
        let result =
            dispatch_list_method(&ctx, "take", &elems, &[0], &regs, TEST_LOC, Value::nil())
                .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 2);
                    return;
                }
            }
        }
        panic!("take did not return a list");
    }

    #[test]
    fn test_list_drop() {
        let ctx = test_ctx();
        let elems = vec![Value::number(1.0), Value::number(2.0), Value::number(3.0)];
        let regs = [Value::number(1.0)];
        let result =
            dispatch_list_method(&ctx, "drop", &elems, &[0], &regs, TEST_LOC, Value::nil())
                .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 2);
                    assert_eq!(elems[0].as_number(), Some(2.0));
                    assert_eq!(elems[1].as_number(), Some(3.0));
                    return;
                }
            }
        }
        panic!("drop did not return a list");
    }

    #[test]
    fn test_list_unique() {
        let ctx = test_ctx();
        let elems = vec![
            Value::number(1.0),
            Value::number(2.0),
            Value::number(2.0),
            Value::number(3.0),
            Value::number(1.0),
        ];
        let result = dispatch_list_method(
            &ctx,
            "unique",
            &elems,
            NO_ARGS,
            NO_REGS,
            TEST_LOC,
            Value::nil(),
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 3);
                    return;
                }
            }
        }
        panic!("unique did not return a list");
    }

    // ── Object method tests ────────────────────────────────────────────────

    fn make_object(ctx: &Context, entries: Vec<(&str, Value)>) -> Value {
        let string_pool: Vec<Arc<str>> = entries.iter().map(|(k, _)| Arc::from(*k)).collect();
        // We need a way to get name_ids. Since we can't modify the context's
        // string pool easily after construction, we'll build into the pool
        // by taking advantage of the fact that we're constructing a fresh context.
        // We'll use name_ids that match positions in our local pool.
        // But the actual context's string_pool is different.
        //
        // Alternative: build the fields map using name_ids that we look up
        // from the context's string_pool at runtime.
        // The simplest approach: override the context's string_pool.
        // Since string_pool is pub, we can do that.

        // Actually the simplest: we'll look up name_ids from the string_pool
        // and insert entries one-by-one.
        let mut fields = rustc_hash::FxHashMap::default();
        for (key, val) in entries {
            // Find or skip — if the string isn't in the pool, we skip it
            if let Some(pos) = ctx.string_pool.iter().position(|s| s.as_ref() == key) {
                fields.insert(pos as u32, val);
            }
        }
        ctx.alloc(ManagedObject::Object(ObjectData::new(fields)))
    }

    #[test]
    fn test_object_len() {
        let ctx = test_ctx();
        // The default context has string_pool = [""], so we insert with name_id 0
        let mut fields = rustc_hash::FxHashMap::default();
        fields.insert(0u32, Value::number(42.0));
        let obj_val = ctx.alloc(ManagedObject::Object(ObjectData::new(fields)));

        let result = dispatch_object_method(
            &ctx,
            "len",
            obj_val.as_obj_id().unwrap(),
            NO_ARGS,
            NO_REGS,
            TEST_LOC,
        )
        .unwrap();
        assert_eq!(result.as_number(), Some(1.0));
    }

    #[test]
    fn test_object_has() {
        let ctx = test_ctx();
        let mut fields = rustc_hash::FxHashMap::default();
        // The string pool has [""] — so empty string is at index 0
        fields.insert(0u32, Value::number(42.0));
        let obj_val = ctx.alloc(ManagedObject::Object(ObjectData::new(fields)));

        // Check if the key "" exists (name_id 0)
        let regs = [Value::sso("").unwrap()];
        let result = dispatch_object_method(
            &ctx,
            "has",
            obj_val.as_obj_id().unwrap(),
            &[0],
            &regs,
            TEST_LOC,
        )
        .unwrap();
        assert!(result.as_bool().unwrap());

        // Check a non-existent key
        let regs2 = [Value::sso("nope").unwrap()];
        let result2 = dispatch_object_method(
            &ctx,
            "has",
            obj_val.as_obj_id().unwrap(),
            &[0],
            &regs2,
            TEST_LOC,
        )
        .unwrap();
        assert!(!result2.as_bool().unwrap());
    }

    #[test]
    fn test_object_keys_values() {
        let ctx = test_ctx();
        // Build a context with a richer string pool
        // We'll use an unsafe approach: replace the Arc's data.
        // Actually, we can just create a new context with a custom string_pool.
        let mut ctx_inner = Context::new();
        ctx_inner.string_pool = std::sync::Arc::from(vec![
            std::sync::Arc::from(""),
            std::sync::Arc::from("a"),
            std::sync::Arc::from("b"),
        ]);
        let ctx = Arc::new(ctx_inner);

        let mut fields = rustc_hash::FxHashMap::default();
        fields.insert(1u32, Value::number(10.0)); // "a"
        fields.insert(2u32, Value::number(20.0)); // "b"
        let obj_val = ctx.alloc(ManagedObject::Object(ObjectData::new(fields)));

        let result = dispatch_object_method(
            &ctx,
            "keys",
            obj_val.as_obj_id().unwrap(),
            NO_ARGS,
            NO_REGS,
            TEST_LOC,
        )
        .unwrap();
        if let Some(oid) = result.as_obj_id() {
            let objects = ctx.heap.objects.get();
            if let Some(Some(obj)) = objects.get(oid as usize) {
                if let ManagedObject::List(elems) = &obj.obj {
                    assert_eq!(elems.len(), 2);
                    // Keys may come in any order
                    let s0 = ctx.value_as_string(elems[0]);
                    let s1 = ctx.value_as_string(elems[1]);
                    assert!(
                        (s0 == Some("a".into()) && s1 == Some("b".into()))
                            || (s0 == Some("b".into()) && s1 == Some("a".into()))
                    );
                    return;
                }
            }
        }
        panic!("keys did not return a list");
    }
}
