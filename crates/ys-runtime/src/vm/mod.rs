//! The register-based virtual machine execution loop.
//!
//! [`execute_bytecode`] is the core dispatch loop. It is called recursively
//! for function calls (via a frame stack).
//!
//! ## Design notes
//! - Register arrays are `Vec<Value>` — each frame owns its registers.
//! - The frame stack is a plain `Vec<CallFrame>`.
//!   Using a `Vec` and popping frames avoids indirect-recursion and keeps
//!   stack depth constant from Rust's perspective.

pub mod collections;
pub mod dispatch;
pub mod frame;
pub mod methods;
pub mod promise;
pub mod setup;

use crate::context::{Callable, Context, NativeCtx};
use crate::heap::{ManagedObject, ObjectData};
use crate::value_ext::ValueExt;
use crate::value_fmt::stringify_value;
use std::sync::Arc;
use ys_core::compiler::{Instruction, QNAN, TAG_FAILURE, Value};
use ys_core::error::JitError;

pub use setup::run_interpreter;

//  Re-exports for cross-module visibility
pub(crate) use collections::{
    GetResult, handle_list_get, handle_list_set, handle_object_get, handle_object_set,
};
pub(crate) use dispatch::{
    build_call_registers, build_closure_registers, dispatch_callable, make_registers, pool_regs,
    set_call_loc,
};
pub(crate) use frame::{
    CallFrame, FrameState, InstrPtr, ReturnTarget, scan_current_frames, set_current_frames,
};
pub(crate) use promise::{PromiseState, resolve_promise, resolve_promise_value};

//  Interpreter entry point (public)

pub use crate::context::Backend;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: ys_core::compiler::Program) -> Result<(), JitError> {
        run_interpreter(program)
    }
}

//  Internal helpers

/// Insertion sort for numeric values (sequential fallback when rayon is absent).
fn sort_insertion(elems: &mut [Value]) {
    for i in 1..elems.len() {
        let mut j = i;
        while j > 0 {
            let (a, b) = (elems[j - 1].as_number(), elems[j].as_number());
            if let (Some(a), Some(b)) = (a, b) {
                if a <= b {
                    break;
                }
                elems.swap(j - 1, j);
            } else {
                break;
            }
            j -= 1;
        }
    }
}

//  Hot-path arithmetic macros

macro_rules! numeric_bin {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        // Read each register and extract its bits exactly once.
        let lv  = regs[$l];
        let rv  = regs[$r];
        let lb  = lv.to_bits();
        let rb  = rv.to_bits();
        let f   = QNAN | TAG_FAILURE;
        if (lb & f) == f {
            regs[$dst] = lv;
        } else if (rb & f) == f {
            regs[$dst] = rv;
        } else if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
            regs[$dst] = Value::number(f64::from_bits(lb) $op f64::from_bits(rb));
        } else if let (Some(ln), Some(rn)) = (lv.as_number(), rv.as_number()) {
            regs[$dst] = Value::number(ln $op rn);
        } else {
            return Err(JitError::runtime(
                concat!("Math error: expected numbers for '", stringify!($op), "'"),
                $loc.as_error_pos(),
            ));
        }
    }};
}

macro_rules! compare_op {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        let lv  = regs[$l];
        let rv  = regs[$r];
        let lb  = lv.to_bits();
        let rb  = rv.to_bits();
        let f   = QNAN | TAG_FAILURE;
        if (lb & f) == f {
            regs[$dst] = lv;
        } else if (rb & f) == f {
            regs[$dst] = rv;
        } else {
            let result = if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                Some(f64::from_bits(lb) $op f64::from_bits(rb))
            } else {
                match (lv.as_number(), rv.as_number()) {
                    (Some(ln), Some(rn)) => Some(ln $op rn),
                    _ => None,
                }
            };
            match result {
                Some(b) => regs[$dst] = Value::bool(b),
                None    => return Err(JitError::runtime(
                    concat!("Compare error: expected numbers for '", stringify!($op), "'"),
                    $loc.as_error_pos(),
                )),
            }
        }
    }};
}

/// Reconstruct an `Arc<[Instruction]>` for a call frame, needed only when
/// creating a `FrameState` continuation (async/generator suspension).
///
/// Named functions look up their instructions from `ctx.callables[name_id]` —
/// no clone on the call path. The top-level frame (`func_name_id == None`)
/// clones from the entry `instructions` parameter.
#[inline(never)]
fn frame_instr_arc(
    fr: &CallFrame,
    entry_instructions: &Arc<[Instruction]>,
    ctx: &Context,
) -> Arc<[Instruction]> {
    match fr.func_name_id {
        Some(nid) => {
            let callables = ctx.callables.get();
            match callables.get(nid as usize).and_then(|c| c.as_ref()) {
                Some(Callable::User(f)) => f.instructions.clone(),
                _ => unreachable!("Function call must exist for async continuation"),
            }
        }
        None => entry_instructions.clone(),
    }
}

//  Main dispatch loop

