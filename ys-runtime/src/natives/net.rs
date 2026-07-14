//! Network built-ins: `fetch`, `serve`, `spawn`.
//!
//! Uses `ureq` for blocking HTTP requests and `std::net` for TCP.

use crate::context::{Callable, Completion, Context, NativeFn, SpawnedTask};
use crate::heap::ManagedObject;
use crate::vm::execute_bytecode;
use crate::vm::PromiseState;
use crate::value_fmt::stringify_value;
use rustc_hash::FxHashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    fns.insert("fetch".into(), Arc::new(native_fetch));
    fns.insert("serve".into(), Arc::new(native_serve));
    fns.insert("spawn".into(), Arc::new(native_spawn));
}

/// Wrap a value in a resolved Promise on the heap.
fn resolved_promise(ctx: &Context, val: Value) -> Value {
    ctx.alloc(ManagedObject::Promise(PromiseState::Resolved(val)))
}

fn native_fetch(ctx: &Arc<Context>, args: &[Value]) -> Result<Value, JitError> {
    let url = args.first()
        .and_then(|v| ctx.value_as_string(*v))
        .ok_or_else(|| JitError::runtime(
            "fetch(url) requires a string URL as the first argument", 0, 0,
        ))?;

    // Create a pending promise that the event loop will resolve.
    let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Pending { continuation: None }));
    let promise_oid = promise.as_obj_id().unwrap();

    let ctx_clone = ctx.clone();
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
        ctx_clone.completions.lock().unwrap().push(Completion {
            promise_oid,
            result,
        });
    });

    Ok(promise)
}

fn native_serve(ctx: &Arc<Context>, args: &[Value]) -> Result<Value, JitError> {
    let (port_val, handler_val) = match args {
        [p, h] => (*p, *h),
        _ => return Err(JitError::runtime(
            "serve(port, handler) expects 2 arguments", 0, 0,
        )),
    };

    let port = port_val.as_number().ok_or_else(|| JitError::runtime(
        "serve: port must be a number", 0, 0,
    ))? as u16;

    let handler_name = ctx.value_as_string(handler_val).ok_or_else(|| JitError::runtime(
        "serve: handler must be a function name string", 0, 0,
    ))?;

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .map_err(|e| JitError::runtime(
            format!("Failed to bind to port {}: {}", port, e), 0, 0,
        ))?;

    println!("Web server listening on port {} (use Ctrl-C to stop)", port);

    let ctx_shared = Arc::clone(ctx);
    let handler = handler_name.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let ctx_clone = Arc::clone(&ctx_shared);
                    let h = handler.clone();
                    std::thread::spawn(move || {
                        handle_connection(stream, &ctx_clone, &h);
                    });
                }
                Err(e) => eprintln!("Connection error: {}", e),
            }
        }
    });

    // Return a resolved Promise indicating the server has started.
    let msg = format!("Server started on port {}", port);
    let val = Value::sso(&msg).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(msg))));
    Ok(resolved_promise(ctx, val))
}

fn native_spawn(ctx: &Arc<Context>, args: &[Value]) -> Result<Value, JitError> {
    if args.is_empty() {
        return Err(JitError::runtime("spawn(fn, ...) requires at least a function name or closure", 0, 0));
    }

    // Resolve the callable — by name string or closure value
    let (callable, call_args) = if let Some(name) = ctx.value_as_string(args[0]) {
        let c = ctx.get_callable_by_name(&name)
            .cloned()
            .ok_or_else(|| JitError::runtime(format!("spawn: unknown function '{}'", name), 0, 0))?;
        (c, args[1..].to_vec())
    } else if let Some(oid) = args[0].as_obj_id() {
        let objects = ctx.heap.objects.get();
        let entry = objects.get(oid as usize).and_then(|o| o.as_ref());
        match entry.map(|o| &o.obj) {
            Some(ManagedObject::Closure(cl)) => {
                let c = ctx.get_callable(cl.name_id)
                    .cloned()
                    .ok_or_else(|| JitError::runtime("spawn: closure references unknown function", 0, 0))?;
                // Prepend captured values as arguments
                let mut all_args: Vec<Value> = cl.captures.clone();
                all_args.extend_from_slice(&args[1..]);
                (c, all_args)
            }
            _ => return Err(JitError::runtime("spawn: first argument must be a function name or closure", 0, 0)),
        }
    } else {
        return Err(JitError::runtime("spawn: first argument must be a function name or closure", 0, 0));
    };

    // Create a pending promise
    let promise = ctx.alloc(ManagedObject::Promise(PromiseState::Pending { continuation: None }));
    let promise_oid = promise.as_obj_id().unwrap();

    // Queue the spawned task — the event loop will execute it on the main thread
    ctx.spawned_tasks.get_mut().push(SpawnedTask {
        promise_oid,
        callable,
        args: call_args,
    });

    Ok(promise)
}

fn handle_connection(mut stream: TcpStream, ctx: &Arc<Context>, handler_name: &str) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req_data = String::from_utf8_lossy(&buf[..n]).to_string();

    let name_id = ctx.string_pool
        .iter()
        .position(|s| s.as_ref() == handler_name)
        .map(|i| i as u32);

    let callable = name_id.and_then(|id| ctx.get_callable(id));

    let Some(Callable::User(f)) = callable else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\n\r\nHandler not found");
        return;
    };

    let mut registers = vec![Value::from_bits(0); f.locals_count];
    if f.locals_count > 0 {
        let val = if let Some(sso) = Value::sso(&req_data) {
            sso
        } else {
            ctx.alloc(ManagedObject::String(Arc::from(req_data.clone())))
        };
        registers[0] = val;
    }

    match execute_bytecode(&f.instructions, Arc::clone(ctx), registers, 0) {
        Ok(res) => {
            let body = stringify_value(ctx, res);
            let resp = if body.starts_with("HTTP/") {
                body
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
                    body.len(), body
                )
            };
            let _ = stream.write_all(resp.as_bytes());
        }
        Err(e) => {
            let err = format!("HTTP/1.1 500 Internal Server Error\r\n\r\nError: {:?}", e);
            let _ = stream.write_all(err.as_bytes());
        }
    }
}
