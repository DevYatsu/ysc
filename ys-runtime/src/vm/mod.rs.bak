//! The register-based virtual machine execution loop.
//!
//! [`execute_bytecode`] is the core dispatch loop. It is called recursively
//! for function calls (via a frame stack).
//!
//! ## Design notes
//! - Register arrays are `Vec<Value>` — each frame owns its registers.
//! - The frame stack is a plain `Vec<CallFrame>` on the async task stack.
//!   Using a `Vec` and popping frames avoids indirect-recursion and keeps
//!   stack depth constant from Rust's perspective.
//! - The yield every 16 384 instructions prevents starvation of other tasks.

pub mod setup;

use crate::context::{Callable, Context};
use crate::heap::{Generation, ManagedObject};
use crate::value_ext::ValueExt;
use std::cell::RefCell;
use std::pin::Pin;

thread_local! {
    static REG_POOL: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
}

use std::sync::Arc;
use std::future::Future;
use ys_core::compiler::{Instruction, Loc, Value, QNAN};
use ys_core::error::JitError;

pub use setup::run_interpreter;

// ── Interpreter entry point (public) ─────────────────────────────────────────

pub use crate::context::Backend;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: ys_core::compiler::Program)
        -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>>
    {
        Box::pin(async move { run_interpreter(program).await })
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Allocate a zero-initialised register array of `count` slots.
pub(crate) fn make_registers(count: usize) -> Vec<Value> {
    vec![Value::from_bits(0); count]
}

// Unchecked register access — indices are compiler-validated, so no
// bounds checks needed in the dispatch loop.


/// Build a register array pre-populated with call arguments.
fn build_call_registers(locals: usize, args_regs: &[usize], caller: &[Value]) -> Vec<Value> {
    const _: () = assert!(std::mem::size_of::<Value>() == 8);
    if let Some(mut regs) = REG_POOL.with(|pool| pool.borrow_mut().pop())
        && regs.len() == locals
    {
        for v in regs.iter_mut() { *v = Value::from_bits(0); }
        for (i, &r) in args_regs.iter().enumerate().take(locals) {
            regs[i] = unsafe { *caller.get_unchecked(r) };
        }
        return regs;
    }
    let mut regs: Vec<Value> = vec![Value::from_bits(0); locals];
    for (i, &r) in args_regs.iter().enumerate().take(locals) {
        regs[i] = unsafe { *caller.get_unchecked(r) };
    }
    regs
}

/// Build a register array pre-populated with captures followed by call arguments.
/// Tries to reuse a pooled Vec before allocating a new one.
fn build_closure_registers(locals: usize, captures: &[Value], args_regs: &[usize], caller: &[Value]) -> Vec<Value> {
    if let Some(mut regs) = REG_POOL.with(|pool| pool.borrow_mut().pop())
        && regs.len() == locals
    {
        for v in regs.iter_mut() { *v = Value::from_bits(0); }
        for (i, v) in captures.iter().enumerate().take(locals) {
            regs[i] = *v;
        }
        for (i, &r) in args_regs.iter().enumerate().take(locals.saturating_sub(captures.len())) {
            regs[captures.len() + i] = unsafe { *caller.get_unchecked(r) };
        }
        return regs;
    }
    let mut regs: Vec<Value> = vec![Value::from_bits(0); locals];
    for (i, v) in captures.iter().enumerate().take(locals) {
        regs[i] = *v;
    }
    for (i, &r) in args_regs.iter().enumerate().take(locals.saturating_sub(captures.len())) {
        regs[captures.len() + i] = unsafe { *caller.get_unchecked(r) };
    }
    regs
}

fn pool_regs(regs: Vec<Value>) {
    REG_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if regs.len() <= 64 && pool.len() < 100 { pool.push(regs); }
    });
}

const _: () = assert!(std::mem::size_of::<Value>() == 8);

// ── Hot-path arithmetic macros ────────────────────────────────────────────

macro_rules! numeric_bin {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        let lv = regs[$l];
        let rv = regs[$r];
        let lb = lv.to_bits();
        let rb = rv.to_bits();
        if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
            regs[$dst] = Value::number(f64::from_bits(lb) $op f64::from_bits(rb));
        } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
            regs[$dst] = Value::number(lv $op rv);
        } else {
            return Err(JitError::runtime(
                concat!("Math error: expected numbers for '", stringify!($op), "'"),
                $loc.line as usize, $loc.col as usize,
            ));
        }
    }};
}

