//! Extension trait that adds heap-dependent string operations to [`Value`].
//!
//! These methods could not live in `ys-core` because they require access to
//! the runtime [`Context`] and its heap.  They are provided here as a trait
//! so callers can write `val.with_str(&ctx, |s| ...)` just as before.

use ys_core::compiler::Value;
use crate::context::Context;

/// Heap-dependent helpers on [`Value`].
pub trait ValueExt {
    /// Call `f` with the string content of this value, if it is a string.
    ///
    /// Handles both SSO (inline) strings and heap-allocated strings.
    fn with_str<R>(&self, ctx: &Context, f: impl FnOnce(&str) -> R) -> Option<R>;

    /// Return the string content of this value as an owned `String`, if any.
    fn as_string(&self, ctx: &Context) -> Option<String>;
}

impl ValueExt for Value {
    fn with_str<R>(&self, ctx: &Context, f: impl FnOnce(&str) -> R) -> Option<R> {
        use ys_core::compiler::{QNAN, TAG_MASK, TAG_POOL};
        let bits = self.to_bits();
        let tag  = (bits & TAG_MASK) >> 48;

        // SSO strings require the QNAN bit to be set (NaN-boxing).  Normal
        // numbers can have tag bits 3-9 purely from their exponent/mantissa
        // — we must check QNAN to avoid misinterpreting them as SSO strings.
        if (3..=9).contains(&tag) && (bits & QNAN) == QNAN {
            // SSO inline string — decode without heap access.
            let len = (tag - 3) as usize;
            let mut bytes = [0u8; 6];
            for (i, byte) in bytes.iter_mut().enumerate().take(len) {
                *byte = ((bits >> (i * 8)) & 0xFF) as u8;
            }
            return std::str::from_utf8(&bytes[..len]).ok().map(f);
        }

        // Pool strings (compile-time interned) — use a different tag from
        // heap objects so their IDs never collide with runtime allocations.
        if (bits & (TAG_MASK)) == TAG_POOL {
            let id = (bits & 0xFFFFFFFF) as usize;
            if id < ctx.string_pool.len() {
                return Some(f(&ctx.string_pool[id]));
            }
        }

        if let Some(oid) = self.as_obj_id() {
            {
                let heap = ctx.heap.objects.get();
                if let Some(Some(obj)) = heap.get(oid as usize)
                    && let crate::heap::ManagedObject::String(s) = &obj.obj
                {
                    return Some(f(s.as_ref()));
                }
            }
        }

        None
    }

    #[inline]
    fn as_string(&self, ctx: &Context) -> Option<String> {
        self.with_str(ctx, |s| s.to_string())
    }
}
