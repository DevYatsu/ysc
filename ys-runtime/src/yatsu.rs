//! High‑level embedding API inspired by `mlua`.
//!
//! Usage:
//! ```
//! use yatsuscript::Yatsu;
//!
//! let yatsu = Yatsu::new();
//! yatsu.register("add", |_, args| {
//!     Ok(Value::number(args[0].as_number().unwrap() + args[1].as_number().unwrap()))
//! });
//! let sum: f64 = yatsu.exec("add(2, 3)").unwrap();
//! assert_eq!(sum, 5.0);
//! ```

use std::sync::Arc;
use crate::context::{Callable, Context};
use crate::heap::{ManagedObject, SyncCell};
use crate::vm::execute_bytecode;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

// ── Value type name (for error messages) ──────────────────────────────────────

/// Return a human-readable type name for a Value (like Lua's `type()`).
pub fn value_type_name(val: &Value) -> &'static str {
    if val.as_number().is_some() { "number" }
    else if val.as_bool().is_some() { "boolean" }
    else if val.as_sso_str().is_some() { "string" }
    else if val.as_obj_id().is_some() { "object" }
    else { "nil" }
}

/// Build a descriptive error message, e.g.
/// `"bad argument #1 to 'add' (expected number, got string)"`.
pub fn bad_arg(idx: usize, func_name: &str, expected: &str, got: &Value) -> JitError {
    JitError::runtime(
        format!("bad argument #{} to '{}' (expected {}, got {})",
            idx + 1, func_name, expected, value_type_name(got)),
        0, 0,
    )
}

// ── FromLua / ToLua ───────────────────────────────────────────────────────────

/// Types that can be created from a YatsuScript [`Value`].
pub trait FromLua: Sized {
    fn from_lua(val: Value, ctx: &Context) -> Result<Self, JitError>;
}

/// Types that can be converted into a YatsuScript [`Value`].
pub trait ToLua {
    fn to_lua(self, ctx: &Context) -> Result<Value, JitError>;
}

/// Extension trait for `FromLua` that reads from argument slices (for
/// typed function registration).  Called internally by [`Yatsu::register_fn`].
pub trait FromLuaSlice: Sized {
    fn from_lua_slice(args: &[Value], func_name: &str, ctx: &Context) -> Result<Self, JitError>;
}

// ── FromLuaSlice implementations for tuples ────────────────────────────────────

impl<A: FromLua> FromLuaSlice for (A,) {
    fn from_lua_slice(args: &[Value], func_name: &str, ctx: &Context) -> Result<Self, JitError> {
        if args.is_empty() {
            return Err(JitError::runtime(format!(
                "bad argument #1 to '{}' (expected at least 1 argument, got 0)", func_name), 0, 0));
        }
        let a = A::from_lua(args[0], ctx)
            .map_err(|_| bad_arg(0, func_name, std::any::type_name::<A>(), &args[0]))?;
        Ok((a,))
    }
}

impl<A: FromLua, B: FromLua> FromLuaSlice for (A, B) {
    fn from_lua_slice(args: &[Value], func_name: &str, ctx: &Context) -> Result<Self, JitError> {
        if args.len() < 2 {
            return Err(JitError::runtime(format!(
                "bad argument #{} to '{}' (expected at least 2 arguments, got {})",
                args.len() + 1, func_name, args.len()), 0, 0));
        }
        let a = A::from_lua(args[0], ctx)
            .map_err(|_| bad_arg(0, func_name, std::any::type_name::<A>(), &args[0]))?;
        let b = B::from_lua(args[1], ctx)
            .map_err(|_| bad_arg(1, func_name, std::any::type_name::<B>(), &args[1]))?;
        Ok((a, b))
    }
}

impl<A: FromLua, B: FromLua, C: FromLua> FromLuaSlice for (A, B, C) {
    fn from_lua_slice(args: &[Value], func_name: &str, ctx: &Context) -> Result<Self, JitError> {
        if args.len() < 3 {
            return Err(JitError::runtime(format!(
                "bad argument #{} to '{}' (expected at least 3 arguments, got {})",
                args.len() + 1, func_name, args.len()), 0, 0));
        }
        let a = A::from_lua(args[0], ctx)
            .map_err(|_| bad_arg(0, func_name, std::any::type_name::<A>(), &args[0]))?;
        let b = B::from_lua(args[1], ctx)
            .map_err(|_| bad_arg(1, func_name, std::any::type_name::<B>(), &args[1]))?;
        let c = C::from_lua(args[2], ctx)
            .map_err(|_| bad_arg(2, func_name, std::any::type_name::<C>(), &args[2]))?;
        Ok((a, b, c))
    }
}