macro_rules! compare_op {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        let lv = regs[$l];
        let rv = regs[$r];
        let lb = lv.to_bits();
        let rb = rv.to_bits();
        let result = if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
            Some(f64::from_bits(lb) $op f64::from_bits(rb))
        } else {
            match (lv.as_number(), rv.as_number()) {
                (Some(lv), Some(rv)) => Some(lv $op rv),
                _ => None,
            }
        };
        match result {
            Some(b) => regs[$dst] = Value::bool(b),
            None    => return Err(JitError::runtime(
                concat!("Compare error: expected numbers for '", stringify!($op), "'"),
                $loc.line as usize, $loc.col as usize,
            )),
        }
    }};
}

macro_rules! inc_register {
    ($regs:expr, $i:expr) => {{
        let regs = $regs;
        let v = regs[$i];
        if let Some(n) = v.as_number() {
            regs[$i] = Value::number(n + 1.0);
        }
    }};
}

#[inline(always)]
fn eq_fast(ctx: &Context, lb: u64, rb: u64) -> bool {
    if lb == rb && (lb & QNAN) != QNAN { true }
    else { ctx.values_equal(Value::from_bits(lb), Value::from_bits(rb)) }
}

// ── GetResult for collection access ────────────────────────────────────────

enum GetResult {
    Value(Value),
    BoundMethod(Value),
    Error(String),
}

// ── Collection handlers ────────────────────────────────────────────────────

fn handle_list_get(
    regs: &[Value],
    list: usize,
    index_reg: usize,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let list_val  = regs[list];
    let index_val = regs[index_reg];
    let idx = match index_val.as_number() {
        Some(n) => n as usize,
        None => return GetResult::Error("List index must be a number".into()),
    };

    if let Some(oid) = list_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        // Safety: object ID is always valid (allocated by this heap).
        let obj = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(obj) = obj {
            return match &obj.obj {
                ManagedObject::List(elems) => {
                    if idx < elems.len() {
                        GetResult::Value(unsafe { *elems.get_unchecked(idx) })
                    } else {
                        GetResult::Error(format!("List index {} out of bounds (len={})", idx, elems.len()))
                    }
                }
                ManagedObject::String(s) => {
                    if idx < s.len() {
                        let byte = unsafe { *s.as_bytes().get_unchecked(idx) };
                        {
    let mut buf = [0u8; 4];
    let s = (byte as char).encode_utf8(&mut buf);
    GetResult::Value(Value::sso(s).unwrap_or(Value::from_bits(0)))
}
                    } else {
                        GetResult::Error(format!("String index {} out of bounds", idx))
                    }
                }
                _ => GetResult::Error("Expected a list or string for index".into()),
            };
        }
        GetResult::Error("Null object dereference".into())
    } else if let Some(s) = ctx.value_as_string(list_val)
        && idx < s.len()
    {
        let byte = s.as_bytes()[idx];
        {
    let mut buf = [0u8; 4];
    let s = (byte as char).encode_utf8(&mut buf);
    GetResult::Value(Value::sso(s).unwrap_or(Value::from_bits(0)))
}
    } else {
        GetResult::Error("Expected a list or string for index".into())
    }
}

fn handle_list_set(
    regs: &[Value],
    list: usize,
    index_reg: usize,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let list_val  = regs[list];
    let index_val = regs[index_reg];
    let src_val   = regs[src];
    let idx = index_val.as_number()
        .ok_or_else(|| "List index must be a number".to_string())? as usize;
    let oid = list_val.as_obj_id()
        .ok_or_else(|| "Expected list for index assignment".to_string())?;

    let generation;
    {
        let heap = ctx.heap.objects.get_mut();
        // Safety: object ID is always valid.
        let obj = unsafe { heap.get_unchecked_mut(oid as usize) };
        let obj = obj.as_mut()
            .ok_or_else(|| "Expected list for index assignment".to_string())?;
        let ManagedObject::List(elems) = &mut obj.obj else {
            return Err("Expected list for index assignment".to_string());
        };
        if idx < elems.len() {
            // Safety: idx is checked above.
            unsafe { *elems.get_unchecked_mut(idx) = src_val; }
        } else {
            elems.resize(idx + 1, Value::from_bits(0));
            elems[idx] = src_val;
        }
        generation = obj.generation;
    }

    // Write barrier — track tenured → nursery pointers.
    if generation == Generation::Tenured
        && let Some(src_oid) = src_val.as_obj_id()
        && let Some(Some(src_obj)) = ctx.heap.objects.get().get(src_oid as usize)
        && src_obj.generation == Generation::Nursery
    {
        ctx.heap.metadata.get_mut().remembered_set.insert(oid);
    }
    Ok(())
}

