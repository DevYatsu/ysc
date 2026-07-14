//! Time built-ins: `time`, `timestamp`, `sleep`.

use crate::context::NativeFn;
use crate::heap::ManagedObject;
#[cfg(feature = "networking")]
use crate::context::Completion;
#[cfg(feature = "networking")]
use crate::vm::PromiseState;
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

    fns.insert("sleep".into(), Arc::new(|_ctx, args| {
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

        // Background-thread path (native/threaded targets)
        #[cfg(feature = "networking")]
        {
            let promise = _ctx.alloc(ManagedObject::Promise(PromiseState::Pending { continuation: None }));
            let promise_oid = promise.as_obj_id().unwrap();
            let ctx_clone = _ctx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                ctx_clone.completions.lock().unwrap().push(Completion {
                    promise_oid,
                    result: Ok(String::new()),
                });
            });
            return Ok(promise);
        }

        // Fallback for threadless targets (WASM, etc.) — blocking sleep
        #[cfg(not(feature = "networking"))]
        {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            Ok(Value::from_bits(0))
        }
    }));
}
