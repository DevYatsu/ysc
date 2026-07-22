//! Function dispatch, register management, and call-location tracking.
//!
//! Provides:
//! - [`REG_POOL`] for recycling register vectors across calls.
//! - [`dispatch_callable`] for invoking native or user-defined functions.
//! - [`make_registers`], [`build_call_registers`], [`build_closure_registers`], [`pool_regs`].
//! - [`CALL_LOC`] for annotating `print()` output with source locations.

use crate::context::{Callable, Context, NativeCtx};
use crate::heap::ManagedObject;
use crate::vm::frame::{CallFrame, InstrPtr, ReturnTarget};
use std::cell::{Cell, UnsafeCell};
use std::sync::Arc;
use ys_core::compiler::{Loc, Value};
use ys_core::error::JitError;

// ─────────────────────────────────────────────────────────────────────────────
//  Register pool — raw TLS access (no RefCell borrow-check overhead)
// ─────────────────────────────────────────────────────────────────────────────

thread_local! {
    static REG_POOL: UnsafeCell<Vec<Vec<Value>>> = const { UnsafeCell::new(Vec::new()) };
}

#[inline(always)]
fn with_reg_pool<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<Vec<Value>>) -> R,
{
    REG_POOL.with(|cell| f(unsafe { &mut *cell.get() }))
}

const _: () = assert!(std::mem::size_of::<Value>() == 8);

// ─────────────────────────────────────────────────────────────────────────────
//  Public helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Allocate a zero-initialised register array of `count` slots.
///
/// Tries to reuse a pooled `Vec` before allocating a new one.
pub(crate) fn make_registers(count: usize) -> Vec<Value> {
    if let Some(regs) = with_reg_pool(|pool| pool.pop())
        && regs.len() == count
    {
        return regs;
    }
    vec![Value::nil(); count]
}

/// Build a register array pre-populated with call arguments.
///
/// Tries to reuse a pooled `Vec` before allocating a new one.
pub fn build_call_registers(locals: usize, args_regs: &[usize], caller: &[Value]) -> Vec<Value> {
    if let Some(mut regs) = with_reg_pool(|pool| pool.pop())
        && regs.len() == locals
    {
        let args = args_regs.len().min(locals);
        for (i, &r) in args_regs.iter().enumerate().take(args) {
            regs[i] = unsafe { *caller.get_unchecked(r) };
        }
        for v in regs[args..].iter_mut() {
            *v = Value::nil();
        }
        return regs;
    }
    let mut regs: Vec<Value> = vec![Value::nil(); locals];
    for (i, &r) in args_regs.iter().enumerate().take(locals) {
        regs[i] = unsafe { *caller.get_unchecked(r) };
    }
    regs
}

/// Build a register array pre-populated with captures followed by call arguments.
///
/// Tries to reuse a pooled `Vec` before allocating a new one.
pub fn build_closure_registers(
    locals: usize,
    captures: &[Value],
    args_regs: &[usize],
    caller: &[Value],
) -> Vec<Value> {
    if let Some(mut regs) = with_reg_pool(|pool| pool.pop())
        && regs.len() == locals
    {
        // Only zero remaining registers after captures + args
        let filled = (captures.len() + args_regs.len()).min(locals);
        for (i, v) in captures.iter().enumerate().take(locals) {
            regs[i] = *v;
        }
        for (i, &r) in args_regs
            .iter()
            .enumerate()
            .take(locals.saturating_sub(captures.len()))
        {
            regs[captures.len() + i] = unsafe { *caller.get_unchecked(r) };
        }
        for v in regs[filled..].iter_mut() {
            *v = Value::nil();
        }
        return regs;
    }
    let mut regs: Vec<Value> = vec![Value::nil(); locals];
    for (i, v) in captures.iter().enumerate().take(locals) {
        regs[i] = *v;
    }
    for (i, &r) in args_regs
        .iter()
        .enumerate()
        .take(locals.saturating_sub(captures.len()))
    {
        regs[captures.len() + i] = unsafe { *caller.get_unchecked(r) };
    }
    regs
}

