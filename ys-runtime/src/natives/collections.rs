//! Collection built-ins: `len`.

use crate::context::NativeFn;
use crate::heap::ManagedObject;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("len".into(), Arc::new(|ctx, args| {
        let [val] = args else {
            return Err(JitError::runtime(
                "len() expects 1 argument",
                0, 0,
            ));
        };
        let val = *val;

        if let Some(oid) = val.as_obj_id() {
            let heap = ctx.heap.objects.get();
            if let Some(Some(obj)) = heap.get(oid as usize) {
                return Ok(Value::number(match &obj.obj {
                    ManagedObject::String(s)  => s.len() as f64,
                    ManagedObject::List(l)    => l.len() as f64,
                    ManagedObject::Object(o)  => o.len() as f64,
                    ManagedObject::Range { start, end, step } => {
                        if *step == 0.0 { 0.0 }
                        else { ((end - start) / step).ceil().max(0.0) }
                    }
                    ManagedObject::Timestamp(_) | ManagedObject::BoundMethod { .. }
                    | ManagedObject::Closure(_) => 0.0,
                }));
            }
        } else if let Some(s) = ctx.value_as_string(val) {
            return Ok(Value::number(s.len() as f64));
        }

        Err(JitError::runtime(
            "len() expects string or list",
            0, 0,
        ))
    }));
}
