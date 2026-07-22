//! Network built-ins: `fetch`, `serve`, `spawn`.
//!
//! Uses `ureq` for blocking HTTP requests and `std::net` for TCP on native.
//! On wasm32, uses `web-sys::XmlHttpRequest` to call the browser's fetch API.

use crate::context::{Context, NativeCtx};
use crate::heap::ManagedObject;
use crate::natives::{NativeRegistry, alloc_string};
use crate::vm::PromiseState;
use std::borrow::Cow;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

#[cfg(not(target_arch = "wasm32"))]
use crate::context::{Completion, SpawnedTask};
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Read, Write};
#[cfg(not(target_arch = "wasm32"))]
use std::net::TcpListener;

pub(crate) fn register(reg: &mut NativeRegistry) {
    reg.insert("fetch", native_fetch);
    #[cfg(not(target_arch = "wasm32"))]
    {
        reg.insert("serve", native_serve);
        reg.insert("spawn", native_spawn);
    }
}

/// Wrap a value in a resolved Promise on the heap.
fn resolved_promise(ctx: &Context, val: Value) -> Value {
    ctx.alloc(ManagedObject::Promise(PromiseState::Resolved(val)))
}

/// Native fetch implementation for wasm32 using the browser's `XMLHttpRequest`.
#[cfg(target_arch = "wasm32")]
fn native_fetch(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let url = args
        .first()
        .and_then(|v| ctx.value_as_string(*v).map(Cow::into_owned))
        .ok_or_else(|| {
            JitError::runtime(
                "fetch(url) requires a string URL as the first argument",
                (0, 0),
            )
        })?;

    let xhr = web_sys::XmlHttpRequest::new()
        .map_err(|_| JitError::runtime("fetch: failed to create XMLHttpRequest", (0, 0)))?;

    // Open a synchronous request (blocks until complete).
    xhr.open_with_async("GET", &url, false)
        .map_err(|_| JitError::runtime("fetch: failed to open request", (0, 0)))?;

    xhr.send()
        .map_err(|_| JitError::runtime("fetch: failed to send request", (0, 0)))?;

    let _status = xhr.status();
    let text = match xhr.response_text() {
        Ok(Some(t)) => t,
        _ => String::new(),
    };

    let body = alloc_string(ctx.as_inner(), text);
    let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Resolved(body)));
    Ok(promise)
}

/// Native fetch implementation for non-wasm32 using `ureq` + threading.
#[cfg(not(target_arch = "wasm32"))]
fn native_fetch(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let url = args
        .first()
        .and_then(|v| ctx.value_as_string(*v).map(Cow::into_owned))
        .ok_or_else(|| {
            JitError::runtime(
                "fetch(url) requires a string URL as the first argument",
                (0, 0),
            )
        })?;

    // Create a pending promise that the event loop will resolve.
    let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Pending {
        continuation: None,
    }));
    let promise_oid = promise.as_obj_id().unwrap();

    let completions = ctx.completions_handle();
    let url_clone = url.clone();
    std::thread::spawn(move || {
        let result = match ureq::get(&url_clone).call() {
            Ok(resp) => {
                let status = resp.status();
                let mut reader = resp.into_body().into_reader();
                let mut body = String::new();
                let _ = reader.read_to_string(&mut body);
                println!("Fetch {}: {} - {}", url_clone, status, body);
                Ok(body)
            }
            Err(e) => {
                eprintln!("Fetch {} failed: {}", url_clone, e);
                Err("NetworkError".to_string())
            }
        };
        completions.lock().unwrap().push(Completion {
            promise_oid,
            result,
        });
    });

    Ok(promise)
}

