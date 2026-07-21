//! Object operations: keys, values, entries, has.
//!
//! Each function takes the receiver object as its first argument
//! (the value that was piped into it).

use crate::context::{Context, NativeFn};
use crate::heap::ManagedObject;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// Extract the object ID from `args[0]`, verifying it's a ManagedObject::Object.
fn get_object<'a>(ctx: &'a Context, args: &[Value], name: &str)
    -> Result<(u32, &'a FxHashMap<u32, Value>), JitError>
{
    let val = args.first().copied().unwrap_or(Value::from_bits(0));
    let oid = val.as_obj_id().ok_or_else(|| {
        JitError::runtime(format!("{}: expected an object as first argument", name), 0, 0)
    })?;
    let objects = ctx.heap.objects.get();
    let o = objects.get(oid as usize).and_then(|o| o.as_ref());
    match o.map(|o| &o.obj) {
        Some(ManagedObject::Object(fields)) => Ok((oid, fields)),
        _ => Err(JitError::runtime(format!("{}: expected an object as first argument", name), 0, 0)),
    }
}

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("keys".into(), Arc::new(|ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "keys")?;
        let mut keys: Vec<Value> = Vec::with_capacity(fields.len());
        for &name_id in fields.keys() {
            let name = ctx.string_pool.get(name_id as usize).map(|s| s.as_ref()).unwrap_or("?");
            keys.push(Value::sso(name).unwrap_or_else(|| Value::pool(name_id)));
        }
        Ok(ctx.alloc(ManagedObject::List(keys)))
    }));

    fns.insert("values".into(), Arc::new(|ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "values")?;
        let vals: Vec<Value> = fields.values().copied().collect();
        Ok(ctx.alloc(ManagedObject::List(vals)))
    }));

    fns.insert("entries".into(), Arc::new(|ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "entries")?;
        let entries: Vec<Value> = fields.iter().map(|(&name_id, &v)| {
            let name = ctx.string_pool.get(name_id as usize).map(|s| s.as_ref()).unwrap_or("?");
            let key = Value::sso(name).unwrap_or_else(|| Value::pool(name_id));
            ctx.alloc(ManagedObject::List(vec![key, v]))
        }).collect();
        Ok(ctx.alloc(ManagedObject::List(entries)))
    }));

    fns.insert("has".into(), Arc::new(|ctx, args| {
        let (_oid, fields) = get_object(ctx, args, "has")?;
        let key = args.get(1).and_then(|v| ctx.value_as_string(*v)).unwrap_or_default();
        let found = ctx.string_pool.iter().position(|s| s.as_ref() == key)
            .map_or(false, |id| fields.contains_key(&(id as u32)));
        Ok(Value::bool(found))
    }));
}