fn handle_object_get(
    regs: &[Value],
    obj: usize,
    name_id: u32,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let obj_val = regs[obj];
    if let Some(oid) = obj_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        // Safety: object ID is always valid.
        let o = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(o) = o {
            return match &o.obj {
                ManagedObject::Object(fields) => {
                    if let Some(slot) = fields.get(&name_id) {
                        GetResult::Value(*slot)
                    } else {
                        GetResult::BoundMethod(obj_val)
                    }
                }
                _ => GetResult::BoundMethod(obj_val),
            };
        }
        GetResult::Error("Null object dereference".into())
    } else {
        GetResult::Error("Expected object for property access".into())
    }
}

fn handle_object_set(
    regs: &[Value],
    obj: usize,
    name_id: u32,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let obj_val = regs[obj];
    let src_val = regs[src];
    let oid = obj_val.as_obj_id()
        .ok_or_else(|| "Expected object for property assignment".to_string())?;

    let generation;
    {
        let heap = ctx.heap.objects.get_mut();
        // Safety: object ID is always valid.
        let o = unsafe { heap.get_unchecked_mut(oid as usize) };
        let o = o.as_mut()
            .ok_or_else(|| "Expected object for property assignment".to_string())?;
        let ManagedObject::Object(fields) = &mut o.obj else {
            return Err("Expected object for property assignment".to_string());
        };
        let existing = fields.get_mut(&name_id);
        if let Some(slot) = existing {
            *slot = src_val;
        } else {
            fields.insert(name_id, src_val);
        }
        generation = o.generation;
    }

    // Write barrier — track tenured → nursery pointers.
    if generation == Generation::Tenured
        && let Some(src_oid) = src_val.as_obj_id()
        && let Some(Some(src_obj)) = ctx.heap.objects.get().get(src_oid as usize)
        && src_obj.generation == Generation::Nursery
    {
        ctx.heap.metadata.get_mut().remembered_set.insert(oid);
    }
    Ok(())
}

// ── Call frame ────────────────────────────────────────────────────────────────

#[repr(transparent)]
#[derive(Clone, Copy)]
struct InstrPtr(*const [Instruction]);
unsafe impl Send for InstrPtr {}
unsafe impl Sync for InstrPtr {}
impl InstrPtr {
    fn from_arc(arc: &Arc<[Instruction]>) -> Self { Self(&**arc as *const [Instruction]) }
    fn slice(&self) -> &[Instruction] { unsafe { &*self.0 } }
}

struct ReturnTarget {
    dst: usize,
}

struct CallFrame {
    instructions: InstrPtr,
    registers:    Vec<Value>,
    pc:           usize,
    return_to:    Option<ReturnTarget>,
}

impl CallFrame {
    fn instr_slice(&self) -> &[Instruction] { self.instructions.slice() }
}

thread_local! {
    static CURRENT_FRAMES: std::cell::UnsafeCell<*const Vec<CallFrame>> = const { std::cell::UnsafeCell::new(std::ptr::null()) };
}

pub(crate) fn scan_current_frames(worklist: &mut Vec<u32>) {
    let ptr = CURRENT_FRAMES.with(|cell| unsafe { *cell.get() });
    if !ptr.is_null() {
        let frames = unsafe { &*ptr };
        for frame in frames.iter() {
            for v in frame.registers.iter() {
                if let Some(id) = v.as_obj_id() {
                    worklist.push(id);
                }
            }
        }
    }
}

fn set_current_frames(frames: &Vec<CallFrame>) {
    CURRENT_FRAMES.with(|cell| unsafe { *cell.get() = frames as *const Vec<CallFrame> });
}

