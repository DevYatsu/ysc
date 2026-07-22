//! Object operations: keys, values, entries, has.
//!
//! Each function takes the receiver object as its first argument
//! (the value that was piped into it).

use crate::context::NativeCtx;
use crate::heap::ManagedObject;
use crate::natives::NativeRegistry;
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// Extract the object ID from `args[0]`, verifying it's a ManagedObject::Object.
fn get_object<'a>(
    ctx: &'a NativeCtx,
    args: &[Value],
    name: &str,
) -> Result<(u32, &'a FxHashMap<u32, Value>), JitError> {
    let val = args.first().copied().unwrap_or(Value::nil());
    let oid = val.as_obj_id().ok_or_else(|| {
        JitError::runtime(
            format!("{}: expected an object as first argument", name),
            (0, 0),
        )
    })?;
    let objects = ctx.heap_objects();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::Object(d)) => Ok((oid, &d.map)),
        _ => Err(JitError::runtime(
            format!("{}: expected an object as first argument", name),
            (0, 0),
        )),
    }
}

pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("keys", |ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "keys")?;
        let mut keys: Vec<Value> = Vec::with_capacity(fields.len());
        for &name_id in fields.keys() {
            let name = ctx
                .as_inner()
                .string_pool
                .get(name_id as usize)
                .map(|s| s.as_ref())
                .unwrap_or("?");
            keys.push(Value::sso(name).unwrap_or_else(|| Value::pool(name_id)));
        }
        Ok(ctx.alloc(ManagedObject::List(keys)))
    });

    reg.insert("values", |ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "values")?;
        let vals: Vec<Value> = fields.values().copied().collect();
        Ok(ctx.alloc(ManagedObject::List(vals)))
    });

    reg.insert("entries", |ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "entries")?;
        let entries: Vec<Value> = fields
            .iter()
            .map(|(&name_id, &v)| {
                let name = ctx
                    .as_inner()
                    .string_pool
                    .get(name_id as usize)
                    .map(|s| s.as_ref())
                    .unwrap_or("?");
                let key = Value::sso(name).unwrap_or_else(|| Value::pool(name_id));
                ctx.alloc(ManagedObject::List(vec![key, v]))
            })
            .collect();
        Ok(ctx.alloc(ManagedObject::List(entries)))
    });

    reg.insert("has", |ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "has")?;
        let key = args
            .get(1)
            .and_then(|v| ctx.value_as_string(*v))
            .map(Cow::into_owned)
            .unwrap_or_default();
        let found = ctx.pool_id(&key).is_some_and(|id| fields.contains_key(&id));
        Ok(Value::bool(found))
    });
}
