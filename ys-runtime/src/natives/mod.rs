//! Native built-in function registry.
//!
//! Collects all native functions into a single hash map keyed by name.
//! Each sub-module owns one logical group of built-ins.

pub mod collections;
pub mod io;
pub mod list_ops;
pub mod net;
pub mod time;

use crate::context::NativeFn;
use rustc_hash::FxHashMap;

/// Populate `fns` with all built-in functions.
pub fn register(fns: &mut FxHashMap<String, NativeFn>) {
    io::register(fns);
    collections::register(fns);
    list_ops::register(fns);
    time::register(fns);
    net::register(fns);
}
