use crate::heap::{Closure, Heap, ManagedObject, SyncCell};
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};
use rustc_hash::FxHashMap;

use ys_core::error::JitError;

//  Backend trait

/// An execution backend for compiled YatsuScript programs.
///
/// This trait allows different execution strategies (e.g. interpreter vs potentially a JIT)
/// to be swapped out while using the same shared context and heap.
pub trait Backend: Send + Sync {
    /// Execute a compiled program.
    fn run(&self, program: ys_core::compiler::Program) -> Result<(), JitError>;
}

//  Shared Execution State

/// Store for native function implementations.
/// Synchronous native function — no async overhead.
/// For async I/O (fetch/serve) use a separate mechanism.
pub type NativeFn = Arc<
    dyn Fn(&Arc<Context>, &[Value]) -> Result<Value, JitError> + Send + Sync,
>;

/// Any object that can be called (either a user-defined function or a native builtin).
#[derive(Clone)]
pub enum Callable {
    User(ys_core::compiler::UserFunction),
    Native(NativeFn),
}

/// The global shared state for a YatsuScript execution environment.
///
/// Contains:
/// - The [`Heap`] (object storage)
/// - The [`string_pool`] (interned strings)
/// - The `globals` register array
/// - The map of all registered [`Callable`] entities
pub struct Context {
    pub heap:       Heap,
    pub string_pool: Arc<[Arc<str>]>,
    pub globals:    SyncCell<Vec<Value>>,
    pub callables:  SyncCell<rustc_hash::FxHashMap<u32, Callable>>,
    pub callables_by_name: SyncCell<FxHashMap<String, Callable>>,
}

impl Context {
    /// Create a fresh context with an empty heap and no callables.
    pub fn new() -> Self {
        Self {
            heap: Heap {
                objects:        SyncCell::new(Vec::with_capacity(256)),
                metadata:       SyncCell::new(crate::heap::HeapMetadata {
                    free_list:      Vec::with_capacity(32),
                    nursery_ids:    Vec::with_capacity(256),
                    remembered_set: rustc_hash::FxHashSet::default(),
                }),
                gc_count:       SyncCell::new(0),
                alloc_since_gc: SyncCell::new(0),
            },
            string_pool:       std::sync::Arc::from(vec![std::sync::Arc::from("")]),
            globals:           SyncCell::new(Vec::new()),
            callables:         SyncCell::new(rustc_hash::FxHashMap::default()),
            callables_by_name: SyncCell::new(rustc_hash::FxHashMap::default()),
        }
    }

    /// Allocate a new object on the heap, automatically triggering a GC if needed.
    ///
    /// Returns the [`Value`] representing a reference to the new object.
    pub fn alloc(&self, obj: ManagedObject) -> Value {
        self.heap.alloc(obj, self)
    }

    /// Check if two values are equal, potentially diving into the heap for objects.
    pub fn values_equal(&self, a: Value, b: Value) -> bool {
        if a.to_bits() == b.to_bits() { return true; }

        match (a.as_obj_id(), b.as_obj_id()) {
            (Some(aid), Some(bid)) => {
                let heap = self.heap.objects.get();
                if let (Some(Some(ao)), Some(Some(bo))) = (heap.get(aid as usize), heap.get(bid as usize)) {
                    match (&ao.obj, &bo.obj) {
                        (ManagedObject::String(asrc), ManagedObject::String(bsrc)) => asrc == bsrc,
                        _ => aid == bid,
                    }
                } else { aid == bid }
            }
            _ => false,
        }
    }

    /// Retrieve a callable — first by name_id (fast path), then by string name.
    pub fn get_callable(&self, id: u32) -> Option<&Callable> {
        self.callables.get().get(&id)
    }

    /// Retrieve a callable by its string name (primary API for embedding).
    pub fn get_callable_by_name(&self, name: &str) -> Option<&Callable> {
        self.callables_by_name.get().get(name)
    }

    /// Register a native function so scripts can call it by name.
    ///
    /// This is the primary API for embedding YatsuScript in games and apps.
    pub fn register_sync(&mut self, name: &str, f: impl Fn(&Arc<Context>, &[Value]) -> Result<Value, JitError> + Send + Sync + 'static) {
        self.callables_by_name.get_mut().insert(name.to_string(), Callable::Native(Arc::new(f)));
    }

    /// Try to read a value as a string (SSO, heap, or string pool).
    pub fn value_as_string(&self, v: Value) -> Option<String> {
        if let Some(s) = v.as_sso_str() {
            return std::str::from_utf8(&s).ok().map(|s| s.to_string());
        }
        // Pool strings have their own tag to avoid ID collision with heap objects.
        if let Some(id) = v.as_pool_id() {
            if (id as usize) < self.string_pool.len() {
                return Some(self.string_pool[id as usize].to_string());
            }
        }
        if let Some(oid) = v.as_obj_id() {
            let heap = self.heap.objects.get();
            if let Some(Some(obj)) = heap.get(oid as usize)
                && let ManagedObject::String(s) = &obj.obj {
                    return Some(s.to_string());
            }
        }
        None
    }

    /// Convert a value to its interned pool ID if possible.
    pub fn value_as_pool_id(&self, v: Value) -> Option<u32> {
        if let Some(id) = v.as_pool_id() { return Some(id); }
        if let Some(oid) = v.as_obj_id() { return Some(oid); }
        if let Some(s) = v.as_sso_str() {
            let s_str = std::str::from_utf8(&s).ok()?;
            return self.string_pool.iter().position(|p| p.as_ref() == s_str).map(|i| i as u32);
        }
        None
    }

    /// Call a closure value with the given arguments.
    ///
    /// Native functions should pass their `&ctx` (an `&Arc<Context>`) as the first argument.
    /// Returns the closure's return value.
    pub fn call_closure(
        ctx: &Arc<Self>,
        closure_val: Value,
        args: Vec<Value>,
        loc: Loc,
    ) -> Result<Value, JitError> {
        let oid = closure_val
            .as_obj_id()
            .ok_or_else(|| JitError::runtime("Expected a closure", loc.line as usize, loc.col as usize))?;
        let cl = {
            let objects = ctx.heap.objects.get();
            let o = objects
                .get(oid as usize)
                .and_then(|o| o.as_ref())
                .ok_or_else(|| {
                    JitError::runtime("Expected a closure", loc.line as usize, loc.col as usize)
                })?;
            match &o.obj {
                ManagedObject::Closure(Closure { name_id, captures }) => crate::heap::Closure {
                    name_id: *name_id,
                    captures: captures.clone(),
                },
                _ => {
                    return Err(JitError::runtime(
                        "Expected a closure",
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            }
        };
        let callable = ctx.get_callable(cl.name_id).ok_or_else(|| {
            JitError::runtime("Unknown closure function", loc.line as usize, loc.col as usize)
        })?;
        let Callable::User(func) = callable else {
            return Err(JitError::runtime("Closure must be a user function", loc.line as usize, loc.col as usize));
        };
        let total_params = cl.captures.len() + args.len();
        if total_params != func.params_count {
            return Err(JitError::runtime(
                format!(
                    "Closure arity mismatch: expected {}, got {}",
                    func.params_count, total_params
                ),
                loc.line as usize,
                loc.col as usize,
            ));
        }
        let mut registers = vec![Value::from_bits(0); func.locals_count];
        for (i, v) in cl.captures.iter().enumerate() {
            registers[i] = *v;
        }
        for (i, v) in args.iter().enumerate() {
            registers[cl.captures.len() + i] = *v;
        }
        crate::vm::execute_bytecode(&func.instructions, Arc::clone(ctx), registers, 0)
}

}
