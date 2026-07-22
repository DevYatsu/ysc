//! Native built-in function registry.
//!
//! Collects all native functions into a single hash map keyed by name.
//! Each sub-module owns one logical group of built-ins.

pub mod collections;
pub mod io;
pub mod list_ops;
#[cfg(any(feature = "networking", target_arch = "wasm32"))]
pub mod net;
pub mod number_ops;
pub mod object_ops;
pub mod string_ops;
pub mod time;

use crate::context::{NativeCtx, NativeFn};
use crate::heap::ManagedObject;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ys_core::compiler::Value;
use ys_core::error::JitError;

/// A registry for native functions. Native function names are always
/// `&'static str` literals, so the map uses that as the key type —
/// the `String` conversion happens once at merge time when the registry
/// is consumed into the unified `callables_by_name` map.
pub(crate) struct NativeRegistry {
    fns: FxHashMap<&'static str, NativeFn>,
}

impl NativeRegistry {
    pub fn new() -> Self {
        Self {
            fns: FxHashMap::default(),
        }
    }

    /// Register a native function under `name`.
    /// Accepts any closure or function pointer matching the `NativeFn` signature
    /// `(&Context, &[Value]) -> Result<Value, JitError>` — the `Arc` wrapping
    /// happens here so callers write `reg.insert("name", |ctx, args| ...)` instead
    /// of `Arc::new(|...| ...)`.
    pub fn insert<F>(&mut self, name: &'static str, func: F)
    where
        F: Fn(&NativeCtx<'_>, &[Value]) -> Result<Value, JitError> + Send + Sync + 'static,
    {
        self.fns.insert(name, Arc::new(func));
    }

    /// Consume the registry and return a `String`-keyed map, converting each
    /// `&'static str` name for compatibility with the unified callables map.
    pub fn into_map(self) -> FxHashMap<String, NativeFn> {
        self.fns
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }
}

/// Allocate a string value, preferring inline SSO (≤6 bytes) over a
/// heap-allocated [`ManagedObject::String`].
///
/// Accepts any string-like type (`&str`, `String`, `&String`, etc.)
/// via [`AsRef<str>`] so callers avoid manual `.into()` or `.to_string()`.
pub(crate) fn alloc_string(ctx: &crate::context::Context, s: impl AsRef<str>) -> Value {
    let s_ref = s.as_ref();
    Value::sso(s_ref).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s_ref))))
}

/// Convenience wrapper — same as [`alloc_string`] but takes `&NativeCtx`.
pub(crate) fn alloc_string_native(ctx: &NativeCtx, s: impl AsRef<str>) -> Value {
    alloc_string(ctx.as_inner(), s)
}

/// Populate `fns` with all built-in functions.
pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    let mut reg = NativeRegistry::new();
    io::register(&mut reg);
    collections::register(&mut reg);
    list_ops::register(&mut reg);
    number_ops::register(&mut reg);
    object_ops::register(&mut reg);
    string_ops::register(&mut reg);
    time::register(&mut reg);
    #[cfg(any(feature = "networking", target_arch = "wasm32"))]
    net::register(&mut reg);
    *fns = reg.into_map();
}
