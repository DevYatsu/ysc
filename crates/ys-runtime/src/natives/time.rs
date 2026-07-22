//! Time built-ins: `time`, `timestamp`, `sleep`.

#[cfg(feature = "networking")]
use crate::context::Completion;
use crate::heap::ManagedObject;
use crate::natives::NativeRegistry;
#[cfg(feature = "networking")]
use crate::vm::PromiseState;
use ys_core::compiler::Value;
use ys_core::error::JitError;

pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("time", |_, _| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        Ok(Value::number(now))
    });

    reg.insert("timestamp", |ctx, _| {
        Ok(ctx.alloc(ManagedObject::Timestamp(std::time::Instant::now())))
    });

    reg.insert("sleep", |_ctx, args| {
        let [val] = args else {
            return Err(JitError::runtime("sleep() expects 1 argument", (0, 0)));
        };
        let ms = val
            .as_number()
            .ok_or_else(|| JitError::runtime("sleep() expects numeric milliseconds", (0, 0)))?;

        // Background-thread path (native/threaded targets)
        #[cfg(feature = "networking")]
        {
            let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Pending {
                continuation: None,
            }));
            let promise_oid = promise.as_obj_id().unwrap();
            let completions = ctx.completions_handle();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                completions.lock().unwrap().push(Completion {
                    promise_oid,
                    result: Ok(String::new()),
                });
            });
            Ok(promise)
        }

        // Fallback for threadless targets (WASM, etc.) — blocking sleep
        #[cfg(not(feature = "networking"))]
        {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            Ok(Value::nil())
        }
    });
}