#[cfg(not(target_arch = "wasm32"))]
fn native_serve(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    let (port_val, handler_val) = match args {
        [p, h] => (*p, *h),
        _ => {
            return Err(JitError::runtime(
                "serve(port, handler) expects 2 arguments",
                (0, 0),
            ));
        }
    };

    let port = port_val
        .as_number()
        .ok_or_else(|| JitError::runtime("serve: port must be a number", (0, 0)))?
        as u16;

    let handler_name = ctx
        .value_as_string(handler_val)
        .map(Cow::into_owned)
        .ok_or_else(|| {
            JitError::runtime("serve: handler must be a function name string", (0, 0))
        })?;

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).map_err(|e| {
        JitError::runtime(format!("Failed to bind to port {}: {}", port, e), (0, 0))
    })?;

    // Resolve the handler name to a string-pool index ONCE on the main thread.
    let name_id = match ctx
        .as_inner()
        .string_pool
        .iter()
        .position(|s| s.as_ref() == handler_name)
    {
        Some(i) => i as u32,
        None => {
            return Err(JitError::runtime(
                format!("serve: unknown handler '{}'", handler_name),
                (0, 0),
            ));
        }
    };

    println!("Web server listening on port {} (use Ctrl-C to stop)", port);

    let server_tasks = ctx.as_inner().server_tasks.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let tasks = server_tasks.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 4096];
                        let n = match stream.read(&mut buf) {
                            Ok(n) if n > 0 => n,
                            _ => return,
                        };
                        let req_data = String::from_utf8_lossy(&buf[..n]).to_string();

                        // Channel to receive the HTTP response from the event loop.
                        let (tx, rx) = std::sync::mpsc::sync_channel(1);

                        tasks.lock().unwrap().push_back(crate::context::ServerTask {
                            name_id,
                            request_body: req_data,
                            response_tx: tx,
                        });

                        // Block until the event loop processes and sends back the response.
                        if let Ok(response) = rx.recv() {
                            let _ = stream.write_all(response.as_bytes());
                        }
                    });
                }
                Err(e) => eprintln!("Connection error: {}", e),
            }
        }
    });

    // Return a resolved Promise indicating the server has started.
    let msg = format!("Server started on port {}", port);
    let val = Value::sso(&msg).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(msg))));
    Ok(resolved_promise(ctx.as_inner(), val))
}

#[cfg(not(target_arch = "wasm32"))]
fn native_spawn(ctx: &NativeCtx, args: &[Value]) -> Result<Value, JitError> {
    if args.is_empty() {
        return Err(JitError::runtime(
            "spawn(fn, ...) requires at least a function name or closure",
            (0, 0),
        ));
    }

    // Resolve the callable — by name string or closure value
    let (callable, call_args) = if let Some(name) = ctx.value_as_string(args[0]) {
        let c = ctx.get_callable_by_name(&name).cloned().ok_or_else(|| {
            JitError::runtime(format!("spawn: unknown function '{}'", name), (0, 0))
        })?;
        (c, args[1..].to_vec())
    } else if let Some(oid) = args[0].as_obj_id() {
        let objects = ctx.heap_objects();
        let entry = objects.get(oid as usize).and_then(|o| o.as_ref());
        match entry.map(|o| &o.obj) {
            Some(ManagedObject::Closure(cl)) => {
                let c = ctx.get_callable(cl.name_id).cloned().ok_or_else(|| {
                    JitError::runtime("spawn: closure references unknown function", (0, 0))
                })?;
                // Prepend captured values as arguments
                let mut all_args: Vec<Value> = cl.captures.clone();
                all_args.extend_from_slice(&args[1..]);
                (c, all_args)
            }
            _ => {
                return Err(JitError::runtime(
                    "spawn: first argument must be a function name or closure",
                    (0, 0),
                ));
            }
        }
    } else {
        return Err(JitError::runtime(
            "spawn: first argument must be a function name or closure",
            (0, 0),
        ));
    };

    // Create a pending promise
    let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Pending {
        continuation: None,
    }));
    let promise_oid = promise.as_obj_id().unwrap();

    // Queue the spawned task — the event loop will execute it on the main thread
    ctx.as_inner().spawned_tasks.get_mut().push(SpawnedTask {
        promise_oid,
        callable,
        args: call_args,
    });

    Ok(promise)
}

// handle_connection was removed — server requests are now processed by the
// event loop via ServerTask queue, so handler execution stays on the main thread.
