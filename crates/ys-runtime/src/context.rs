use crate::heap::{Closure, Heap, HeapObject, ManagedObject, SyncCell};
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};

use ys_core::error::JitError;

/// A completed async operation — resolves or rejects a pending promise.
/// Background threads push these; the event loop drains them on the main thread.
/// The value is communicated as a raw string; the event loop turns it into a
/// proper heap-allocated Value (string, number, etc.) before resolving.
#[derive(Clone)]
pub struct Completion {
    /// Object ID of the pending promise to resolve/reject.
    pub promise_oid: u32,
    /// `Ok(body_string)` for resolution (response body, sleep nil, etc.)
    /// `Err(failure_name)` for rejection (interned into string pool).
    pub result: Result<String, String>,
}

/// A request from an HTTP server connection, queued for event-loop handling.
/// The thread reads the request and pushes this; the event loop executes the
/// handler on the main thread and sends the response back through the channel.
pub struct ServerTask {
    /// String-pool index of the handler function name.
    pub name_id: u32,
    /// Raw HTTP request body read by the thread.
    pub request_body: String,
    /// Channel to send the HTTP response back to the connection thread.
    pub response_tx: std::sync::mpsc::SyncSender<String>,
}

/// A spawned function call queued for execution on the main thread.
/// The event loop drains these between promise polling iterations.
pub struct SpawnedTask {
    /// Object ID of the pending promise to resolve/reject when done.
    pub promise_oid: u32,
    /// The callable to invoke.
    pub callable: Callable,
    /// Arguments to pass to the callable (already on the main thread's heap).
    pub args: Vec<Value>,
}

//  Backend trait

/// An execution backend for compiled ysc programs.
///
/// This trait allows different execution strategies (e.g. interpreter vs potentially a JIT)
/// to be swapped out while using the same shared context and heap.
pub trait Backend: Send + Sync {
    /// Execute a compiled program.
    fn run(&self, program: ys_core::compiler::Program) -> Result<(), JitError>;
}

//  Shared Execution State

/// Lightweight facade over [`Context`] exposed to native function callbacks.
///
/// Native functions receive `&NativeCtx` instead of `&Context` so that changes
/// to `Context`'s internal structure (e.g. `Arc<Context>` → stack-local) never
/// ripple into embedder code.  Only this struct's methods need updating.
pub struct NativeCtx<'a> {
    ctx: &'a Context,
}

impl<'a> NativeCtx<'a> {
    #[inline]
    pub fn new(ctx: &'a Context) -> Self {
        Self { ctx }
    }

    /// Allocate a new heap object and return its NaN-boxed reference.
    #[inline]
    pub fn alloc(&self, obj: ManagedObject) -> Value {
        self.ctx.alloc(obj)
    }

    /// Look up a string's pool index (O(1) hash map).
    #[inline]
    pub fn pool_id(&self, name: &str) -> Option<u32> {
        self.ctx.pool_id(name)
    }

    /// Extract a string value (SSO, pool, or heap) as a `Cow<str>`.
    #[inline]
    pub fn value_as_string(&self, v: Value) -> Option<Cow<'_, str>> {
        self.ctx.value_as_string(v)
    }

    /// Convert a value to its interned pool ID.
    #[inline]
    pub fn value_as_pool_id(&self, v: Value) -> Option<u32> {
        self.ctx.value_as_pool_id(v)
    }

    /// Read an entry from the interned string pool by index.
    #[inline]
    pub fn string_pool_get(&self, id: usize) -> Option<&Arc<str>> {
        self.ctx.string_pool.get(id)
    }

    /// Return a clone-able handle to the completions queue so threads can push
    /// completions without owning the full `Context`.
    #[inline]
    pub fn completions_handle(&self) -> Arc<std::sync::Mutex<Vec<Completion>>> {
        self.ctx.completions.clone()
    }

    /// Push a completion inline (main-thread path).
    #[inline]
    pub fn push_completion(&self, c: Completion) {
        self.ctx.completions.lock().unwrap().push(c);
    }

    /// Call a closure value with the given arguments.
    #[inline]
    pub fn call_closure(
        &self,
        closure_val: Value,
        args: &[Value],
        loc: Loc,
    ) -> Result<Value, JitError> {
        Context::call_closure(self.ctx, closure_val, args, loc)
    }

    /// Access the globals array (read-only).
    #[inline]
    pub fn globals(&self) -> &Vec<Value> {
        self.ctx.globals.get()
    }

    /// Access the global callables map by name_id (O(1)).
    #[inline]
    pub fn get_callable(&self, id: u32) -> Option<&Callable> {
        self.ctx.get_callable(id)
    }

    /// Access the global callables map by name string.
    #[inline]
    pub fn get_callable_by_name(&self, name: &str) -> Option<&Callable> {
        self.ctx.callables_by_name.get().get(name)
    }

    /// Read-only access to the heap object storage (indexed by object ID).
    #[inline]
    pub fn heap_objects(&self) -> &Vec<Option<HeapObject>> {
        self.ctx.heap.objects.get()
    }

    /// Escape hatch for internal helper functions that still take `&Context`.
    /// Native function callbacks should prefer `NativeCtx` methods over this.
    #[inline]
    pub fn as_inner(&self) -> &Context {
        self.ctx
    }
}