/// Return a register vector to the pool for reuse.
///
/// Only vectors of length ≤ 64 are kept, with a cap of 100 pooled vectors.
pub fn pool_regs(regs: Vec<Value>) {
    with_reg_pool(|pool| {
        if regs.len() <= 64 && pool.len() < 100 {
            pool.push(regs);
        }
    });
}

/// Apply rest-param logic after building the initial registers: collect
/// remaining positional args into a `List` and store at `rest_at`.
pub fn apply_rest(
    ctx: &Context,
    regs: &mut [Value],
    args_regs: &[usize],
    caller: &[Value],
    rest_at: usize,
) {
    let rest_values: Vec<Value> = if rest_at < args_regs.len() {
        args_regs[rest_at..]
            .iter()
            .map(|&r| unsafe { *caller.get_unchecked(r) })
            .collect()
    } else {
        Vec::new()
    };
    regs[rest_at] = ctx.alloc(ManagedObject::List(rest_values));
}

// ─────────────────────────────────────────────────────────────────────────────
//  Call-location tracking
// ─────────────────────────────────────────────────────────────────────────────

// Set before calling a native function so the function (e.g. print) can
// annotate its output with the source line number.
std::thread_local! {
    static CALL_LOC: Cell<Option<(u32, u32)>> = const { Cell::new(None) };
}

pub(crate) fn set_call_loc(line: u32, col: u32) {
    CALL_LOC.with(|loc| loc.set(Some((line, col))));
}

/// Read the call location set by the most recent [`set_call_loc`] call.
/// Used by wasm32 `print()` to annotate output with source locations.
pub(crate) fn get_call_loc() -> Option<(u32, u32)> {
    CALL_LOC.with(|loc| loc.get())
}

// ─────────────────────────────────────────────────────────────────────────────
//  Dispatch
// ─────────────────────────────────────────────────────────────────────────────

/// Dispatch a resolved [`Callable`] — calls a native function or pushes a
/// new call frame for a user-defined function.
pub fn dispatch_callable(
    frames: &mut Vec<CallFrame>,
    ctx: &Context,
    callable: &Callable,
    args_regs: &Arc<[usize]>,
    dst: Option<usize>,
    loc: Loc,
) -> Result<(), JitError> {
    let fi = frames.len() - 1;
    match callable {
        Callable::Native(nf) => {
            // Avoid heap-allocating a Vec for small argument counts.
            // Most native functions take 0–4 args.
            let res = if args_regs.len() <= 8 {
                let mut buf = [Value::nil(); 8];
                for (i, &r) in args_regs.iter().enumerate() {
                    buf[i] = unsafe { *frames[fi].registers.get_unchecked(r) };
                }
                nf(&NativeCtx::new(ctx), &buf[..args_regs.len()])
            } else {
                let args: Vec<Value> = args_regs.iter().map(|&r| frames[fi].registers[r]).collect();
                nf(&NativeCtx::new(ctx), &args)
            }?;
            if let Some(d) = dst {
                frames[fi].registers[d] = res;
            }
        }
        Callable::User(f) => {
            let min_args = f.rest_at.unwrap_or(f.params_count);
            if args_regs.len() < min_args || (f.rest_at.is_none() && args_regs.len() != f.params_count)
            {
                return Err(JitError::runtime(
                    format!(
                        "Function arity mismatch: expected {}, got {}",
                        f.params_count,
                        args_regs.len()
                    ),
                    loc.as_error_pos(),
                ));
            }
            let ret = dst.map(|d| ReturnTarget { dst: d });
            let mut callee_regs =
                build_call_registers(f.locals_count, args_regs, &frames[fi].registers);
            // If there's a rest param, collect remaining args into a list.
            if let Some(rest_at) = f.rest_at {
                apply_rest(ctx, &mut callee_regs, args_regs, &frames[fi].registers, rest_at);
            }
            frames.push(CallFrame {
                instructions: InstrPtr::from_arc(&f.instructions),
                func_name_id: None,
                registers: callee_regs,
                pc: 0,
                return_to: ret,
                obj_cache: Vec::with_capacity(4),
            });
        }
    }
    Ok(())
}
