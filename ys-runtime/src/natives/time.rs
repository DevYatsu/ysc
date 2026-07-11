//! Time built-ins: `time`, `timestamp`, `sleep`.

use crate::context::NativeFn;
use crate::heap::ManagedObject;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("time".into(), Arc::new(|_, _| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        Ok(Value::number(now))
    }));

    fns.insert("timestamp".into(), Arc::new(|ctx, _| {
        Ok(ctx.alloc(ManagedObject::Timestamp(std::time::Instant::now())))
    }));

    fns.insert("sleep".into(), Arc::new(|_, args| {
        let [val] = args else {
            return Err(JitError::runtime(
                "sleep() expects 1 argument",
                0, 0,
            ));
        };
        let ms = val.as_number().ok_or_else(|| JitError::runtime(
            "sleep() expects numeric milliseconds",
            0, 0,
        ))?;
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        Ok(Value::from_bits(0))
    }));
}