impl<'a> AsRef<Context> for NativeCtx<'a> {
    fn as_ref(&self) -> &Context {
        self.ctx
    }
}

/// Store for native function implementations.
/// Synchronous native function — no async overhead.
/// For async I/O (fetch/serve) use a separate mechanism.
pub type NativeFn =
    Arc<dyn for<'a> Fn(&NativeCtx<'a>, &[Value]) -> Result<Value, JitError> + Send + Sync>;

/// Any object that can be called (either a user-defined function or a native builtin).
#[derive(Clone)]
pub enum Callable {
    User(ys_core::compiler::UserFunction),
    Native(NativeFn),
}

/// The global shared state for a ysc execution environment.
///
/// Contains:
/// - The [`Heap`] (object storage)
/// - The [`string_pool`] (interned strings)
/// - The `globals` register array
/// - The map of all registered [`Callable`] entities
pub struct Context {
    pub heap: Heap,
    pub string_pool: Arc<[Arc<str>]>,
    /// O(1) name→index lookup for the string pool. Built once at startup.
    pub string_pool_map: FxHashMap<String, u32>,
    pub globals: SyncCell<Vec<Value>>,
    /// Callables indexed by name_id (string-pool index) for O(1) lookup.
    pub callables: SyncCell<Vec<Option<Callable>>>,
    pub callables_by_name: SyncCell<FxHashMap<String, Callable>>,
    /// Pending promises that the event loop needs to poll on each iteration.
    /// Each entry is a Value holding an object ID of a Promise in Pending state.
    pub pending_tasks: SyncCell<Vec<Value>>,
    /// Completions pushed by background threads (fetch, sleep, etc.).
    /// Drained by the event loop on each tick. Uses a real Mutex for thread safety.
    pub completions: Arc<std::sync::Mutex<Vec<Completion>>>,
    /// HTTP server requests from network threads, queued for main-thread
    /// handler execution.
    pub server_tasks: Arc<std::sync::Mutex<VecDeque<ServerTask>>>,
    /// Spawned function calls queued for execution on the main thread.
    pub spawned_tasks: SyncCell<Vec<SpawnedTask>>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// Create a fresh context with an empty heap and no callables.
    pub fn new() -> Self {
        let default_pool = vec![std::sync::Arc::from("")];
        let string_pool: Arc<[Arc<str>]> = std::sync::Arc::from(default_pool);
        let string_pool_map = string_pool
            .iter()
            .enumerate()
            .map(|(i, s)| (s.to_string(), i as u32))
            .collect();
        Self {
            heap: Heap {
                objects: SyncCell::new(Vec::with_capacity(256)),
                metadata: SyncCell::new(crate::heap::HeapMetadata {
                    free_list: Vec::with_capacity(32),
                    nursery_ids: Vec::with_capacity(256),
                    remembered_set: rustc_hash::FxHashSet::default(),
                }),
                gc_count: SyncCell::new(0),
                alloc_since_gc: SyncCell::new(0),
            },
            string_pool,
            string_pool_map,
            globals: SyncCell::new(Vec::new()),
            callables: SyncCell::new(Vec::new()),
            callables_by_name: SyncCell::new(rustc_hash::FxHashMap::default()),
            pending_tasks: SyncCell::new(Vec::new()),
            completions: Arc::new(std::sync::Mutex::new(Vec::new())),
            server_tasks: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            spawned_tasks: SyncCell::new(Vec::new()),
        }
    }

    /// Allocate a new object on the heap, automatically triggering a GC if needed.
    ///
    /// Returns the [`Value`] representing a reference to the new object.
    pub fn alloc(&self, obj: ManagedObject) -> Value {
        self.heap.alloc(obj, self)
    }

    /// Look up a string's pool index in O(1) instead of scanning the pool.
    pub fn pool_id(&self, name: &str) -> Option<u32> {
        self.string_pool_map.get(name).copied()
    }

    /// Check if two values are equal, potentially diving into the heap for objects.
    pub fn values_equal(&self, a: Value, b: Value) -> bool {
        if a.to_bits() == b.to_bits() {
            return true;
        }

        match (a.as_obj_id(), b.as_obj_id()) {
            (Some(aid), Some(bid)) => {
                let heap = self.heap.objects.get();
                if let (Some(Some(ao)), Some(Some(bo))) =
                    (heap.get(aid as usize), heap.get(bid as usize))
                {
                    match (&ao.obj, &bo.obj) {
                        (ManagedObject::String(asrc), ManagedObject::String(bsrc)) => asrc == bsrc,
                        _ => aid == bid,
                    }
                } else {
                    aid == bid
                }
            }
            _ => false,
        }
    }

