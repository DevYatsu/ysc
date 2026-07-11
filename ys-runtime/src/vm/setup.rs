//! Interpreter bootstrap and initialization.
//!
//! Handles setting up the initial [`Context`], registering native functions,
//! and launching the first call frame (the main module body).

use crate::context::{Callable, Context};
use crate::heap::{Heap, HeapMetadata, SyncCell};
use crate::natives;
use crate::vm::{execute_bytecode, make_registers};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use ys_core::compiler::{Program, Value};
use ys_core::error::JitError;

/// Bootstraps the interpreter environment and executes the program.
pub async fn run_interpreter(program: Program) -> Result<(), JitError> {
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

    let mut callable_map = FxHashMap::default();
    for (i, c) in callables.into_iter().enumerate() {
        if let Some(callable) = c {
            callable_map.insert(i as u32, callable);
        }
    }

    // 3. Initialize the shared context.
    let ctx = Arc::new(Context {
        globals: SyncCell::new(vec![Value::from_bits(0); program.globals_count]),
        string_pool: Arc::clone(&program.string_pool),
        callables: callable_map,
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
    execute_bytecode(&program.instructions, ctx.clone(), main_regs).await?;

    Ok(())
}