pub fn execute_bytecode(
    instructions: &Arc<[Instruction]>,
    ctx: &Context,
    registers: Vec<Value>,
    start_pc: usize,
) -> Result<Value, JitError> {
    //  Frame stack
        let mut frames = Vec::with_capacity(256);
        frames.push(CallFrame {
            instructions: InstrPtr::from_arc(instructions),
            func_name_id: None,
            registers,
            pc: start_pc,
            return_to: None,
            obj_cache: Vec::with_capacity(4),
        });
        set_current_frames(&frames);

    loop {
        let fi = match frames.len() {
            0 => return Ok(Value::nil()),
            n => n - 1,
        };

        // Copy instr_ptr and pc by value to avoid borrowing frames.
        // One bounds-check covers both field reads.
        let (instr_ptr, pc) = {
            let fr = &frames[fi];
            (fr.instructions, fr.pc)
        };

        // Implicit return when the current function's PC is past its
        // last instruction.  Most functions end with a Return, but
        // top-level code (and some closures) may fall through.
        //
        // The explicit bounds check lets us use `get_unchecked` on the
        // fetch immediately below, saving the second bounds check.
        let instr_len = instr_ptr.slice().len();
        if pc >= instr_len {
            let frame = frames.pop().unwrap();
            let ret_val = Value::nil();
            pool_regs(frame.registers);
            if let Some(t) = frame.return_to {
                frames[fi].registers[t.dst] = ret_val;
                continue;
            }
            return Ok(ret_val);
        }

        let instr = unsafe { instr_ptr.slice().get_unchecked(pc) };

        // ── Hot-path peel ────────────────────────────────────────────────
        // The most frequent instructions in tight loops are checked here
        // before falling through to the full match.
        match instr {
            Instruction::Increment(reg) => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*reg];
                if (v.to_bits() & (QNAN | TAG_FAILURE)) != (QNAN | TAG_FAILURE) {
                    if let Some(n) = v.as_number() { fr[*reg] = Value::number(n + 1.0); }
                }
                fr.advance();
                continue;
            }
            Instruction::Jump(target) => { frames[fi].jump_to(*target); continue; }
            Instruction::JumpIfNotLess { var, end, target } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                if let (Some(vn), Some(en)) = (fr[*var].as_number(), fr[*end].as_number()) {
                    if vn >= en { fr.jump_to(*target); continue; }
                }
                fr.advance();
                continue;
            }
            _ => {}
        }

        match instr {
            //  Memory
            Instruction::LoadLiteral { dst, val, .. } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                fr[*dst] = *val;
                fr.advance();
            }
            Instruction::Move { dst, src } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let d = *dst;
                let v = fr[*src];
                fr[d] = v;
                fr.advance();
            }
            Instruction::LoadGlobal { dst, global } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                fr[*dst] = ctx.globals.get()[*global];
                fr.advance();
            }
            Instruction::StoreGlobal { global, src } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                ctx.globals.get_mut()[*global] = fr[*src];
                fr.advance();
            }

            //  Control flow
            Instruction::Jump(target) => {
                frames[fi].jump_to(*target);
                continue;
            }
            Instruction::JumpIfNotLess { var, end, target } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*var];
                let e = fr[*end];
                if let (Some(vn), Some(en)) = (v.as_number(), e.as_number())
                    && vn >= en {
                        fr.jump_to(*target);
                        continue;
                    }
                fr.advance();
            }
            Instruction::JumpIfFalse { cond, target } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                if !fr[*cond].is_truthy() {
                    fr.jump_to(*target);
                } else {
                    fr.advance();
                }
                continue;
            }
            Instruction::JumpIfNotFail { src, target } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let val = fr[*src];
                if (val.to_bits() & (QNAN | TAG_FAILURE)) != (QNAN | TAG_FAILURE) {
                    fr.jump_to(*target);
                } else {
                    fr.advance();
                }
                continue;
            }
            Instruction::Return { value: val_reg, .. } => {
                let ret = val_reg.map_or(Value::nil(), |r| frames[fi][r]);
                let frame = frames.pop().unwrap();
                pool_regs(frame.registers);
                if let Some(t) = frame.return_to {
                    // After the pop, fi is stale. Recompute caller index.
                    let ci = frames.len() - 1;
                    frames[ci].registers[t.dst] = ret;
                    continue;
                }
                return Ok(ret);
            }

            Instruction::Yield {
                dst: _dst,
                value,
                gen_reg,
                loc: _loc,
            } => {
                let _yielded = frames[fi][*value];
                let gen_val = frames[fi][*gen_reg];
                let save_pc = frames[fi].pc;
                let cont = Box::new(FrameState {
                    instructions: frame_instr_arc(&frames[fi], instructions, ctx),
                    registers: std::mem::take(&mut frames[fi].registers),
                    pc: save_pc,
                    return_to: frames[fi].return_to,
                });
                // Attach continuation to the generator promise
                if let Some(goid) = gen_val.as_obj_id() {
                    let hl = ctx.heap.objects.get_mut();
                    if let Some(Some(sl)) = hl.get_mut(goid as usize) {
                        sl.obj = ManagedObject::Promise(PromiseState::Pending {
                            continuation: Some(cont),
                        });
                    }
                }
                // Pop frame and return generator to caller
                let fr = frames.pop().unwrap();
                if let Some(rt) = fr.return_to {
                    let ci = frames.len() - 1;
                    frames[ci].registers[rt.dst] = gen_val;
                    frames[ci].advance();
                } else {
                    return Ok(gen_val);
                }
                continue;
            }

            Instruction::Await {
                dst,
                promise,
                loc: _loc,
            } => {
                let pv = frames[fi][*promise];

                // Case 1: awaiting a List — resolve each element in parallel.
                if let Some(oid) = pv.as_obj_id() {
                    let (len, elems_copy) = {
                        let objects = ctx.heap.objects.get();
                        match objects.get(oid as usize).and_then(|o| o.as_ref()) {
                            Some(o) if matches!(o.obj, ManagedObject::List(_)) => {
                                let elems = match &o.obj {
                                    ManagedObject::List(e) => e,
                                    _ => unreachable!(),
                                };
                                (elems.len(), elems.clone()) // clone once
                            }
                            _ => (0, Vec::new()),
                        }
                    };
                    if len > 0 || !elems_copy.is_empty() {
                        let mut results: Vec<Value> = Vec::with_capacity(len);
                        let mut sub_promises: Vec<Option<u32>> = Vec::with_capacity(len);
                        let mut all_ready = true;
                        for elem in elems_copy.iter() {
                            match resolve_promise_value(ctx, *elem) {
                                Ok(val) => {
                                    results.push(val);
                                    sub_promises.push(None);
                                }
                                Err(_) => {
                                    all_ready = false;
                                    if let Some(sub_oid) = elem.as_obj_id() {
                                        sub_promises.push(Some(sub_oid));
                                    } else {
                                        sub_promises.push(None);
                                    }
                                    results.push(Value::nil()); // placeholder
                                }
                            }
                        }
                        if all_ready {
                            frames[fi][*dst] = ctx.alloc(ManagedObject::List(results));
                            frames[fi].advance();
                            continue;
                        }
                        // Parallel: create a compound promise
                        let saved_pc = frames[fi].pc;
                        let mut frame = frames.pop().unwrap();
                        let compound_val =
                            ctx.alloc(ManagedObject::Promise(PromiseState::Compound {
                                sub_promises,
                                results,
                                continuation: Some(Box::new(FrameState {
                                    instructions: frame_instr_arc(&frame, instructions, ctx),
                                    registers: std::mem::take(&mut frame.registers),
                                    pc: saved_pc,
                                    return_to: frame.return_to,
                                })),
                            }));
                        if let Some(ret) = &frame.return_to {
                            let ci = frames.len() - 1;
                            frames[ci][ret.dst] = compound_val;
                            frames[ci].advance();
                        } else {
                            ctx.pending_tasks.get_mut().push(compound_val);
                        }
                        continue;
                    }
                }

                // Case 2: single Promise or pass-through value.
                let maybe_oid = pv.as_obj_id();
                if let Some(oid) = maybe_oid.filter(|&oid| {
                    let objects = ctx.heap.objects.get();
                    objects
                        .get(oid as usize)
                        .and_then(|o| o.as_ref())
                        .is_some_and(|obj| matches!(obj.obj, ManagedObject::Promise(_)))
                }) {
                    match resolve_promise(ctx, oid) {
                        Ok(val) => {
                            frames[fi][*dst] = val;
                            frames[fi].advance();
                        }
                        Err(Some(name_id)) => {
                            frames[fi][*dst] = Value::failure(name_id);
                            frames[fi].advance();
                            continue;
                        }
                        Err(None) => {
                            // Pending — attach our continuation to the existing promise
                            // so the event loop can resume us when the underlying operation completes.
                            let saved_pc = frames[fi].pc;
                            let continuation = Box::new(FrameState {
                                instructions: frame_instr_arc(&frames[fi], instructions, ctx),
                                registers: std::mem::take(&mut frames[fi].registers),
                                pc: saved_pc,
                                return_to: frames[fi].return_to,
                            });
                            // Attach to the promise object on the heap
                            {
                                let objs = ctx.heap.objects.get_mut();
                                if let Some(Some(slot)) = objs.get_mut(oid as usize)
                                    && let ManagedObject::Promise(PromiseState::Pending {
                                        continuation: c,
                                    }) = &mut slot.obj
                                    {
                                        *c = Some(continuation);
                                    }
                            }
                            let frame = frames.pop().unwrap();
                            let ci = frames.len() - 1;
                            if let Some(ret) = &frame.return_to {
                                // Return the async function's ret_promise (from
                                // MakePendingPromise) so the caller awaits OUR
                                // promise, not the inner awaited promise.
                                let ret_val = {
                                    let instrs = frame.instructions.slice();
                                    if let Some(Instruction::MakePendingPromise { dst }) =
                                        instrs.first()
                                    {
                                        frame.registers.get(*dst).copied().unwrap_or(pv)
                                    } else {
                                        pv
                                    }
                                };
                                frames[ci][ret.dst] = ret_val;
                                // Do NOT advance the caller's PC — it was already
                                // advanced by the Call instruction that invoked us.
                            } else {
                                // Top-level await — hand to event loop
                                ctx.pending_tasks.get_mut().push(pv);
                            }
                            continue;
                        }
                    }
                } else {
                    // Non-promise value — pass through directly
                    frames[fi][*dst] = pv;
                    frames[fi].advance();
                }
            }

            //  Arithmetic
            Instruction::AddNum { dst, lhs, rhs, loc } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let lv = fr[*lhs];
                let rv = fr[*rhs];
                let lb = lv.to_bits();
                let rb = rv.to_bits();

                // Fast path: both are plain f64 — no failure or string handling.
                if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                    fr[*dst] = Value::number(f64::from_bits(lb) + f64::from_bits(rb));
                } else if (lb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = lv;
                } else if (rb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = rv;
                } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
                    fr[*dst] = Value::number(lv + rv);
                } else {
                    // String concatenation (including string + number coercion)
                    let s = {
                        let ls = lv.as_string(ctx);
                        let rs = rv.as_string(ctx);
                        match (ls, rs) {
                            (Some(a), Some(b)) => {
                                let mut s = String::with_capacity(a.len() + b.len());
                                s.push_str(&a);
                                s.push_str(&b);
                                s
                            }
                            (Some(a), None) => {
                                let b_str = stringify_value(ctx, rv);
                                let mut s = String::with_capacity(a.len() + b_str.len());
                                s.push_str(&a);
                                s.push_str(&b_str);
                                s
                            }
                            (None, Some(b)) => {
                                let a_str = stringify_value(ctx, lv);
                                let mut s = String::with_capacity(a_str.len() + b.len());
                                s.push_str(&a_str);
                                s.push_str(&b);
                                s
                            }
                            _ => {
                                return Err(JitError::runtime(
                                    "Add error: expected numbers or strings",
                                    loc.as_error_pos(),
                                ));
                            }
                        }
                    };
                    fr[*dst] = Value::sso(&s)
                        .unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))));
                }
                fr.advance();
            }
            Instruction::AddNumFast { dst, lhs, rhs, loc: _ } => {
                // Pure f64 addition — no failure/string checks.
                // Compiler emits this only when both operands are known numeric.
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let lb = fr[*lhs].to_bits();
                let rb = fr[*rhs].to_bits();
                fr[*dst] = Value::number(f64::from_bits(lb) + f64::from_bits(rb));
                fr.advance();
            }
            Instruction::Add { dst, lhs, rhs, loc } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let lv = fr[*lhs];
                let rv = fr[*rhs];
                let lb = lv.to_bits();
                let rb = rv.to_bits();

                // Fast path: both are plain f64 (no NaN-boxing tag).
                // This avoids the failure/string checks for numeric code.
                if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                    fr[*dst] = Value::number(f64::from_bits(lb) + f64::from_bits(rb));
                } else if (lb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = lv;
                } else if (rb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = rv;
                } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
                    fr[*dst] = Value::number(lv + rv);
                } else {
                    // String concatenation (including string + number coercion)
                    let s = {
                        let ls = lv.as_string(ctx);
                        let rs = rv.as_string(ctx);
                        match (ls, rs) {
                            (Some(a), Some(b)) => {
                                let mut s = String::with_capacity(a.len() + b.len());
                                s.push_str(&a);
                                s.push_str(&b);
                                s
                            }
                            (Some(a), None) => {
                                let b_str = stringify_value(ctx, rv);
                                let mut s = String::with_capacity(a.len() + b_str.len());
                                s.push_str(&a);
                                s.push_str(&b_str);
                                s
                            }
                            (None, Some(b)) => {
                                let a_str = stringify_value(ctx, lv);
                                let mut s = String::with_capacity(a_str.len() + b.len());
                                s.push_str(&a_str);
                                s.push_str(&b);
                                s
                            }
                            _ => {
                                return Err(JitError::runtime(
                                    "Add error: expected numbers or strings",
                                    loc.as_error_pos(),
                                ));
                            }
                        }
                    };
                    fr[*dst] = Value::sso(&s)
                        .unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))));
                }
                fr.advance();
            }
            Instruction::Sub { dst, lhs, rhs, loc } => {
                numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, -, *loc);
                frames[fi].advance();
            }
            Instruction::Mul { dst, lhs, rhs, loc } => {
                numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, *, *loc);
                frames[fi].advance();
            }
            Instruction::Div { dst, lhs, rhs, loc } => {
                // Division by zero → produce DivisionByZero failure
                {
                    let fr = unsafe { frames.get_unchecked_mut(fi) };
                    let rv = fr[*rhs];
                    if let Some(n) = rv.as_number()
                        && n == 0.0
                    {
                        let name_id = ctx.pool_id("DivisionByZero").unwrap_or(0);
                        fr[*dst] = Value::failure(name_id);
                        fr.advance();
                        continue;
                    }
                }
                numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, /, *loc);
                frames[fi].advance();
            }
            Instruction::Mod { dst, lhs, rhs, loc } => {
                // Mod by zero → produce ModByZero failure
                {
                    let fr = unsafe { frames.get_unchecked_mut(fi) };
                    let rv = fr[*rhs];
                    if let Some(n) = rv.as_number()
                        && n == 0.0
                    {
                        let name_id = ctx.pool_id("ModByZero").unwrap_or(0);
                        fr[*dst] = Value::failure(name_id);
                        fr.advance();
                        continue;
                    }
                }
                numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, %, *loc);
                frames[fi].advance();
            }
            Instruction::Not { dst, src, .. } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let sv = fr[*src];
                if (sv.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = sv;
                    fr.advance();
                    continue;
                }
                fr[*dst] = Value::bool(!sv.is_truthy());
                fr.advance();
            }
            Instruction::Neg { dst, src, loc } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*src];
                if (v.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    fr[*dst] = v;
                    fr.advance();
                    continue;
                }
                if let Some(n) = v.as_number() {
                    fr[*dst] = Value::number(-n);
                } else {
                    return Err(JitError::runtime(
                        "Negate error: expected a number",
                        loc.as_error_pos(),
                    ));
                }
                fr.advance();
            }
            Instruction::Fail { dst, name_id } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                fr[*dst] = Value::failure(*name_id);
                fr.advance();
            }

            //  Comparisons
            Instruction::Eq { dst, lhs, rhs } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let lv = fr[*lhs];
                let rv = fr[*rhs];
                let lb = lv.to_bits();
                let rb = rv.to_bits();
                let both_plain = (lb & QNAN) != QNAN && (rb & QNAN) != QNAN;
                // For plain f64 values, NaN != NaN (IEEE 754).
                // For NaN-boxed types (SSO strings, objects, bools, etc.),
                // compare bits directly.
                let eq = if both_plain {
                    lb == rb && (lb & QNAN) != QNAN
                } else {
                    lb == rb
                };
                fr[*dst] = Value::bool(eq);
                fr.advance();
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let lv = fr[*lhs];
                let rv = fr[*rhs];
                let lb = lv.to_bits();
                let rb = rv.to_bits();
                let both_plain = (lb & QNAN) != QNAN && (rb & QNAN) != QNAN;
                let ne = if both_plain {
                    lb != rb || (lb & QNAN) == QNAN
                } else {
                    lb != rb
                };
                fr[*dst] = Value::bool(ne);
                fr.advance();
            }
            Instruction::Lt { dst, lhs, rhs, loc } => {
                compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, <, *loc);
                frames[fi].advance();
            }
            Instruction::Le { dst, lhs, rhs, loc } => {
                compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, <=, *loc);
                frames[fi].advance();
            }
            Instruction::Gt { dst, lhs, rhs, loc } => {
                compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, >, *loc);
                frames[fi].advance();
            }
            Instruction::Ge { dst, lhs, rhs, loc } => {
                compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, >=, *loc);
                frames[fi].advance();
            }

            //  Unified ForNext iteration — handles lists, objects, and ranges.
            //  Returns the current element and a "has more" flag.
            Instruction::ForNext {
                dst_val,
                dst_done,
                iterable,
                idx_reg,
                loc: _,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let idx = fr[*idx_reg]
                    .as_number()
                    .map(|n| n.max(0.0) as usize)
                    .unwrap_or(0);
                let iter_val = fr[*iterable];
                let mut has_more = false;
                let mut value = Value::nil();

                if let Some(oid) = iter_val.as_obj_id() {
                    // Object case requires mutable heap access for sorted_keys cache.
                    // Try Object first (rare path vs List/Range).
                    {
                        let objects = ctx.heap.objects.get_mut();
                        if let Some(Some(o)) = objects.get_mut(oid as usize)
                            && let ManagedObject::Object(d) = &mut o.obj
                        {
                            let keys = d.sorted_keys();
                            if idx < keys.len() {
                                value = Value::pool(keys[idx]);
                                has_more = true;
                            }
                        }
                    }
                    if !has_more {
                        let objects = ctx.heap.objects.get();
                        match objects
                            .get(oid as usize)
                            .and_then(|o| o.as_ref())
                            .map(|o| &o.obj)
                        {
                            Some(ManagedObject::List(elems)) => {
                                if idx < elems.len() {
                                    value = elems[idx];
                                    has_more = true;
                                }
                            }
                            Some(ManagedObject::Range { start, end, step }) => {
                                let v = *start + idx as f64 * *step;
                                if (*step > 0.0 && v < *end) || (*step < 0.0 && v > *end) {
                                    value = Value::number(v);
                                    has_more = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                fr[*dst_val] = value;
                fr[*dst_done] = Value::bool(has_more);
                fr[*idx_reg] = Value::number((idx + 1) as f64);
                fr.advance();
            }

            //  Increments / Decrements
            Instruction::Increment(reg) => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*reg];
                if (v.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                    // Failure propagation — leave as-is
                } else if let Some(n) = v.as_number() {
                    fr[*reg] = Value::number(n + 1.0);
                }
                fr.advance();
            }
            Instruction::Decrement(reg) => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*reg];
                if (v.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
                } else if let Some(n) = v.as_number() {
                    fr[*reg] = Value::number(n - 1.0);
                }
                fr.advance();
            }
            Instruction::IncrementGlobal(global) => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = ctx.globals.get()[*global];
                if let Some(n) = v.as_number() {
                    ctx.globals.get_mut()[*global] = Value::number(n + 1.0);
                }
                fr.advance();
            }
            Instruction::DecrementGlobal(global) => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = ctx.globals.get()[*global];
                if let Some(n) = v.as_number() {
                    ctx.globals.get_mut()[*global] = Value::number(n - 1.0);
                }
                fr.advance();
            }

            //  Ranges
            Instruction::Range {
                dst,
                start,
                end,
                step,
                loc,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let s = fr[*start].as_number().ok_or_else(|| {
                    JitError::runtime("Range start must be a number", loc.as_error_pos())
                })?;
                let e = fr[*end].as_number().ok_or_else(|| {
                    JitError::runtime("Range end must be a number", loc.as_error_pos())
                })?;
                let st = if let Some(sr) = *step {
                    fr[sr].as_number().ok_or_else(|| {
                        JitError::runtime("Range step must be a number", loc.as_error_pos())
                    })?
                } else {
                    1.0
                };
                fr[*dst] = ctx.alloc(ManagedObject::Range {
                    start: s,
                    end: e,
                    step: st,
                });
                fr.advance();
            }
            Instruction::RangeInfo {
                range,
                start_dst,
                end_dst,
                step_dst,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let rv = fr[*range];
                let (s, e, st) = if let Some(oid) = rv.as_obj_id() {
                    let heap = ctx.heap.objects.get();
                    let o = unsafe { heap.get_unchecked(oid as usize) };
                    if let Some(o) = o
                        && let ManagedObject::Range { start, end, step } = &o.obj
                    {
                        (*start, *end, *step)
                    } else {
                        (0.0, 0.0, 1.0)
                    }
                } else {
                    (0.0, 0.0, 1.0)
                };
                fr[*start_dst] = Value::number(s);
                fr[*end_dst] = Value::number(e);
                fr[*step_dst] = Value::number(st);
                fr.advance();
            }

            //  Collections
            Instruction::NewList { dst, len } => {
                let elems: Vec<Value> = (0..*len).map(|_| Value::nil()).collect();
                frames[fi][*dst] = ctx.alloc(ManagedObject::List(elems));
                frames[fi].advance();
            }
            Instruction::NewListFrom { dst, elems } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let vals: Vec<Value> = elems.iter().map(|&r| fr[r]).collect();
                fr[*dst] = ctx.alloc(ManagedObject::List(vals));
                fr.advance();
            }
            Instruction::NewListRepeat { dst, val, count } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let v = fr[*val];
                let n = fr[*count]
                    .as_number()
                    .map(|n| n.max(0.0) as usize)
                    .unwrap_or(0);
                let vals = vec![v; n];
                fr[*dst] = ctx.alloc(ManagedObject::List(vals));
                fr.advance();
            }
            Instruction::ListGet {
                dst,
                list,
                index_reg,
                loc,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                match handle_list_get(&fr.registers, *list, *index_reg, ctx, *loc) {
                    GetResult::Value(v) => fr[*dst] = v,
                    GetResult::Error(msg) => {
                        return Err(JitError::runtime(msg, loc.as_error_pos()));
                    }
                    _ => unreachable!(),
                }
                fr.advance();
            }
            Instruction::ListSet {
                list,
                index_reg,
                src,
                loc,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                if let Err(msg) =
                    handle_list_set(&fr.registers, *list, *index_reg, *src, ctx, *loc)
                {
                    return Err(JitError::runtime(msg, loc.as_error_pos()));
                }
                fr.advance();
            }
            Instruction::NewObject { dst, .. } => {
                frames[fi][*dst] = ctx.alloc(ManagedObject::Object(ObjectData::new(
                    rustc_hash::FxHashMap::default(),
                )));
                frames[fi].advance();
            }
            Instruction::NewObjectFrom { dst, fields } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let mut map = rustc_hash::FxHashMap::default();
                map.reserve(fields.len());
                for &(name_id, src) in fields.iter() {
                    map.insert(name_id, fr[src]);
                }
                fr[*dst] = ctx.alloc(ManagedObject::Object(ObjectData::new(map)));
                fr.advance();
            }
            Instruction::ObjectGet {
                dst,
                obj,
                name_id,
                loc,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let obj_reg = fr[*obj];
                // Inline cache: check cached (object_id, name_id) pairs first.
                let cached = obj_reg.as_obj_id().and_then(|oid| {
                    fr.obj_cache.iter().rev().find(|(co, cn, _)| *co == oid && *cn == *name_id)
                        .map(|&(_, _, v)| v)
                });
                let val = match cached {
                    Some(v) => v,
                    None => match handle_object_get(&fr.registers, *obj, *name_id, ctx, *loc) {
                        GetResult::Value(v) => {
                            // Cache the result for repeated access.
                            if let Some(oid) = obj_reg.as_obj_id() {
                                if fr.obj_cache.len() >= 8 { fr.obj_cache.clear(); }
                                fr.obj_cache.push((oid, *name_id, v));
                            }
                            v
                        }
                        GetResult::BoundMethod(receiver) => ctx.alloc(ManagedObject::BoundMethod {
                            receiver,
                            name_id: *name_id,
                        }),
                        GetResult::Error(msg) => {
                            return Err(JitError::runtime(msg, loc.as_error_pos()));
                        }
                    },
                };
                fr[*dst] = val;
                fr.advance();
            }
            Instruction::ObjectSet {
                obj,
                name_id,
                src,
                loc,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                // Invalidate inline cache for this object.
                if let Some(oid) = fr[*obj].as_obj_id() {
                    fr.obj_cache.retain(|(co, _, _)| *co != oid);
                }
                if let Err(msg) = handle_object_set(&fr.registers, *obj, *name_id, *src, ctx, *loc)
                {
                    return Err(JitError::runtime(msg, loc.as_error_pos()));
                }
                fr.advance();
            }

            //  Closures
            Instruction::MakeClosure {
                dst,
                name_id,
                captures,
            } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let mut vals = Vec::with_capacity(captures.len());
                for &reg in captures.iter() {
                    vals.push(fr[reg]);
                }
                let cl = crate::heap::Closure {
                    name_id: *name_id,
                    captures: vals,
                };
                fr[*dst] = ctx.alloc(ManagedObject::Closure(cl));
                fr.advance();
            }

            //  Async
            Instruction::MakePromise { dst, src } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let val = fr[*src];
                fr[*dst] = ctx.alloc(ManagedObject::Promise(PromiseState::Resolved(val)));
                fr.advance();
            }
            Instruction::MakePendingPromise { dst } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                fr[*dst] = ctx.alloc(ManagedObject::Promise(PromiseState::Pending {
                    continuation: None,
                }));
                fr.advance();
            }
            Instruction::ResolvePromise { promise, value } => {
                let fr = unsafe { frames.get_unchecked_mut(fi) };
                let promise_val = fr[*promise];
                let val = fr[*value];
                let continuation = if let Some(oid) = promise_val.as_obj_id() {
                    let objs = ctx.heap.objects.get_mut();
                    if let Some(Some(slot)) = objs.get_mut(oid as usize) {
                        match &mut slot.obj {
                            ManagedObject::Promise(PromiseState::Pending { continuation: c }) => {
                                let cont = c.take();
                                slot.obj = ManagedObject::Promise(PromiseState::Resolved(val));
                                cont
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                // Resume any continuation that was awaiting this promise
                if let Some(frame) = continuation {
                    // This recursively executes the awaiting frame.
                    // The frame's instructions/registers are clones, so they're
                    // independent of the current frame's state.
                    execute_bytecode(&frame.instructions, ctx, frame.registers, frame.pc)?;
                }
                fr.advance();
            }

            //  Calls
            Instruction::Call(box_data) => {
                let name_id = box_data.name_id;
                let dst = box_data.dst;
                let loc = box_data.loc;

                // Fast path: borrow the Callable directly to avoid cloning
                // the Arc<[Instruction]> on every call.  For user functions
                // (the common case in recursive code) this saves an atomic
                // reference-count increment/decrement per invocation.
                {
                    let callables = ctx.callables.get();
                    if let Some(Some(Callable::User(f))) = callables.get(name_id as usize) {
                        let ret = dst.map(|d| ReturnTarget { dst: d });
                        let callee_regs = build_call_registers(
                            f.locals_count,
                            &box_data.args_regs,
                            &frames[fi].registers,
                        );
                        let instr_ptr = InstrPtr::from_arc(&f.instructions);
                        frames.push(CallFrame {
                            instructions: instr_ptr,
                            func_name_id: Some(name_id),
                            registers: callee_regs,
                            pc: 0,
                            return_to: ret,
                            obj_cache: Vec::with_capacity(4),
                        });
                        frames[fi].pc += 1;
                        continue;
                    }
                }

                // Slow path: cloned Callable + by-name fallback.
                let callable = match ctx.get_callable(name_id) {
                    Some(c) => c,
                    None => {
                        let name = ctx
                            .string_pool
                            .get(name_id as usize)
                            .map(|s| s.as_ref())
                            .unwrap_or("?");
                        ctx.get_callable_by_name(name).ok_or_else(|| {
                            JitError::runtime(
                                format!("Unknown function: {}", name),
                                loc.as_error_pos(),
                            )
                        })?
                    }
                };
                set_call_loc(loc.line, loc.col);
                match callable {
                    Callable::Native(nf) => {
                        let res = if box_data.args_regs.len() <= 8 {
                            let mut buf = [Value::nil(); 8];
                            for (i, &r) in box_data.args_regs.iter().enumerate() {
                                buf[i] = frames[fi][r];
                            }
                            nf(&NativeCtx::new(ctx), &buf[..box_data.args_regs.len()])
                        } else {
                            let args: Vec<Value> =
                                box_data.args_regs.iter().map(|&r| frames[fi][r]).collect();
                            nf(&NativeCtx::new(ctx), &args)
                        }?;
                        if let Some(d) = dst {
                            frames[fi][d] = res;
                        }
                    }
                    Callable::User(f) => {
                        if box_data.args_regs.len() != f.params_count {
                            return Err(JitError::runtime(
                                format!(
                                    "Function arity mismatch: expected {}, got {}",
                                    f.params_count,
                                    box_data.args_regs.len()
                                ),
                                loc.as_error_pos(),
                            ));
                        }
                        let ret = dst.map(|d| ReturnTarget { dst: d });
                        let callee_regs = build_call_registers(
                            f.locals_count,
                            &box_data.args_regs,
                            &frames[fi].registers,
                        );
                        frames.push(CallFrame {
                            instructions: InstrPtr::from_arc(&f.instructions),
                            func_name_id: Some(name_id),
                            registers: callee_regs,
                            pc: 0,
                            return_to: ret,
                            obj_cache: Vec::with_capacity(4),
                        });
                    }
                }
                // After User path push, frames[fi] is caller (not top).
                // After Native path (no push), frames[fi] is still top.
                frames[fi].pc += 1;
            }

            Instruction::CallDynamic(box_data) => {
                let callee_reg = box_data.callee_reg;
                let dst = box_data.dst;
                let loc = box_data.loc;
                let callee_val = frames[fi][callee_reg];

                // BoundMethod dispatch (range.step, list.pad, …) — extracted to methods.rs
                if let Some(oid) = callee_val.as_obj_id() {
                    let heap = ctx.heap.objects.get();
                    let o = unsafe { heap.get_unchecked(oid as usize) };
                    if let Some(o) = o
                        && let ManagedObject::BoundMethod { receiver, name_id } = &o.obj
                        && let Some(result) = methods::dispatch_bound_method(
                            ctx,
                            *receiver,
                            *name_id,
                            &box_data.args_regs,
                            &frames[fi].registers,
                            loc,
                        )? {
                            if let Some(d) = dst {
                                frames[fi][d] = result;
                            }
                            frames[fi].advance();
                            continue;
                        }
                        // Not a known built-in method — fall through to closure or pool-string lookup

                    // Closure dispatch — call a closure's captured function.
                    if let Some(o) = o
                        && let ManagedObject::Closure(cl) = &o.obj
                    {
                        let args_regs = &*box_data.args_regs;
                        let total_args = cl.captures.len() + args_regs.len();
                        let callable = ctx.get_callable(cl.name_id).ok_or_else(|| {
                            JitError::runtime(
                                format!(
                                    "Closure references unknown function '{}'",
                                    ctx.string_pool.get(cl.name_id as usize).map_or("?", |s| s)
                                ),
                                loc.as_error_pos(),
                            )
                        })?;
                        let Callable::User(func) = callable else {
                            return Err(JitError::runtime(
                                "Closure must be a user function",
                                loc.as_error_pos(),
                            ));
                        };
                        if total_args != func.params_count {
                            return Err(JitError::runtime(
                                format!(
                                    "Closure arity mismatch: expected {}, got {}",
                                    func.params_count, total_args
                                ),
                                loc.as_error_pos(),
                            ));
                        }
                        let ret = dst.map(|d| ReturnTarget { dst: d });
                        frames[fi].advance();
                        let callee_regs = build_closure_registers(
                            func.locals_count,
                            &cl.captures,
                            args_regs,
                            &frames[fi].registers,
                        );
                        frames.push(CallFrame {
                            instructions: InstrPtr::from_arc(&func.instructions),
                            func_name_id: Some(cl.name_id),
                            registers: callee_regs,
                            pc: 0,
                            return_to: ret,
                            obj_cache: Vec::with_capacity(4),
                        });
                        continue;
                    }
                }

                let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| {
                    let hint = if let Some(b) = callee_val.as_bool() {
                        format!("boolean '{}'", b)
                    } else if callee_val.as_number().is_some() {
                        "number".into()
                    } else {
                        "value".into()
                    };
                    JitError::runtime(
                        format!(
                            "Cannot call {} as a function — not a function name or callable",
                            hint
                        ),
                        loc.as_error_pos(),
                    )
                })?;
                let callable = ctx
                    .get_callable(name_id)
                    .or_else(|| {
                        let name = ctx.string_pool.get(name_id as usize)?;
                        ctx.get_callable_by_name(name)
                    })
                    .ok_or_else(|| {
                        JitError::runtime(
                            format!(
                                "Dynamic call: unknown function '{}'",
                                ctx.string_pool.get(name_id as usize).map_or("?", |s| s)
                            ),
                            loc.as_error_pos(),
                        )
                    })?;

                dispatch_callable(&mut frames, ctx, callable, &box_data.args_regs, dst, loc)?;
                // After dispatch_callable which may have pushed, frames[fi] is the caller (not top)
                frames[fi].pc += 1;
            }
        }
    }
}