    /// Retrieve a callable by its name_id (string-pool index) — O(1) Vec index.
    pub fn get_callable(&self, id: u32) -> Option<&Callable> {
        self.callables
            .get()
            .get(id as usize)
            .and_then(|c| c.as_ref())
    }

    /// Retrieve a callable by its string name (primary API for embedding).
    pub fn get_callable_by_name(&self, name: &str) -> Option<&Callable> {
        self.callables_by_name.get().get(name)
    }

    /// Register a native function so scripts can call it by name.
    ///
    /// This is the primary API for embedding ysc in games and apps.
    pub fn register_sync(
        &mut self,
        name: &str,
        f: impl Fn(&NativeCtx<'_>, &[Value]) -> Result<Value, JitError> + Send + Sync + 'static,
    ) {
        self.callables_by_name
            .get_mut()
            .insert(name.to_string(), Callable::Native(Arc::new(f)));
    }

    /// Try to read a value as a string (SSO, heap, or string pool).
    ///
    /// Pool strings return `Cow::Borrowed` (zero-copy). SSO and heap strings
    /// return `Cow::Owned` since they must be decoded or cloned.
    pub fn value_as_string(&self, v: Value) -> Option<Cow<'_, str>> {
        if let Some(s) = v.as_sso_str() {
            // Slice to actual string length to avoid trailing null bytes from
            // the 6-byte SSO buffer padding.
            let len = v.sso_len().unwrap_or(6);
            return std::str::from_utf8(&s[..len])
                .ok()
                .map(|s| Cow::Owned(s.to_string()));
        }
        // Pool strings have their own tag to avoid ID collision with heap objects.
        if let Some(id) = v.as_pool_id()
            && (id as usize) < self.string_pool.len()
        {
            return Some(Cow::Borrowed(&self.string_pool[id as usize]));
        }
        if let Some(oid) = v.as_obj_id() {
            let heap = self.heap.objects.get();
            if let Some(Some(obj)) = heap.get(oid as usize)
                && let ManagedObject::String(s) = &obj.obj
            {
                return Some(Cow::Owned(s.to_string()));
            }
        }
        None
    }

    /// Convert a value to its interned pool ID if possible.
    pub fn value_as_pool_id(&self, v: Value) -> Option<u32> {
        if let Some(id) = v.as_pool_id() {
            return Some(id);
        }
        if let Some(oid) = v.as_obj_id() {
            return Some(oid);
        }
        if let Some(s) = v.as_sso_str() {
            let len = v.sso_len().unwrap_or(6);
            let s_str = std::str::from_utf8(&s[..len]).ok()?;
            return self.pool_id(s_str);
        }
        None
    }

    /// Call a closure value with the given arguments.
    ///
    /// Native functions pass `ctx` (a `&Context`) as the first argument.
    /// Returns the closure's return value.
    pub fn call_closure(
        ctx: &Self,
        closure_val: Value,
        args: &[Value],
        loc: Loc,
    ) -> Result<Value, JitError> {
        let oid = closure_val
            .as_obj_id()
            .ok_or_else(|| JitError::runtime("Expected a closure", loc.as_error_pos()))?;
        let cl = {
            let objects = ctx.heap.objects.get();
            let o = objects
                .get(oid as usize)
                .and_then(|o| o.as_ref())
                .ok_or_else(|| JitError::runtime("Expected a closure", loc.as_error_pos()))?;
            match &o.obj {
                ManagedObject::Closure(Closure { name_id, captures }) => crate::heap::Closure {
                    name_id: *name_id,
                    captures: captures.clone(),
                },
                _ => return Err(JitError::runtime("Expected a closure", loc.as_error_pos())),
            }
        };
        let callable = ctx
            .get_callable(cl.name_id)
            .ok_or_else(|| JitError::runtime("Unknown closure function", loc.as_error_pos()))?;
        let Callable::User(func) = callable else {
            return Err(JitError::runtime(
                "Closure must be a user function",
                loc.as_error_pos(),
            ));
        };
        let total_params = cl.captures.len() + args.len();
        if total_params != func.params_count {
            return Err(JitError::runtime(
                format!(
                    "Closure arity mismatch: expected {}, got {}",
                    func.params_count, total_params
                ),
                loc.as_error_pos(),
            ));
        }
        let mut registers = crate::vm::make_registers(func.locals_count);
        for (i, v) in cl.captures.iter().enumerate() {
            registers[i] = *v;
        }
        for (i, v) in args.iter().enumerate() {
            registers[cl.captures.len() + i] = *v;
        }
        crate::vm::execute_bytecode(&func.instructions, ctx, registers, 0)
    }
}