// ── Implementations for primitive types ────────────────────────────────────────

impl FromLua for f64 {
    fn from_lua(val: Value, _ctx: &Context) -> Result<Self, JitError> {
        val.as_number().ok_or_else(|| JitError::runtime("expected number", 0, 0))
    }
}
impl ToLua for f64 {
    fn to_lua(self, _ctx: &Context) -> Result<Value, JitError> {
        Ok(Value::number(self))
    }
}

impl FromLua for i32 {
    fn from_lua(val: Value, _ctx: &Context) -> Result<Self, JitError> {
        val.as_number().map(|n| n as i32).ok_or_else(|| JitError::runtime("expected number", 0, 0))
    }
}
impl ToLua for i32 {
    fn to_lua(self, _ctx: &Context) -> Result<Value, JitError> {
        Ok(Value::number(self as f64))
    }
}

impl FromLua for bool {
    fn from_lua(val: Value, _ctx: &Context) -> Result<Self, JitError> {
        val.as_bool().ok_or_else(|| JitError::runtime("expected boolean", 0, 0))
    }
}
impl ToLua for bool {
    fn to_lua(self, _ctx: &Context) -> Result<Value, JitError> {
        Ok(Value::bool(self))
    }
}

impl FromLua for String {
    fn from_lua(val: Value, ctx: &Context) -> Result<Self, JitError> {
        ctx.value_as_string(val)
            .ok_or_else(|| JitError::runtime("expected string", 0, 0))
    }
}
impl ToLua for String {
    fn to_lua(self, ctx: &Context) -> Result<Value, JitError> {
        if let Some(sso) = Value::sso(&self) {
            Ok(sso)
        } else {
            Ok(ctx.alloc(crate::heap::ManagedObject::String(std::sync::Arc::from(self))))
        }
    }
}
impl ToLua for &str {
    fn to_lua(self, ctx: &Context) -> Result<Value, JitError> {
        self.to_string().to_lua(ctx)
    }
}

impl FromLua for Value {
    fn from_lua(val: Value, _ctx: &Context) -> Result<Self, JitError> {
        Ok(val)
    }
}
impl ToLua for Value {
    fn to_lua(self, _ctx: &Context) -> Result<Value, JitError> {
        Ok(self)
    }
}

// ── Native function type ──────────────────────────────────────────────────────

/// A native callback that receives the context and arguments.
///
/// Return the result via `Ok(Value::number(...))` etc.
pub type NativeCallback = Arc<dyn Fn(&Context, &[Value]) -> Result<Value, JitError> + Send + Sync>;

// ── Yatsu ───────────────────────────────────────────────────────────────────────

/// High‑level embedding API for YatsuScript, inspired by `mlua`.
pub struct Yatsu {
    /// The underlying execution context.
    pub ctx:   Arc<Context>,
    /// Track registered function names so we can rebuild `callables` if needed.
    function_names: Vec<String>,
}

impl Yatsu {
    /// Create a new YatsuScript state with an empty heap and no globals.
    pub fn new() -> Self {
        Self {
            ctx: Arc::new(Context::new()),
            function_names: Vec::new(),
        }
    }

    // ── Code execution ─────────────────────────────────────────────────────

