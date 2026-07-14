//! Interpreter bootstrap and initialization.
//!
//! Handles setting up the initial [`Context`], registering native functions,
//! and launching the first call frame (the main module body).

use crate::context::{Callable, Context};
use crate::heap::{Heap, HeapMetadata, ManagedObject, SyncCell};
use crate::natives;
use crate::vm::{execute_bytecode, make_registers, PromiseState};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use ys_core::compiler::{Program, Value};
use ys_core::error::JitError;

/// Bootstraps the interpreter environment and executes the program.
pub fn run_interpreter(program: Program) -> Result<(), JitError> {
    // 1. Collect all native functions.
    let mut native_fns = FxHashMap::default();
    natives::register(&mut native_fns);

    // 2. Map string-pool IDs to Callables (User or Native).
    let mut callables: Vec<Option<Callable>> = vec![None; program.string_pool.len()];
    
    // Register native functions based on their names in the string pool.
    for (i, name) in program.string_pool.iter().enumerate() {
        if let Some(nf) = native_fns.remove(name.as_ref()) {
            callables[i] = Some(Callable::Native(nf));
        }
    }

    // Register user functions.
    for f in program.functions.iter() {
        if (f.name_id as usize) < callables.len() {
            callables[f.name_id as usize] = Some(Callable::User(f.clone()));
        }
    }

    // 3. Ensure common failure type names are in the string pool so they
    // can be referenced by name at runtime (e.g. division by zero).
    let failure_names: &[&str] = &[
        "DivisionByZero",
        "ModByZero",
        "IndexOutOfBounds",
        "TypeError",
        "NetworkError",
    ];
    let mut pool: Vec<Arc<str>> = program.string_pool.to_vec();
    for &name in failure_names {
        if !pool.iter().any(|s| s.as_ref() == name) {
            pool.push(Arc::from(name));
        }
    }
    // Ensure the callables Vec is large enough for any name_id
    while callables.len() < pool.len() {
        callables.push(None);
    }
    let string_pool: Arc<[Arc<str>]> = Arc::from(pool);

    // 4. Initialize the shared context.
    // Build string-keyed callables from both the callables Vec and remaining
    // native functions that weren't referenced in any source file.
    let mut callables_by_name: FxHashMap<String, Callable> = FxHashMap::default();
    for (name_id, callable) in callables.iter().enumerate() {
        if let Some(c) = callable {
            if let Some(name) = program.string_pool.get(name_id) {
                callables_by_name.insert(name.to_string(), c.clone());
            }
        }
    }
    // Native functions whose names weren't in any source file's string pool
    // still need to be accessible via string lookup.
    for (name, nf) in native_fns {
        callables_by_name.entry(name).or_insert_with(|| Callable::Native(nf));
    }
    let ctx = Arc::new(Context {
        globals: SyncCell::new(vec![Value::from_bits(0); program.globals_count]),
        string_pool,
        callables: SyncCell::new(callables),
        callables_by_name: SyncCell::new(callables_by_name),
        pending_tasks: SyncCell::new(Vec::new()),
        completions: std::sync::Mutex::new(Vec::new()),
        spawned_tasks: SyncCell::new(Vec::new()),
        heap: Heap {
            objects:        SyncCell::new(Vec::with_capacity(1024)),
            metadata:       SyncCell::new(HeapMetadata {
                free_list:      Vec::with_capacity(128),
                nursery_ids:    Vec::with_capacity(1024),
                remembered_set: FxHashSet::default(),
            }),
            gc_count:       SyncCell::new(0),
            alloc_since_gc: SyncCell::new(0),
        },
    });

    // 4. Setup the main task's register set.
    let main_regs = make_registers(program.locals_count);

    // 5. Execute the main bytecode block.
    execute_bytecode(&program.instructions, ctx.clone(), main_regs, 0)?;

    // 6. Event loop — drain completions, process spawned tasks, poll promises.
    loop {
        // 6a. Drain completions from background threads (fetch, sleep, etc.)
        {
            let completions = std::mem::take(&mut *ctx.completions.lock().unwrap());
            for comp in completions {
                // Extract continuation before replacing
                let continuation = {
                    let mut objs = ctx.heap.objects.get_mut();
                    if let Some(Some(slot)) = objs.get_mut(comp.promise_oid as usize) {
                        match &mut slot.obj {
                            ManagedObject::Promise(PromiseState::Pending { continuation: c }) => {
                                c.take()
                            }
                            _ => None,
                        }
                    } else { None }
                };
                match comp.result {
                    Ok(body) => {
                        let val = Value::sso(&body)
                            .unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(body))));
                        let mut objs = ctx.heap.objects.get_mut();
                        if let Some(Some(slot)) = objs.get_mut(comp.promise_oid as usize) {
                            slot.obj = ManagedObject::Promise(PromiseState::Resolved(val));
                        }
                    }
                    Err(failure_name) => {
                        let name_id = ctx.string_pool.iter()
                            .position(|s| s.as_ref() == failure_name)
                            .unwrap_or(0) as u32;
                        let mut objs = ctx.heap.objects.get_mut();
                        if let Some(Some(slot)) = objs.get_mut(comp.promise_oid as usize) {
                            slot.obj = ManagedObject::Promise(PromiseState::Rejected(name_id));
                        }
                    }
                }
                // Resume the continuation if there was one
                if let Some(frame) = continuation {
                    execute_bytecode(&frame.instructions, ctx.clone(), frame.registers, frame.pc)?;
                }
            }
        }

        // 6b. Drain spawned tasks (spawn() calls that run on the main thread)
        {
            let spawned = std::mem::take(&mut *ctx.spawned_tasks.get_mut());
            for task in spawned {
                let result = match &task.callable {
                    crate::context::Callable::User(f) => {
                        let mut regs = vec![Value::from_bits(0); f.locals_count];
                        for (i, arg) in task.args.iter().enumerate() {
                            if i < f.locals_count { regs[i] = *arg; }
                        }
                        execute_bytecode(&f.instructions, ctx.clone(), regs, 0)
                    }
                    crate::context::Callable::Native(nf) => {
                        (nf)(&ctx, &task.args)
                    }
                };
                // Extract continuation BEFORE replacing the promise on the heap
                let continuation = {
                    let mut objs = ctx.heap.objects.get_mut();
                    if let Some(Some(slot)) = objs.get_mut(task.promise_oid as usize) {
                        match &mut slot.obj {
                            ManagedObject::Promise(PromiseState::Pending { continuation: c }) => {
                                c.take()
                            }
                            _ => None,
                        }
                    } else { None }
                };
                match result {
                    Ok(val) => {
                        let mut objs = ctx.heap.objects.get_mut();
                        if let Some(Some(slot)) = objs.get_mut(task.promise_oid as usize) {
                            slot.obj = ManagedObject::Promise(PromiseState::Resolved(val));
                        }
                    }
                    Err(_) => {
                        let failure_id = ctx.string_pool.iter()
                            .position(|s| s.as_ref() == "TypeError").unwrap_or(0) as u32;
                        let mut objs = ctx.heap.objects.get_mut();
                        if let Some(Some(slot)) = objs.get_mut(task.promise_oid as usize) {
                            slot.obj = ManagedObject::Promise(PromiseState::Rejected(failure_id));
                        }
                    }
                }
                // Resume the continuation if there was one
                if let Some(frame) = continuation {
                    execute_bytecode(&frame.instructions, ctx.clone(), frame.registers, frame.pc)?;
                }
            }
        }

        let tasks = std::mem::take(&mut *ctx.pending_tasks.get_mut());
        if tasks.is_empty() { break; }

        let mut new_tasks: Vec<Value> = Vec::new();

        for task_val in tasks {
            let Some(oid) = task_val.as_obj_id() else { continue; };

            // Clone the heap entry's promise state to avoid holding heap lock
            let state_clone = {
                let objects = ctx.heap.objects.get();
                objects.get(oid as usize)
                    .and_then(|o| o.as_ref())
                    .and_then(|o| match &o.obj {
                        ManagedObject::Promise(ps) => Some(ps.clone()),
                        _ => None,
                    })
            };

            let Some(ref state) = state_clone else { continue; };

            // Handle single pending promise
            if let PromiseState::Pending { continuation: Some(c) } = state {
                let is_resolved = {
                    let objects = ctx.heap.objects.get();
                    objects.get(oid as usize)
                        .and_then(|o| o.as_ref())
                        .is_some_and(|o| matches!(&o.obj, ManagedObject::Promise(PromiseState::Resolved(_))))
                };
                if is_resolved {
                    execute_bytecode(&c.instructions, ctx.clone(), c.registers.clone(), c.pc)?;
                } else {
                    new_tasks.push(task_val);
                }
                continue;
            }

            // Handle compound (parallel) promise
            if let PromiseState::Compound { sub_promises, results, continuation: Some(c) } = state {
                let mut all_done = true;
                let mut any_changed = false;
                let mut updated_sp = sub_promises.clone();
                let mut resolved_vals = results.clone();
                // Clone the continuation NOW — state becomes invalid after heap mutation
                let cont_instrs = c.instructions.clone();
                let cont_regs = c.registers.clone();
                let cont_pc = c.pc;

                for (i, sub) in sub_promises.iter().enumerate() {
                    let Some(sub_oid) = sub else { continue; };
                    let resolved_val = {
                        let objects = ctx.heap.objects.get();
                        objects.get(*sub_oid as usize)
                            .and_then(|o| o.as_ref())
                            .and_then(|o| match &o.obj {
                                ManagedObject::Promise(PromiseState::Resolved(v)) => Some(*v),
                                _ => None,
                            })
                    };
                    match resolved_val {
                        Some(val) => {
                            resolved_vals[i] = val;
                            updated_sp[i] = None;
                            any_changed = true;
                        }
                        None => { all_done = false; }
                    }
                }

                if all_done && any_changed {
                    let final_list = ctx.alloc(ManagedObject::List(resolved_vals));
                    // Mutation: this drops the old Compound (including `state`'s data)
                    let mut objs = ctx.heap.objects.get_mut();
                    if let Some(Some(slot)) = objs.get_mut(oid as usize) {
                        slot.obj = ManagedObject::Promise(PromiseState::Resolved(final_list));
                    }
                    drop(objs);
                    // Resume continuation using the cloned data
                    execute_bytecode(&cont_instrs, ctx.clone(), cont_regs, cont_pc)?;
                } else if any_changed {
                    {
                        let mut objs = ctx.heap.objects.get_mut();
                        if let Some(Some(slot)) = objs.get_mut(oid as usize) {
                            if let ManagedObject::Promise(PromiseState::Compound { sub_promises: sp, results: res, .. }) = &mut slot.obj {
                                *sp = updated_sp;
                                *res = resolved_vals;
                            }
                        }
                    }
                    new_tasks.push(task_val);
                } else {
                    new_tasks.push(task_val);
                }
                continue;
            }
        }

        if new_tasks.is_empty() { break; }
        {
            let mut pending = ctx.pending_tasks.get_mut();
            for v in new_tasks { pending.push(v); }
        }
    }

    Ok(())
}

