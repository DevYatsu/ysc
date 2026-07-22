//! Collection access handlers for list/object get/set operations.
//!
//! These functions are called from the main dispatch loop in `mod.rs` for
//! instructions like `ListGet`, `ListSet`, `ObjectGet`, and `ObjectSet`.

use crate::context::Context;
use crate::heap::{Generation, ManagedObject};
use ys_core::compiler::{Loc, Value};

// ─────────────────────────────────────────────────────────────────────────────
//  GetResult
// ─────────────────────────────────────────────────────────────────────────────

/// The result of a collection get operation.
pub enum GetResult {
    /// A plain value was found.
    Value(Value),
    /// The field was not found — treat as a bound-method dispatch candidate.
    BoundMethod(Value),
    /// A runtime error message.
    Error(String),
}

// ─────────────────────────────────────────────────────────────────────────────
//  Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Look up a string key on an object value.
fn handle_object_get_by_key(obj_val: Value, key: &str, ctx: &Context) -> GetResult {
    if let Some(oid) = obj_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        let obj = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(obj) = obj
            && let ManagedObject::Object(d) = &obj.obj
        {
            let name_id = ctx.pool_id(key);
            match name_id.and_then(|id| d.map.get(&id)) {
                Some(val) => return GetResult::Value(*val),
                None => return GetResult::Error(format!("Object has no field '{}'", key)),
            }
        }
    }
    GetResult::Error("Expected an object for field access".into())
}

// ─────────────────────────────────────────────────────────────────────────────
//  Public handlers
// ─────────────────────────────────────────────────────────────────────────────

/// Get an element from a list (or string/object) by index.
pub fn handle_list_get(
    regs: &[Value],
    list: usize,
    index_reg: usize,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let list_val = regs[list];
    let index_val = regs[index_reg];

    // If index is a string, try object field access
    if let Some(key) = ctx.value_as_string(index_val) {
        return handle_object_get_by_key(list_val, &key, ctx);
    }

    let idx = match index_val.as_number() {
        Some(n) => n as usize,
        None => return GetResult::Error("List index must be a number".into()),
    };

    if let Some(oid) = list_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        let obj = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(obj) = obj {
            return match &obj.obj {
                ManagedObject::List(elems) => {
                    if idx < elems.len() {
                        GetResult::Value(unsafe { *elems.get_unchecked(idx) })
                    } else {
                        GetResult::Error(format!(
                            "List index {} out of bounds (len={})",
                            idx,
                            elems.len()
                        ))
                    }
                }
                ManagedObject::String(s) => {
                    if idx < s.len() {
                        let byte = unsafe { *s.as_bytes().get_unchecked(idx) };
                        {
                            let mut buf = [0u8; 4];
                            let s = (byte as char).encode_utf8(&mut buf);
                            GetResult::Value(Value::sso(s).unwrap_or(Value::nil()))
                        }
                    } else {
                        GetResult::Error(format!("String index {} out of bounds", idx))
                    }
                }
                ManagedObject::Object(d) => {
                    // Numeric index on an object — check if string key exists as number
                    let key_str = idx.to_string();
                    let name_id = ctx.pool_id(&key_str);
                    match name_id.and_then(|id| d.map.get(&id)) {
                        Some(val) => GetResult::Value(*val),
                        None => GetResult::Error(format!("Object has no field '{}'", idx)),
                    }
                }
                _ => GetResult::Error("Expected a list, string, or object for index".into()),
            };
        }
        GetResult::Error("Null object dereference".into())
    } else if let Some(s) = ctx.value_as_string(list_val)
        && idx < s.len()
    {
        let byte = s.as_bytes()[idx];
        {
            let mut buf = [0u8; 4];
            let s = (byte as char).encode_utf8(&mut buf);
            GetResult::Value(Value::sso(s).unwrap_or(Value::nil()))
        }
    } else {
        GetResult::Error("Expected a list or string for index".into())
    }
}

/// Set an element in a list by index.
pub fn handle_list_set(
    regs: &[Value],
    list: usize,
    index_reg: usize,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let list_val = regs[list];
    let index_val = regs[index_reg];
    let src_val = regs[src];
    let idx = index_val
        .as_number()
        .ok_or_else(|| "List index must be a number".to_string())? as usize;
    let oid = list_val
        .as_obj_id()
        .ok_or_else(|| "Expected list for index assignment".to_string())?;

    let generation;
    {
        let heap = ctx.heap.objects.get_mut();
        // Safety: object ID is always valid.
        let obj = unsafe { heap.get_unchecked_mut(oid as usize) };
        let obj = obj
            .as_mut()
            .ok_or_else(|| "Expected list for index assignment".to_string())?;
        let ManagedObject::List(elems) = &mut obj.obj else {
            return Err("Expected list for index assignment".to_string());
        };
        if idx < elems.len() {
            // Safety: idx is checked above.
            unsafe {
                *elems.get_unchecked_mut(idx) = src_val;
            }
        } else {
            elems.resize(idx + 1, Value::nil());
            elems[idx] = src_val;
        }
        generation = obj.generation;
    }

    record_write_barrier(ctx, generation, oid, src_val);
    Ok(())
}

/// Insert a remembered-set entry when a tenured object gains a reference to a
/// nursery object.  Called by [`handle_list_set`] and [`handle_object_set`].
#[inline]
pub fn record_write_barrier(ctx: &Context, generation: Generation, oid: u32, val: Value) {
    if generation == Generation::Tenured
        && let Some(src_oid) = val.as_obj_id()
        && let Some(Some(src_obj)) = ctx.heap.objects.get().get(src_oid as usize)
        && src_obj.generation == Generation::Nursery
    {
        ctx.heap.metadata.get_mut().remembered_set.insert(oid);
    }
}

/// Get a property from an object by `name_id`.
pub fn handle_object_get(
    regs: &[Value],
    obj: usize,
    name_id: u32,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let obj_val = regs[obj];
    if let Some(oid) = obj_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        let o = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(o) = o {
            return match &o.obj {
                ManagedObject::Object(d) => {
                    if let Some(slot) = d.map.get(&name_id) {
                        GetResult::Value(*slot)
                    } else {
                        GetResult::BoundMethod(obj_val)
                    }
                }
                // Lists, ranges, closures, BoundMethods themselves, etc.
                // all use BoundMethod dispatch for method calls.
                _ => GetResult::BoundMethod(obj_val),
            };
        }
        GetResult::Error("Null object dereference".into())
    } else {
        // SSO strings, numbers, booleans — allow method dispatch
        // by treating property access as BoundMethod creation.
        GetResult::BoundMethod(obj_val)
    }
}

/// Set a property on an object by `name_id`.
pub fn handle_object_set(
    regs: &[Value],
    obj: usize,
    name_id: u32,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let obj_val = regs[obj];
    let src_val = regs[src];
    let oid = obj_val
        .as_obj_id()
        .ok_or_else(|| "Expected object for property assignment".to_string())?;

    let generation;
    {
        let heap = ctx.heap.objects.get_mut();
        // Safety: object ID is always valid.
        let o = unsafe { heap.get_unchecked_mut(oid as usize) };
        let o = o
            .as_mut()
            .ok_or_else(|| "Expected object for property assignment".to_string())?;
        let ManagedObject::Object(d) = &mut o.obj else {
            return Err("Expected object for property assignment".to_string());
        };
        let existing = d.map.get_mut(&name_id);
        if let Some(slot) = existing {
            *slot = src_val;
        } else {
            d.map.insert(name_id, src_val);
        }
        generation = o.generation;
    }

    record_write_barrier(ctx, generation, oid, src_val);
    Ok(())
}