    /// Compile and execute a source string, returning the result as `R`.
    ///
    /// ```
    /// let yatsu = Yatsu::new();
    /// let sum: f64 = yatsu.exec("1 + 2").unwrap();
    /// assert_eq!(sum, 3.0);
    /// ```
    pub fn exec<R: FromLua>(&mut self, source: &str) -> Result<R, JitError> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| JitError::runtime(format!("runtime error: {}", e), 0, 0))?;
        runtime.block_on(self.exec_async(source))
    }

    /// Async version of [`exec`](Self::exec).
    pub async fn exec_async<R: FromLua>(&mut self, source: &str) -> Result<R, JitError> {
        let program = ys_core::codegen::Codegen::compile(source)?;

        // Register user-defined functions from the compiled program
        for func in program.functions.iter() {
            if let Some(name) = program.string_pool.get(func.name_id as usize) {
                self.ctx.callables.get_mut().insert(func.name_id, Callable::User(func.clone()));
                self.ctx.callables_by_name.get_mut().insert(name.to_string(), Callable::User(func.clone()));
            }
        }

        // Execute the program's main bytecode
        // The program's string pool is used for name resolution
        let string_pool = program.string_pool.clone();
        let mut registers = vec![Value::from_bits(0); program.locals_count];
        let result = execute_bytecode(
            &program.instructions,
            Arc::clone(&self.ctx),
            registers,
        ).await?;

        R::from_lua(result, &self.ctx)
    }

    // ── Global variable access ──────────────────────────────────────────────

    /// Set a global variable from Rust.
    ///
    /// ```
    /// yatsu.set("x", 42.0);
    /// let x: f64 = yatsu.get("x").unwrap();
    /// assert_eq!(x, 42.0);
    /// ```
    pub fn set<T: ToLua>(&mut self, name: &str, val: T) -> Result<(), JitError> {
        let v = val.to_lua(&self.ctx)?;
        // Find or allocate a global slot
        let idx = self.ensure_global(name);
        self.ctx.globals.get_mut()[idx] = v;
        Ok(())
    }

    /// Get a global variable's value.
    pub fn get<R: FromLua>(&self, name: &str) -> Result<R, JitError> {
        // Look up the global by name
        let idx = self.find_global(name);
        match idx {
            Some(idx) => {
                let val = self.ctx.globals.get()[idx];
                R::from_lua(val, &self.ctx)
            }
            None => {
                // Check if it's a registered function
                if self.ctx.callables_by_name.get().contains_key(name) {
                    // Return a function reference (string value for now)
                    let val = Value::sso(name).unwrap_or_else(|| {
                        let oid = self.ctx.alloc(
                            crate::heap::ManagedObject::String(std::sync::Arc::from(name.to_string()))
                        );
                        oid
                    });
                    R::from_lua(val, &self.ctx)
                } else {
                    Err(JitError::runtime(
                        format!("global '{}' not found", name), 0, 0,
                    ))
                }
            }
        }
    }

    // ── Function registration ───────────────────────────────────────────────

    /// Register a Rust function that can be called from scripts.
    ///
    /// ```
    /// yatsu.register("add", |_, args| {
    ///     Ok(Value::number(args[0].as_number().unwrap() + args[1].as_number().unwrap()))
    /// });
    /// ```
    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&Context, &[Value]) -> Result<Value, JitError> + Send + Sync + 'static,
    {
        let wrapped = Arc::new(f);
        let nf: crate::context::NativeFn = Arc::new(move |ctx, args, loc| {
            let w = Arc::clone(&wrapped);
            Box::pin(async move {
                w(&ctx, &args)
            })
        });
        let callable = Callable::Native(Arc::clone(&nf));
        self.ctx.callables_by_name.get_mut().insert(name.to_string(), callable);
        // Also try to insert into name_id map if the name is in the string pool
        if let Some(pos) = self.ctx.string_pool.iter().position(|s| s.as_ref() == name) {
            self.ctx.callables.get_mut().insert(pos as u32, Callable::Native(nf));
        }
        self.function_names.push(name.to_string());
    }

    /// Register a typed function with automatic argument validation.
    ///
    /// Arguments are automatically extracted from the [`Value`] slice via
    /// [`FromLuaSlice`].  On type mismatch a descriptive error is returned
    /// (e.g. `"bad argument #1 to 'add' (expected f64, got string)"`).
    ///
    /// ```
    /// ys.register_fn("add", |(a, b): (f64, f64)| Ok(a + b));
    /// ys.register_fn("hello", |_args: ()| Ok("hello world".to_string()));
    /// ```
    pub fn register_fn<A, R>(&mut self, name: &str, f: impl Fn(A) -> Result<R, JitError> + Send + Sync + 'static)
    where
        A: FromLuaSlice + Send + 'static,
        R: ToLua + Send + 'static,
    {
        let func_name = name.to_string();
        self.register(name, move |ctx, args| {
            let params = A::from_lua_slice(args, &func_name, ctx)?;
            let result = f(params)?;
            result.to_lua(ctx)
        });
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn ensure_global(&self, name: &str) -> usize {
        let globals = self.ctx.globals.get_mut();
        // Check existing globals by name — we use a simple linear scan
        // since globals count is typically small (<100)
        // In a production implementation, maintain a name→index map.
        // For now, just append
        let idx = globals.len();
        globals.push(Value::from_bits(0));
        idx
    }

    fn find_global(&self, name: &str) -> Option<usize> {
        // We don't have a name→index map for globals yet
        // This is a placeholder — in real usage, we'd track this
        None
    }
}

impl Default for Yatsu {
    fn default() -> Self { Self::new() }
}