// ── Main dispatch loop ────────────────────────────────────────────────────────

pub fn execute_bytecode<'a>(
    instructions: &'a Arc<[Instruction]>,
    ctx:          Arc<Context>,
    registers:    Vec<Value>,
) -> Pin<Box<dyn Future<Output = Result<Value, JitError>> + Send + 'a>> {
    Box::pin(async move {
// ── Frame stack ───────────────────────────────────────────────────
        let mut frames = vec![CallFrame {
            instructions: InstrPtr::from_arc(instructions),
            registers,
            pc: 0,
            return_to: None,
        }];
        set_current_frames(&frames);

        loop {
            if frames.is_empty() { return Ok(Value::from_bits(0)); }

            let _fi = frames.len() - 1;

            // Implicit return at end of frame.
            if frames.last_mut().unwrap().pc >= frames.last_mut().unwrap().instr_slice().len() {
                let frame    = frames.pop().unwrap();
                let ret_val  = Value::from_bits(0);
                pool_regs(frame.registers);
                if let Some(t) = frame.return_to {
                    frames.last_mut().unwrap().registers[t.dst] = ret_val;
                    continue;
                }
                return Ok(ret_val);
            }

            // Copy instr_ptr by value to avoid borrowing frames
            let instr_ptr = frames.last_mut().unwrap().instructions;
            let pc = frames.last_mut().unwrap().pc;
            let instr = &instr_ptr.slice()[pc];

            match instr {
                // ── Memory ───────────────────────────────────────────────
                Instruction::LoadLiteral { dst, val } => {
                    frames.last_mut().unwrap().registers[*dst] = *val;
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Move { dst, src } => {
                    let d = *dst;
                    let v = frames.last_mut().unwrap().registers[*src];
                    frames.last_mut().unwrap().registers[d] = v;
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::LoadGlobal { dst, global } => {
                    let v = ctx.globals.get()[*global];
                    frames.last_mut().unwrap().registers[*dst] = v;
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::StoreGlobal { global, src } => {
                    let v = frames.last_mut().unwrap().registers[*src];
                    ctx.globals.get_mut()[*global] = v;
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Control flow ──────────────────────────────────────────
                Instruction::Jump(target) => { frames.last_mut().unwrap().pc = *target; continue; }
                Instruction::JumpIfNotLess { var, end, target } => {
                    let v = frames.last_mut().unwrap().registers[*var];
                    let e = frames.last_mut().unwrap().registers[*end];
                    if let (Some(vn), Some(en)) = (v.as_number(), e.as_number()) {
                        if vn >= en {
                            frames.last_mut().unwrap().pc = *target;
                            continue;
                        }
                    }
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::JumpIfFalse { cond, target } => {
                    if !frames.last_mut().unwrap().registers[*cond].is_truthy() {
                        frames.last_mut().unwrap().pc = *target;
                    } else {
                        frames.last_mut().unwrap().pc += 1;
                    }
                    continue;
                }
                Instruction::Return(val_reg) => {
                    let ret = val_reg
                        .map(|r| frames.last_mut().unwrap().registers[r])
                        .unwrap_or_else(|| Value::from_bits(0));
                    let frame = frames.pop().unwrap();
                    pool_regs(frame.registers);
                    if let Some(t) = frame.return_to {
                        frames.last_mut().unwrap().registers[t.dst] = ret;
                        continue;
                    }
                    return Ok(ret);
                }

                // ── Arithmetic ────────────────────────────────────────────
                Instruction::Add { dst, lhs, rhs, loc } => {
                    let lv = frames.last_mut().unwrap().registers[*lhs];
                    let rv = frames.last_mut().unwrap().registers[*rhs];
                    let lb = lv.to_bits();
                    let rb = rv.to_bits();

                    if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                        frames.last_mut().unwrap().registers[*dst] = Value::number(f64::from_bits(lb) + f64::from_bits(rb));
                    } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
                        frames.last_mut().unwrap().registers[*dst] = Value::number(lv + rv);
                    } else {
                        // String concatenation
                        let combined = lv.with_str(&ctx, |l| rv.with_str(&ctx, |r| {
                            let mut s = String::with_capacity(l.len() + r.len());
                            s.push_str(l); s.push_str(r); s
                        })).flatten();
                        match combined {
                            Some(s) if Value::sso(&s).is_some() => {
                                frames.last_mut().unwrap().registers[*dst] = Value::sso(&s).unwrap();
                            }
                            Some(s) => {
                                frames.last_mut().unwrap().registers[*dst] =
                                    ctx.alloc(ManagedObject::String(Arc::from(s)));
                            }
                            None => return Err(JitError::runtime(
                                "Add error: expected numbers or strings",
                                loc.line as usize, loc.col as usize,
                            )),
                        }
                    }
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Sub { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, -, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Mul { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, *, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Div { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, /, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Mod { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, %, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Not { dst, src, .. } => {
                    let d = *dst;
                    frames.last_mut().unwrap().registers[d] =
                          Value::bool(!frames.last_mut().unwrap().registers[*src].is_truthy());
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Comparisons ───────────────────────────────────────────
                Instruction::Eq { dst, lhs, rhs } => {
                    let lv = frames.last_mut().unwrap().registers[*lhs];
                    let rv = frames.last_mut().unwrap().registers[*rhs];
                    frames.last_mut().unwrap().registers[*dst] = Value::bool(lv.to_bits() == rv.to_bits() && (lv.to_bits() & QNAN) != QNAN);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Ne { dst, lhs, rhs } => {
                    let lv = frames.last_mut().unwrap().registers[*lhs];
                    let rv = frames.last_mut().unwrap().registers[*rhs];
                    frames.last_mut().unwrap().registers[*dst] = Value::bool(lv.to_bits() != rv.to_bits() || (lv.to_bits() & QNAN) == QNAN);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Lt { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, <, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Le { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, <=, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Gt { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, >, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::Ge { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames.last_mut().unwrap().registers, *dst, *lhs, *rhs, >=, *loc);
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Increments ────────────────────────────────────────────
                Instruction::Increment(reg) => {
                    inc_register!(&mut frames.last_mut().unwrap().registers, *reg);
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::IncrementGlobal(global) => {
                    let v = ctx.globals.get()[*global];
                    if let Some(n) = v.as_number() {
                        ctx.globals.get_mut()[*global] = Value::number(n + 1.0);
                    }
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Ranges ────────────────────────────────────────────────
                Instruction::Range { dst, start, end, step, loc } => {
                    let s = frames.last_mut().unwrap().registers[*start].as_number()
                        .ok_or_else(|| JitError::runtime("Range start must be a number", loc.line as usize, loc.col as usize))?;
                    let e = frames.last_mut().unwrap().registers[*end].as_number()
                        .ok_or_else(|| JitError::runtime("Range end must be a number", loc.line as usize, loc.col as usize))?;
                    let st = if let Some(sr) = *step {
                        frames.last_mut().unwrap().registers[sr].as_number()
                            .ok_or_else(|| JitError::runtime("Range step must be a number", loc.line as usize, loc.col as usize))?
                    } else { 1.0 };
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::Range { start: s, end: e, step: st });
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::RangeInfo { range, start_dst, end_dst, step_dst } => {
                    let rv  = frames.last_mut().unwrap().registers[*range];
                    let (s, e, st) = if let Some(oid) = rv.as_obj_id() {
                        let heap = ctx.heap.objects.get();
                        let o = unsafe { heap.get_unchecked(oid as usize) };
                        if let Some(o) = o
                            && let ManagedObject::Range { start, end, step } = &o.obj
                        { (*start, *end, *step) } else { (0.0, 0.0, 1.0) }
                    } else { (0.0, 0.0, 1.0) };
                    frames.last_mut().unwrap().registers[*start_dst] = Value::number(s);
                    frames.last_mut().unwrap().registers[*end_dst]   = Value::number(e);
                    frames.last_mut().unwrap().registers[*step_dst]  = Value::number(st);
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Collections ───────────────────────────────────────────
                Instruction::NewList { dst, len } => {
                    let elems: Vec<Value> = (0..*len).map(|_| Value::from_bits(0)).collect();
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::List(elems));
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::NewListFrom { dst, elems } => {
                    let vals: Vec<Value> = elems.iter().map(|&r| frames.last_mut().unwrap().registers[r]).collect();
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::List(vals));
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::NewListRepeat { dst, val, count } => {
                    let v = frames.last_mut().unwrap().registers[*val];
                    let n = frames.last_mut().unwrap().registers[*count].as_number().unwrap_or(0.0) as usize;
                    let vals = vec![v; n];
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::List(vals));
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::ListGet { dst, list, index_reg, loc } => {
                    match handle_list_get(&frames.last_mut().unwrap().registers, *list, *index_reg, &ctx, *loc) {
                        GetResult::Value(v) => frames.last_mut().unwrap().registers[*dst] = v,
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                        _ => unreachable!(),
                    }
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::ListSet { list, index_reg, src, loc } => {
                    if let Err(msg) = handle_list_set(&frames.last_mut().unwrap().registers, *list, *index_reg, *src, &ctx, *loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::NewObject { dst, .. } => {
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::Object(rustc_hash::FxHashMap::default()));
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::NewObjectFrom { dst, fields } => {
                    let mut map = rustc_hash::FxHashMap::default();
                    map.reserve(fields.len());
                    for &(name_id, src) in fields.iter() {
                        map.insert(name_id, frames.last_mut().unwrap().registers[src]);
                    }
                    frames.last_mut().unwrap().registers[*dst] =
                        ctx.alloc(ManagedObject::Object(map));
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::ObjectGet { dst, obj, name_id, loc } => {
                    match handle_object_get(&frames.last_mut().unwrap().registers, *obj, *name_id, &ctx, *loc) {
                        GetResult::Value(v) => frames.last_mut().unwrap().registers[*dst] = v,
                        GetResult::BoundMethod(receiver) => {
                            frames.last_mut().unwrap().registers[*dst] =
                                ctx.alloc(ManagedObject::BoundMethod { receiver, name_id: *name_id });
                        }
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                    }
                    frames.last_mut().unwrap().pc += 1;
                }
                Instruction::ObjectSet { obj, name_id, src, loc } => {
                    if let Err(msg) = handle_object_set(&frames.last_mut().unwrap().registers, *obj, *name_id, *src, &ctx, *loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Closures ──────────────────────────────────────────────
                Instruction::MakeClosure { dst, func_index, captures } => {
                    let mut vals = Vec::with_capacity(captures.len());
                    for &reg in captures.iter() {
                        vals.push(frames.last_mut().unwrap().registers[reg]);
                    }
                    let cl = crate::heap::Closure { func_index: *func_index as u32, captures: vals };
                    frames.last_mut().unwrap().registers[*dst] = ctx.alloc(ManagedObject::Closure(cl));
                    frames.last_mut().unwrap().pc += 1;
                }

                // ── Calls ─────────────────────────────────────────────────
                Instruction::Call(box_data) => {
                    let name_id = box_data.name_id;
                        let dst = box_data.dst;
                        let loc = box_data.loc;
                    let callable = ctx.get_callable(name_id).ok_or_else(|| {
                        JitError::runtime(
                            format!("Unknown function: {}", ctx.string_pool.get(name_id as usize).map_or("?", |s| s)),
                            loc.line as usize, loc.col as usize,
                        )
                    })?;

                    match callable {
                        Callable::Native(nf) => {
                            let args_regs = box_data.args_regs.clone();
                            let args: Vec<Value> = args_regs.iter().map(|&r| frames.last_mut().unwrap().registers[r]).collect();
                            let res = nf(ctx.clone(), args, loc).await?;
                            if let Some(d) = dst { frames.last_mut().unwrap().registers[d] = res; }
                            frames.last_mut().unwrap().pc += 1;
                        }
                        Callable::User(f) => {
                            let args_regs = &*box_data.args_regs;
                            if args_regs.len() != f.params_count {
                                return Err(JitError::runtime(
                                    format!("Function arity mismatch: expected {}, got {}", f.params_count, args_regs.len()),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { dst: d });
                            frames.last_mut().unwrap().pc += 1;
                            let callee_regs = build_call_registers(f.locals_count, args_regs, &frames.last_mut().unwrap().registers);
                            frames.push(CallFrame { instructions: InstrPtr::from_arc(&f.instructions), registers: callee_regs, pc: 0, return_to: ret });
                        }
                    }
                }

                Instruction::CallDynamic(box_data) => {
                    let callee_reg = box_data.callee_reg;
                        let dst = box_data.dst;
                        let loc = box_data.loc;
                    let callee_val = frames.last_mut().unwrap().registers[callee_reg];

                    // BoundMethod dispatch (range.step, list.pad, …)
                    if let Some(oid) = callee_val.as_obj_id() {
                        let heap = ctx.heap.objects.get();
                        let o = unsafe { heap.get_unchecked(oid as usize) };
                        if let Some(o) = o
                            && let ManagedObject::BoundMethod { receiver, name_id } = &o.obj
                        {
                            let method = ctx.string_pool.get(*name_id as usize).map(|s| s.as_ref()).unwrap_or("");
                            let receiver = *receiver;

                            if method == "step"
                                && let Some(r_oid) = receiver.as_obj_id() {
                                    let (start, end) = {
                                        let heap = ctx.heap.objects.get();
                                        let o = unsafe { heap.get_unchecked(r_oid as usize) };
                                        o.as_ref()
                                            .and_then(|o| if let ManagedObject::Range { start, end, .. } = &o.obj { Some((*start, *end)) } else { None })
                                    }.unwrap_or((0.0, 0.0));
                                    let args_regs = &*box_data.args_regs;
                                    let new_step = args_regs.first().map(|&r| frames.last_mut().unwrap().registers[r].as_number().unwrap_or(1.0)).unwrap_or(1.0);
                                    let val = ctx.alloc(ManagedObject::Range { start, end, step: new_step });
                                    if let Some(d) = dst {
                                        frames.last_mut().unwrap().registers[d] = val;
                                    }
                                    frames.last_mut().unwrap().pc += 1;
                                    continue;
                                }
                            return Err(JitError::runtime(format!("Unknown method '{}'", method), loc.line as usize, loc.col as usize));
                        }

                        // Closure dispatch — call a closure's captured function.
                        if let Some(o) = o
                            && let ManagedObject::Closure(cl) = &o.obj
                        {
                            let func_idx = cl.func_index as usize;
                            let args_regs = &*box_data.args_regs;
                            let total_args = cl.captures.len() + args_regs.len();
                            let func = &ctx.functions[func_idx];
                            if total_args != func.params_count {
                                return Err(JitError::runtime(
                                    format!("Closure arity mismatch: expected {}, got {}", func.params_count, total_args),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { dst: d });
                            frames.last_mut().unwrap().pc += 1;
                            let callee_regs = build_closure_registers(
                                func.locals_count,
                                &cl.captures,
                                args_regs,
                                &frames.last_mut().unwrap().registers,
                            );
                            frames.push(CallFrame {
                                instructions: InstrPtr::from_arc(&func.instructions),
                                registers: callee_regs,
                                pc: 0,
                                return_to: ret,
                            });
                            continue;
                        }
                    }

                    let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| JitError::runtime(
                        "Callee is not a known function name", loc.line as usize, loc.col as usize,
                    ))?;
                    let callable = ctx.get_callable(name_id).ok_or_else(|| JitError::runtime(
                        format!("Dynamic call: unknown function '{}'", ctx.string_pool.get(name_id as usize).map_or("?", |s| s)),
                        loc.line as usize, loc.col as usize,
                    ))?;

                    match callable {
                        Callable::Native(nf) => {
                            let args_regs = box_data.args_regs.clone();
                            let args: Vec<Value> = args_regs.iter().map(|&r| frames.last_mut().unwrap().registers[r]).collect();
                            let res = nf(ctx.clone(), args, loc).await?;
                            if let Some(d) = dst { frames.last_mut().unwrap().registers[d] = res; }
                            frames.last_mut().unwrap().pc += 1;
                        }
                        Callable::User(f) => {
                            let args_regs = &*box_data.args_regs;
                            if args_regs.len() != f.params_count {
                                return Err(JitError::runtime(
                                    format!("Function arity mismatch: expected {}, got {}", f.params_count, args_regs.len()),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { dst: d });
                            frames.last_mut().unwrap().pc += 1;
                            let callee_regs = build_call_registers(f.locals_count, args_regs, &frames.last_mut().unwrap().registers);
                            frames.push(CallFrame { instructions: InstrPtr::from_arc(&f.instructions), registers: callee_regs, pc: 0, return_to: ret });
                        }
                    }
                }
            }

        }
    })
}
