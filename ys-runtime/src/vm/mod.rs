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

#[cfg(feature = "parallel")]
use rayon::prelude::*;

pub mod setup;

use crate::context::{Callable, Context};
use crate::heap::{Generation, ManagedObject};
use crate::value_ext::ValueExt;
use crate::value_fmt::stringify_value;
use std::cell::RefCell;

thread_local! {
    static REG_POOL: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
}

use std::sync::Arc;
use ys_core::compiler::{Instruction, Loc, Value, QNAN, TAG_FAILURE};
use ys_core::error::JitError;

pub use setup::run_interpreter;

//  Interpreter entry point (public)

pub use crate::context::Backend;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: ys_core::compiler::Program) -> Result<(), JitError> {
        run_interpreter(program)
    }
}

//  Internal helpers

/// Allocate a zero-initialised register array of `count` slots.
pub(crate) fn make_registers(count: usize) -> Vec<Value> {
    vec![Value::from_bits(0); count]
}

/// Insertion sort for numeric values (sequential fallback when rayon is absent).
fn sort_insertion(elems: &mut [Value]) {
    for i in 1..elems.len() {
        let mut j = i;
        while j > 0 {
            let (a, b) = (elems[j-1].as_number(), elems[j].as_number());
            if let (Some(a), Some(b)) = (a, b) { if a <= b { break; } elems.swap(j-1, j); } else { break; }
            j -= 1;
        }
    }
}

/// Try to extract the resolved value from a Promise.  Returns:
/// - `Ok(val)` if resolved
/// - `Err(Some(name_id))` if rejected  
/// - `Err(None)` if still pending (or compound not yet satisfied)
fn resolve_promise(ctx: &Context, oid: u32) -> Result<Value, Option<u32>> {
    let objects = ctx.heap.objects.get();
    if let Some(Some(obj)) = objects.get(oid as usize) {
        match &obj.obj {
            ManagedObject::Promise(PromiseState::Resolved(v)) => Ok(*v),
            ManagedObject::Promise(PromiseState::Rejected(name_id)) => Err(Some(*name_id)),
            ManagedObject::Promise(PromiseState::Pending { .. })
            | ManagedObject::Promise(PromiseState::Compound { .. }) => Err(None),
            _ => Err(None),
        }
    } else {
        Err(None)
    }
}

/// If `v` is a resolved Promise, return its inner value; otherwise return `v` as-is.
fn resolve_promise_value(ctx: &Context, v: Value) -> Result<Value, ()> {
    if let Some(oid) = v.as_obj_id() {
        let objects = ctx.heap.objects.get();
        if let Some(Some(obj)) = objects.get(oid as usize)
            && let ManagedObject::Promise(ps) = &obj.obj
        {
            return match ps {
                PromiseState::Resolved(val) => Ok(*val),
                _ => Err(()),
            };
        }
    }
    Ok(v)
}

// Unchecked register access — indices are compiler-validated, so no
// bounds checks needed in the dispatch loop.


/// Build a register array pre-populated with call arguments.
fn build_call_registers(locals: usize, args_regs: &[usize], caller: &[Value]) -> Vec<Value> {
    const _: () = assert!(std::mem::size_of::<Value>() == 8);
    if let Some(mut regs) = REG_POOL.with(|pool| pool.borrow_mut().pop())
        && regs.len() == locals
    {
        // Only zero registers that args don't overwrite
        let args = args_regs.len().min(locals);
        for (i, &r) in args_regs.iter().enumerate().take(args) {
            regs[i] = unsafe { *caller.get_unchecked(r) };
        }
        for v in regs[args..].iter_mut() { *v = Value::from_bits(0); }
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
        // Only zero remaining registers after captures + args
        let filled = (captures.len() + args_regs.len()).min(locals);
        for (i, v) in captures.iter().enumerate().take(locals) {
            regs[i] = *v;
        }
        for (i, &r) in args_regs.iter().enumerate().take(locals.saturating_sub(captures.len())) {
            regs[captures.len() + i] = unsafe { *caller.get_unchecked(r) };
        }
        for v in regs[filled..].iter_mut() { *v = Value::from_bits(0); }
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

//  Hot-path arithmetic macros

macro_rules! numeric_bin {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        let l_bits = regs[$l].to_bits();
        let r_bits = regs[$r].to_bits();
        if (l_bits & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
            regs[$dst] = regs[$l];
        } else if (r_bits & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
            regs[$dst] = regs[$r];
        } else {
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
        }
    }};
}

macro_rules! compare_op {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let regs = $regs;
        let l_bits = regs[$l].to_bits();
        let r_bits = regs[$r].to_bits();
        if (l_bits & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
            regs[$dst] = regs[$l];
        } else if (r_bits & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
            regs[$dst] = regs[$r];
        } else {
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
        }
    }};
}

macro_rules! inc_register {
    ($regs:expr, $i:expr) => {{
        let regs = $regs;
        let v = regs[$i];
        if (v.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) {
            // Failure propagation — leave the failure register as-is
        } else if let Some(n) = v.as_number() {
            regs[$i] = Value::number(n + 1.0);
        }
    }};
}

#[allow(dead_code)]
#[inline(always)]
fn eq_fast(ctx: &Context, lb: u64, rb: u64) -> bool {
    if lb == rb && (lb & QNAN) != QNAN { true }
    else { ctx.values_equal(Value::from_bits(lb), Value::from_bits(rb)) }
}

//  GetResult for collection access

enum GetResult {
    Value(Value),
    BoundMethod(Value),
    Error(String),
}

//  Collection handlers

fn handle_object_get_by_key(
    obj_val: Value,
    key: &str,
    ctx: &Context,
) -> GetResult {
    if let Some(oid) = obj_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
        let obj = unsafe { heap.get_unchecked(oid as usize) };
        if let Some(obj) = obj {
            if let ManagedObject::Object(fields) = &obj.obj {
                let name_id = ctx.string_pool.iter()
                    .position(|s| s.as_ref() == key)
                    .map(|i| i as u32);
                match name_id.and_then(|id| fields.get(&id)) {
                    Some(val) => return GetResult::Value(*val),
                    None => return GetResult::Error(format!("Object has no field '{}'", key)),
                }
            }
        }
    }
    GetResult::Error("Expected an object for field access".into())
}

fn handle_list_get(
    regs: &[Value],
    list: usize,
    index_reg: usize,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let list_val  = regs[list];
    let index_val = regs[index_reg];

    // If index is a string, try object field access
    if let Some(key) = ctx.value_as_string(index_val) {
        return handle_object_get_by_key(list_val, &key, ctx);
    }

    let idx = match index_val.as_number() {
        Some(n) => n as usize,
        None => return GetResult::Error("List index must be a number".into()),
    };

    if let Some(oid) = list_val.as_obj_id() {
        let heap = ctx.heap.objects.get();
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
                ManagedObject::Object(fields) => {
                    // Numeric index on an object — check if string key exists as number
                    let key_str = idx.to_string();
                    let name_id = ctx.string_pool.iter()
                        .position(|s| s.as_ref() == key_str)
                        .map(|i| i as u32);
                    match name_id.and_then(|id| fields.get(&id)) {
                        Some(val) => GetResult::Value(*val),
                        None => GetResult::Error(format!("Object has no field '{}'", idx)),
                    }
                }
                _ => GetResult::Error("Expected a list, string, or object for index".into()),
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

    record_write_barrier(ctx, generation, oid, src_val);
    Ok(())
}

/// Insert a remembered-set entry when a tenured object gains a reference to a
/// nursery object.  Called by [`handle_list_set`] and [`handle_object_set`].
#[inline]
fn record_write_barrier(ctx: &Context, generation: Generation, oid: u32, val: Value) {
    if generation == Generation::Tenured
        && let Some(src_oid) = val.as_obj_id()
        && let Some(Some(src_obj)) = ctx.heap.objects.get().get(src_oid as usize)
        && src_obj.generation == Generation::Nursery
    {
        ctx.heap.metadata.get_mut().remembered_set.insert(oid);
    }
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
                // Lists, ranges, closures, BoundMethods themselves, etc.
                // all use BoundMethod dispatch for method calls.
                _ => GetResult::BoundMethod(obj_val),
            };
        }
        GetResult::Error("Null object dereference".into())
    } else {
        // SSO strings, numbers, booleans — allow method dispatch
        // by treating property access as BoundMethod creation.
        GetResult::BoundMethod(obj_val)
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

    record_write_barrier(ctx, generation, oid, src_val);
    Ok(())
}

//  Call frame

#[repr(transparent)]
#[derive(Clone, Copy)]
struct InstrPtr(*const [Instruction]);
unsafe impl Send for InstrPtr {}
unsafe impl Sync for InstrPtr {}
impl InstrPtr {
    fn from_arc(arc: &Arc<[Instruction]>) -> Self { Self(&**arc as *const [Instruction]) }
    fn slice(&self) -> &[Instruction] { unsafe { &*self.0 } }
}

#[derive(Clone, Copy)]
pub struct ReturnTarget {
    pub dst: usize,
}

#[derive(Clone)]
pub struct FrameState {
    pub instructions: Arc<[Instruction]>,
    pub registers:    Vec<Value>,
    pub pc:           usize,
    pub return_to:    Option<ReturnTarget>,
}

pub enum PromiseState {
    Pending { continuation: Option<Box<FrameState>> },
    Resolved(Value),
    Rejected(u32),  // failure name_id from string pool
    /// Tracks sub-promises that must all resolve before this promise resolves.
    /// The event loop polls sub-promises each tick.
    Compound {
        /// Object IDs of all sub-promises (unresolved = Some(oid), resolved = None).
        sub_promises: Vec<Option<u32>>,
        /// Resolved values collected in order (placeholder for unresolved).
        results: Vec<Value>,
        /// Continuation to resume when all sub-promises resolve.
        continuation: Option<Box<FrameState>>,
    },
}

struct CallFrame {
    instructions: InstrPtr,
    /// The Arc is kept alongside the raw pointer so FrameState (which is
    /// stored in continuations) can cheaply clone via Arc::clone instead of
    /// copying the entire instruction Vec.
    instr_arc:    Arc<[Instruction]>,
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

// Set before calling a native function so the function (e.g. print) can
// annotate its output with the source line number.
std::thread_local! {
    static CALL_LOC: std::cell::Cell<Option<(u32, u32)>> = const { std::cell::Cell::new(None) };
}
pub(crate) fn set_call_loc(line: u32, col: u32) {
    CALL_LOC.with(|loc| loc.set(Some((line, col))));
}
pub(crate) fn get_call_loc() -> Option<(u32, u32)> {
    CALL_LOC.with(|loc| loc.get())
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

/// Dispatch a resolved [`Callable`] — calls a native function or pushes a
/// new call frame for a user-defined function.
fn dispatch_callable(
    frames: &mut Vec<CallFrame>,
    ctx: &Arc<Context>,
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
                let mut buf = [Value::from_bits(0); 8];
                for (i, &r) in args_regs.iter().enumerate() {
                    buf[i] = unsafe { *frames[fi].registers.get_unchecked(r) };
                }
                nf(ctx, &buf[..args_regs.len()])
            } else {
                let args: Vec<Value> = args_regs.iter().map(|&r| frames[fi].registers[r]).collect();
                nf(ctx, &args)
            }?;
            if let Some(d) = dst { frames[fi].registers[d] = res; }
        }
        Callable::User(f) => {
            if args_regs.len() != f.params_count {
                return Err(JitError::runtime(
                    format!("Function arity mismatch: expected {}, got {}", f.params_count, args_regs.len()),
                    loc.line as usize, loc.col as usize,
                ));
            }
            let ret = dst.map(|d| ReturnTarget { dst: d });
            let callee_regs = build_call_registers(f.locals_count, args_regs, &frames[fi].registers);
            frames.push(CallFrame {
                instructions: InstrPtr::from_arc(&f.instructions),
                instr_arc: f.instructions.clone(),
                registers: callee_regs,
                pc: 0,
                return_to: ret,
            });
        }
    }
    Ok(())
}

//  Main dispatch loop

pub fn execute_bytecode(
    instructions: &Arc<[Instruction]>,
    ctx:          Arc<Context>,
    registers:    Vec<Value>,
    start_pc:     usize,
) -> Result<Value, JitError> {
//  Frame stack
        let mut frames = vec![CallFrame {
            instructions: InstrPtr::from_arc(instructions),
            instr_arc: instructions.clone(),
            registers,
            pc: start_pc,
            return_to: None,
        }];
        set_current_frames(&frames);

        loop {
            if frames.is_empty() { return Ok(Value::from_bits(0)); }

            let fi = frames.len() - 1;

            // Implicit return at end of frame.
            if frames[fi].pc >= frames[fi].instr_slice().len() {
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
            let instr_ptr = frames[fi].instructions;
            let pc = frames[fi].pc;
            let instr = &instr_ptr.slice()[pc];

            match instr {
                //  Memory
                Instruction::LoadLiteral { dst, val, .. } => {
                    frames[fi].registers[*dst] = *val;
                    frames[fi].pc += 1;
                }
                Instruction::Move { dst, src } => {
                    let fr = &mut frames[fi];
                    let d = *dst;
                    let v = fr.registers[*src];
                    fr.registers[d] = v;
                    fr.pc += 1;
                }
                Instruction::LoadGlobal { dst, global } => {
                    let fr = &mut frames[fi];
                    fr.registers[*dst] = ctx.globals.get()[*global];
                    fr.pc += 1;
                }
                Instruction::StoreGlobal { global, src } => {
                    let fr = &mut frames[fi];
                    ctx.globals.get_mut()[*global] = fr.registers[*src];
                    fr.pc += 1;
                }

                //  Control flow
                Instruction::Jump(target) => { frames[fi].pc = *target; continue; }
                Instruction::JumpIfNotLess { var, end, target } => {
                    let v = frames[fi].registers[*var];
                    let e = frames[fi].registers[*end];
                    if let (Some(vn), Some(en)) = (v.as_number(), e.as_number()) {
                        if vn >= en {
                            frames[fi].pc = *target;
                            continue;
                        }
                    }
                    frames[fi].pc += 1;
                }
                Instruction::JumpIfFalse { cond, target } => {
                    if !frames[fi].registers[*cond].is_truthy() {
                        frames[fi].pc = *target;
                    } else {
                        frames[fi].pc += 1;
                    }
                    continue;
                }
                Instruction::JumpIfNotFail { src, target } => {
                    let val = frames[fi].registers[*src];
                    if (val.to_bits() & (QNAN | TAG_FAILURE)) != (QNAN | TAG_FAILURE) {
                        frames[fi].pc = *target;
                    } else {
                        frames[fi].pc += 1;
                    }
                    continue;
                }
                Instruction::Return { value: val_reg, .. } => {
                    let ret = val_reg.map_or(Value::from_bits(0), |r| frames[fi].registers[r]);
                    let frame = frames.pop().unwrap();
                    pool_regs(frame.registers);
                    if let Some(t) = frame.return_to {
                        frames.last_mut().unwrap().registers[t.dst] = ret;
                        continue;
                    }
                    return Ok(ret);
                }

                Instruction::Yield { dst: _dst, value, gen_reg, loc: _loc } => {
                    let _yielded = frames[fi].registers[*value];
                    let gen_val = frames[fi].registers[*gen_reg];
                    let save_pc = frames[fi].pc;
                    let cont = Box::new(FrameState {
                        instructions: frames[fi].instr_arc.clone(),
                        registers: frames[fi].registers.clone(),
                        pc: save_pc,
                        return_to: frames[fi].return_to,
                    });
                    // Attach continuation to the generator promise
                    if let Some(goid) = gen_val.as_obj_id() {
                        let hl = ctx.heap.objects.get_mut();
                        if let Some(Some(sl)) = hl.get_mut(goid as usize) {
                            sl.obj = ManagedObject::Promise(
                                PromiseState::Pending { continuation: Some(cont) }
                            );
                        }
                    }
                    // Pop frame and return generator to caller
                    let fr = frames.pop().unwrap();
                    if let Some(rt) = fr.return_to {
                        frames.last_mut().unwrap().registers[rt.dst] = gen_val;
                        frames.last_mut().unwrap().pc += 1;
                    } else {
                        return Ok(gen_val);
                    }
                    continue;
                }

                Instruction::Await { dst, promise, loc: _loc } => {
                    let pv = frames[fi].registers[*promise];

                    // Case 1: awaiting a List — resolve each element in parallel.
                    if let Some(oid) = pv.as_obj_id() {
                        let (len, elems_copy) = {
                            let objects = ctx.heap.objects.get();
                            match objects.get(oid as usize).and_then(|o| o.as_ref()) {
                                Some(o) if matches!(o.obj, ManagedObject::List(_)) => {
                                    let elems = match &o.obj { ManagedObject::List(e) => e, _ => unreachable!() };
                                    (elems.len(), elems.clone())  // clone once
                                }
                                _ => (0, Vec::new()),
                            }
                        };
                        if len > 0 || !elems_copy.is_empty() {
                            let mut results: Vec<Value> = Vec::with_capacity(len);
                            let mut sub_promises: Vec<Option<u32>> = Vec::with_capacity(len);
                            let mut all_ready = true;
                            for elem in elems_copy.iter() {
                                match resolve_promise_value(&ctx, *elem) {
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
                                        results.push(Value::from_bits(0)); // placeholder
                                    }
                                }
                            }
                            if all_ready {
                                frames[fi].registers[*dst] = ctx.alloc(ManagedObject::List(results));
                                frames[fi].pc += 1;
                                continue;
                            }
                            // Parallel: create a compound promise
                            let saved_pc = frames[fi].pc;
                            let frame = frames.pop().unwrap();
                            let compound_val = ctx.alloc(ManagedObject::Promise(
                                PromiseState::Compound {
                                    sub_promises,
                                    results,
                                    continuation: Some(Box::new(FrameState {
                                        instructions: frame.instr_arc.clone(),
                                        registers: frame.registers.clone(),
                                        pc: saved_pc,
                                        return_to: frame.return_to,
                                    })),
                                }
                            ));
                            if let Some(ret) = &frame.return_to {
                                frames.last_mut().unwrap().registers[ret.dst] = compound_val;
                                frames.last_mut().unwrap().pc += 1;
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
                        objects.get(oid as usize)
                            .and_then(|o| o.as_ref())
                            .is_some_and(|obj| matches!(obj.obj, ManagedObject::Promise(_)))
                    }) {
                        match resolve_promise(&ctx, oid) {
                            Ok(val) => {
                                frames[fi].registers[*dst] = val;
                                frames[fi].pc += 1;
                            }
                            Err(Some(name_id)) => {
                                frames[fi].registers[*dst] = Value::failure(name_id);
                                frames[fi].pc += 1;
                                continue;
                            }
                            Err(None) => {
                                // Pending — attach our continuation to the existing promise
                                // so the event loop can resume us when the underlying operation completes.
                                let saved_pc = frames[fi].pc;
                                let continuation = Box::new(FrameState {
                                    instructions: frames[fi].instr_arc.clone(),
                                    registers: frames[fi].registers.clone(),
                                    pc: saved_pc,
                                    return_to: frames[fi].return_to,
                                });
                                // Attach to the promise object on the heap
                                {
                                    let objs = ctx.heap.objects.get_mut();
                                    if let Some(Some(slot)) = objs.get_mut(oid as usize) {
                                        if let ManagedObject::Promise(PromiseState::Pending { continuation: c }) = &mut slot.obj {
                                            *c = Some(continuation);
                                        }
                                    }
                                }
                                let frame = frames.pop().unwrap();
                                if let Some(ret) = &frame.return_to {
                                    // Return the async function's ret_promise (from
                                    // MakePendingPromise) so the caller awaits OUR
                                    // promise, not the inner awaited promise.
                                    let ret_val = {
                                        let instrs = frame.instructions.slice();
                                        if let Some(Instruction::MakePendingPromise { dst }) = instrs.first() {
                                            frame.registers.get(*dst).copied().unwrap_or(pv)
                                        } else {
                                            pv
                                        }
                                    };
                                    frames.last_mut().unwrap().registers[ret.dst] = ret_val;
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
                        frames[fi].registers[*dst] = pv;
                        frames[fi].pc += 1;
                    }
                }

                //  Arithmetic
                Instruction::AddNum { dst, lhs, rhs } => {
                    let fr = &mut frames[fi];
                    let lb = fr.registers[*lhs].to_bits();
                    let rb = fr.registers[*rhs].to_bits();
                    if (lb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { fr.registers[*dst] = fr.registers[*lhs]; fr.pc += 1; continue; }
                    if (rb & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { fr.registers[*dst] = fr.registers[*rhs]; fr.pc += 1; continue; }
                    fr.registers[*dst] = Value::number(
                        f64::from_bits(lb) + f64::from_bits(rb)
                    );
                    fr.pc += 1;
                }
                Instruction::Add { dst, lhs, rhs, loc } => {
                    let lv = frames[fi].registers[*lhs];
                    let rv = frames[fi].registers[*rhs];
                    if (lv.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { frames[fi].registers[*dst] = lv; frames[fi].pc += 1; continue; }
                    if (rv.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { frames[fi].registers[*dst] = rv; frames[fi].pc += 1; continue; }
                    let lb = lv.to_bits();
                    let rb = rv.to_bits();

                    if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                        frames[fi].registers[*dst] = Value::number(f64::from_bits(lb) + f64::from_bits(rb));
                    } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
                        frames[fi].registers[*dst] = Value::number(lv + rv);
                    } else {
                        // String concatenation (including string + number coercion)
                        let s = {
                            let ls = lv.as_string(&ctx);
                            let rs = rv.as_string(&ctx);
                            match (ls, rs) {
                                (Some(a), Some(b)) => {
                                    let mut s = String::with_capacity(a.len() + b.len());
                                    s.push_str(&a); s.push_str(&b); s
                                }
                                (Some(a), None) => {
                                    let b_str = stringify_value(&ctx, rv);
                                    let mut s = String::with_capacity(a.len() + b_str.len());
                                    s.push_str(&a); s.push_str(&b_str); s
                                }
                                (None, Some(b)) => {
                                    let a_str = stringify_value(&ctx, lv);
                                    let mut s = String::with_capacity(a_str.len() + b.len());
                                    s.push_str(&a_str); s.push_str(&b); s
                                }
                                _ => return Err(JitError::runtime(
                                    "Add error: expected numbers or strings",
                                    loc.line as usize, loc.col as usize,
                                )),
                            }
                        };
                        frames[fi].registers[*dst] = Value::sso(&s).unwrap_or_else(|| {
                            ctx.alloc(ManagedObject::String(Arc::from(s)))
                        });
                    }
                    frames[fi].pc += 1;
                }
                Instruction::Sub { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, -, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Mul { dst, lhs, rhs, loc } => {
                    numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, *, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Div { dst, lhs, rhs, loc } => {
                    // Division by zero → produce DivisionByZero failure
                    {
                        let rv = frames[fi].registers[*rhs];
                        if let Some(n) = rv.as_number() && n == 0.0 {
                            let name_id = ctx.string_pool.iter()
                                .position(|s| s.as_ref() == "DivisionByZero")
                                .unwrap_or(0) as u32;
                            frames[fi].registers[*dst] = Value::failure(name_id);
                            frames[fi].pc += 1;
                            continue;
                        }
                    }
                    numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, /, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Mod { dst, lhs, rhs, loc } => {
                    // Mod by zero → produce ModByZero failure
                    {
                        let rv = frames[fi].registers[*rhs];
                        if let Some(n) = rv.as_number() && n == 0.0 {
                            let name_id = ctx.string_pool.iter()
                                .position(|s| s.as_ref() == "ModByZero")
                                .unwrap_or(0) as u32;
                            frames[fi].registers[*dst] = Value::failure(name_id);
                            frames[fi].pc += 1;
                            continue;
                        }
                    }
                    numeric_bin!(&mut frames[fi].registers, *dst, *lhs, *rhs, %, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Not { dst, src, .. } => {
                    let fr = &mut frames[fi];
                    let sv = fr.registers[*src];
                    if (sv.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { fr.registers[*dst] = sv; fr.pc += 1; continue; }
                    fr.registers[*dst] = Value::bool(!sv.is_truthy());
                    fr.pc += 1;
                }
                Instruction::Neg { dst, src, loc } => {
                    let fr = &mut frames[fi];
                    let v = fr.registers[*src];
                    if (v.to_bits() & (QNAN | TAG_FAILURE)) == (QNAN | TAG_FAILURE) { fr.registers[*dst] = v; fr.pc += 1; continue; }
                    if let Some(n) = v.as_number() {
                        fr.registers[*dst] = Value::number(-n);
                    } else {
                        return Err(JitError::runtime(
                            "Negate error: expected a number",
                            loc.line as usize, loc.col as usize,
                        ));
                    }
                    fr.pc += 1;
                }
                Instruction::Fail { dst, name_id } => {
                    let fr = &mut frames[fi];
                    fr.registers[*dst] = Value::failure(*name_id);
                    fr.pc += 1;
                }

                //  Comparisons
                Instruction::Eq { dst, lhs, rhs } => {
                    let fr = &mut frames[fi];
                    let lv = fr.registers[*lhs];
                    let rv = fr.registers[*rhs];
                    let both_plain = (lv.to_bits() & QNAN) != QNAN && (rv.to_bits() & QNAN) != QNAN;
                    // For plain f64 values, NaN != NaN (IEEE 754).
                    // For NaN-boxed types (SSO strings, objects, bools, etc.),
                    // compare bits directly.
                    let eq = if both_plain {
                        lv.to_bits() == rv.to_bits() && (lv.to_bits() & QNAN) != QNAN
                    } else {
                        lv.to_bits() == rv.to_bits()
                    };
                    fr.registers[*dst] = Value::bool(eq);
                    fr.pc += 1;
                }
                Instruction::Ne { dst, lhs, rhs } => {
                    let fr = &mut frames[fi];
                    let lv = fr.registers[*lhs];
                    let rv = fr.registers[*rhs];
                    let both_plain = (lv.to_bits() & QNAN) != QNAN && (rv.to_bits() & QNAN) != QNAN;
                    let ne = if both_plain {
                        lv.to_bits() != rv.to_bits() || (lv.to_bits() & QNAN) == QNAN
                    } else {
                        lv.to_bits() != rv.to_bits()
                    };
                    fr.registers[*dst] = Value::bool(ne);
                    fr.pc += 1;
                }
                Instruction::Lt { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, <, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Le { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, <=, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Gt { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, >, *loc);
                    frames[fi].pc += 1;
                }
                Instruction::Ge { dst, lhs, rhs, loc } => {
                    compare_op!(&mut frames[fi].registers, *dst, *lhs, *rhs, >=, *loc);
                    frames[fi].pc += 1;
                }

                //  Unified ForNext iteration — handles lists, objects, and ranges.
                //  Returns the current element and a "has more" flag.
                Instruction::ForNext { dst_val, dst_done, iterable, idx_reg, loc: _ } => {
                    let idx = frames[fi].registers[*idx_reg].as_number().unwrap_or(0.0) as usize;
                    let iter_val = frames[fi].registers[*iterable];
                    let mut has_more = false;
                    let mut value = Value::from_bits(0);

                    if let Some(oid) = iter_val.as_obj_id() {
                        let objects = ctx.heap.objects.get();
                        match objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                            Some(ManagedObject::List(elems)) => {
                                if idx < elems.len() {
                                    value = elems[idx];
                                    has_more = true;
                                }
                            }
                            Some(ManagedObject::Object(fields)) => {
                                // Build keys list once — cache the result
                                // For simplicity, rebuild each time (objects are small)
                                let keys: Vec<&str> = ctx.string_pool.iter()
                                    .enumerate()
                                    .filter(|(i, _)| fields.contains_key(&(*i as u32)))
                                    .map(|(_, s)| s.as_ref())
                                    .collect();
                                if idx < keys.len() {
                                    let key = keys[idx];
                                    value = Value::sso(key).unwrap_or_else(||
                                        Value::pool(ctx.string_pool.iter()
                                            .position(|s| s.as_ref() == key).unwrap_or(0) as u32));
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
                    frames[fi].registers[*dst_val] = value;
                    frames[fi].registers[*dst_done] = Value::bool(has_more);
                    frames[fi].registers[*idx_reg] = Value::number((idx + 1) as f64);
                    frames[fi].pc += 1;
                }

                //  Increments
                Instruction::Increment(reg) => {
                    inc_register!(&mut frames[fi].registers, *reg);
                    frames[fi].pc += 1;
                }
                Instruction::IncrementGlobal(global) => {
                    let fr = &mut frames[fi];
                    let v = ctx.globals.get()[*global];
                    if let Some(n) = v.as_number() {
                        ctx.globals.get_mut()[*global] = Value::number(n + 1.0);
                    }
                    fr.pc += 1;
                }

                //  Ranges
                Instruction::Range { dst, start, end, step, loc } => {
                    let fr = &mut frames[fi];
                    let s = fr.registers[*start].as_number()
                        .ok_or_else(|| JitError::runtime("Range start must be a number", loc.line as usize, loc.col as usize))?;
                    let e = fr.registers[*end].as_number()
                        .ok_or_else(|| JitError::runtime("Range end must be a number", loc.line as usize, loc.col as usize))?;
                    let st = if let Some(sr) = *step {
                        fr.registers[sr].as_number()
                            .ok_or_else(|| JitError::runtime("Range step must be a number", loc.line as usize, loc.col as usize))?
                    } else { 1.0 };
                    fr.registers[*dst] =
                        ctx.alloc(ManagedObject::Range { start: s, end: e, step: st });
                    fr.pc += 1;
                }
                Instruction::RangeInfo { range, start_dst, end_dst, step_dst } => {
                    let fr = &mut frames[fi];
                    let rv  = fr.registers[*range];
                    let (s, e, st) = if let Some(oid) = rv.as_obj_id() {
                        let heap = ctx.heap.objects.get();
                        let o = unsafe { heap.get_unchecked(oid as usize) };
                        if let Some(o) = o
                            && let ManagedObject::Range { start, end, step } = &o.obj
                        { (*start, *end, *step) } else { (0.0, 0.0, 1.0) }
                    } else { (0.0, 0.0, 1.0) };
                    fr.registers[*start_dst] = Value::number(s);
                    fr.registers[*end_dst]   = Value::number(e);
                    fr.registers[*step_dst]  = Value::number(st);
                    fr.pc += 1;
                }

                //  Collections
                Instruction::NewList { dst, len } => {
                    let elems: Vec<Value> = (0..*len).map(|_| Value::from_bits(0)).collect();
                    frames[fi].registers[*dst] =
                        ctx.alloc(ManagedObject::List(elems));
                    frames[fi].pc += 1;
                }
                Instruction::NewListFrom { dst, elems } => {
                    let fr = &mut frames[fi];
                    let vals: Vec<Value> = elems.iter().map(|&r| fr.registers[r]).collect();
                    fr.registers[*dst] = ctx.alloc(ManagedObject::List(vals));
                    fr.pc += 1;
                }
                Instruction::NewListRepeat { dst, val, count } => {
                    let fr = &mut frames[fi];
                    let v = fr.registers[*val];
                    let n = fr.registers[*count].as_number().unwrap_or(0.0) as usize;
                    let vals = vec![v; n];
                    frames[fi].registers[*dst] =
                        ctx.alloc(ManagedObject::List(vals));
                    frames[fi].pc += 1;
                }
                Instruction::ListGet { dst, list, index_reg, loc } => {
                    match handle_list_get(&frames[fi].registers, *list, *index_reg, &ctx, *loc) {
                        GetResult::Value(v) => frames[fi].registers[*dst] = v,
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                        _ => unreachable!(),
                    }
                    frames[fi].pc += 1;
                }
                Instruction::ListSet { list, index_reg, src, loc } => {
                    if let Err(msg) = handle_list_set(&frames[fi].registers, *list, *index_reg, *src, &ctx, *loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames[fi].pc += 1;
                }
                Instruction::NewObject { dst, .. } => {
                    frames[fi].registers[*dst] =
                        ctx.alloc(ManagedObject::Object(rustc_hash::FxHashMap::default()));
                    frames[fi].pc += 1;
                }
                Instruction::NewObjectFrom { dst, fields } => {
                    let fr = &mut frames[fi];
                    let mut map = rustc_hash::FxHashMap::default();
                    map.reserve(fields.len());
                    for &(name_id, src) in fields.iter() {
                        map.insert(name_id, fr.registers[src]);
                    }
                    fr.registers[*dst] = ctx.alloc(ManagedObject::Object(map));
                    fr.pc += 1;
                }
                Instruction::ObjectGet { dst, obj, name_id, loc } => {
                    match handle_object_get(&frames[fi].registers, *obj, *name_id, &ctx, *loc) {
                        GetResult::Value(v) => frames[fi].registers[*dst] = v,
                        GetResult::BoundMethod(receiver) => {
                            frames[fi].registers[*dst] =
                                ctx.alloc(ManagedObject::BoundMethod { receiver, name_id: *name_id });
                        }
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                    }
                    frames[fi].pc += 1;
                }
                Instruction::ObjectSet { obj, name_id, src, loc } => {
                    if let Err(msg) = handle_object_set(&frames[fi].registers, *obj, *name_id, *src, &ctx, *loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames[fi].pc += 1;
                }

                //  Closures
                Instruction::MakeClosure { dst, name_id, captures } => {
                    let fr = &mut frames[fi];
                    let mut vals = Vec::with_capacity(captures.len());
                    for &reg in captures.iter() {
                        vals.push(fr.registers[reg]);
                    }
                    let cl = crate::heap::Closure { name_id: *name_id, captures: vals };
                    fr.registers[*dst] = ctx.alloc(ManagedObject::Closure(cl));
                    fr.pc += 1;
                }

                //  Async
                Instruction::MakePromise { dst, src } => {
                    let val = frames[fi].registers[*src];
                    frames[fi].registers[*dst] =
                        ctx.alloc(ManagedObject::Promise(PromiseState::Resolved(val)));
                    frames[fi].pc += 1;
                }
                Instruction::MakePendingPromise { dst } => {
                    frames[fi].registers[*dst] = ctx.alloc(
                        ManagedObject::Promise(PromiseState::Pending { continuation: None }),
                    );
                    frames[fi].pc += 1;
                }
                Instruction::ResolvePromise { promise, value } => {
                    let promise_val = frames[fi].registers[*promise];
                    let val = frames[fi].registers[*value];
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
                        } else { None }
                    } else { None };
                    // Resume any continuation that was awaiting this promise
                    if let Some(frame) = continuation {
                        // This recursively executes the awaiting frame.
                        // The frame's instructions/registers are clones, so they're
                        // independent of the current frame's state.
                        execute_bytecode(&frame.instructions, ctx.clone(), frame.registers, frame.pc)?;
                    }
                    frames[fi].pc += 1;
                }

                //  Calls
                Instruction::Call(box_data) => {
                    let name_id = box_data.name_id;
                        let dst = box_data.dst;
                        let loc = box_data.loc;
                    let callable = match ctx.get_callable(name_id) {
                        Some(c) => c,
                        None => {
                            let name = ctx.string_pool.get(name_id as usize).map(|s| s.as_ref()).unwrap_or("?");
                            ctx.get_callable_by_name(name).ok_or_else(|| {
                                JitError::runtime(format!("Unknown function: {}", name), loc.line as usize, loc.col as usize)
                            })?
                        }
                    };

                    // Store call location so native functions (e.g. print)
                    // can annotate output with the source line.
                    set_call_loc(loc.line, loc.col);

                    // Inline dispatch — avoids the dispatch_callable function call
                    // and extra enum match.  Matches on the reference directly.
                    match callable {
                        Callable::Native(nf) => {
                            let res = if box_data.args_regs.len() <= 8 {
                                let mut buf = [Value::from_bits(0); 8];
                                for (i, &r) in box_data.args_regs.iter().enumerate() {
                                    buf[i] = unsafe { *frames[fi].registers.get_unchecked(r) };
                                }
                                nf(&ctx, &buf[..box_data.args_regs.len()])
                            } else {
                                let args: Vec<Value> = box_data.args_regs.iter().map(|&r| frames[fi].registers[r]).collect();
                                nf(&ctx, &args)
                            }?;
                            if let Some(d) = dst { frames[fi].registers[d] = res; }
                        }
                        Callable::User(f) => {
                            if box_data.args_regs.len() != f.params_count {
                                return Err(JitError::runtime(
                                    format!("Function arity mismatch: expected {}, got {}",
                                        f.params_count, box_data.args_regs.len()),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { dst: d });
                            let callee_regs = build_call_registers(f.locals_count, &box_data.args_regs, &frames[fi].registers);
                            frames.push(CallFrame {
                                instructions: InstrPtr::from_arc(&f.instructions),
                                instr_arc: f.instructions.clone(),
                                registers: callee_regs,
                                pc: 0,
                                return_to: ret,
                            });
                        }
                    }
                    frames[fi].pc += 1;
                }

                Instruction::CallDynamic(box_data) => {
                    let callee_reg = box_data.callee_reg;
                        let dst = box_data.dst;
                        let loc = box_data.loc;
                    let callee_val = frames[fi].registers[callee_reg];

                    // BoundMethod dispatch (range.step, list.pad, …)
                    if let Some(oid) = callee_val.as_obj_id() {
                        let heap = ctx.heap.objects.get();
                        let o = unsafe { heap.get_unchecked(oid as usize) };
                        if let Some(o) = o
                            && let ManagedObject::BoundMethod { receiver, name_id } = &o.obj
                        {
                            let method = ctx.string_pool.get(*name_id as usize).map(|s| s.as_ref()).unwrap_or("");
                            let receiver = *receiver;

                            //  List method dispatch (all 18 methods)
                            if let Some(list_oid) = receiver.as_obj_id() {
                                let elems = {
                                    let objects = ctx.heap.objects.get();
                                    objects.get(list_oid as usize)
                                        .and_then(|o| o.as_ref())
                                        .and_then(|o| if let ManagedObject::List(elems) = &o.obj { Some(elems.clone()) } else { None })
                                };
                                if let Some(elems) = elems {
                                    let args_regs = &*box_data.args_regs;
                                    let read = |i: usize| args_regs.get(i).map(|&r| frames[fi].registers[r]).unwrap_or(Value::from_bits(0));
                                    let result = match method {
                                        "map" => {
                                            let mut out = Vec::with_capacity(elems.len());
                                            for v in elems {
                                                out.push(Context::call_closure(&ctx, read(0), vec![v], loc)?);
                                            }
                                            ctx.alloc(ManagedObject::List(out))
                                        }
                                        "filter" => {
                                            let mut out = Vec::new();
                                            for v in elems {
                                                if Context::call_closure(&ctx, read(0), vec![v], loc)?.is_truthy() { out.push(v); }
                                            }
                                            ctx.alloc(ManagedObject::List(out))
                                        }
                                        "reduce" if args_regs.len() >= 2 => {
                                            let init = read(0);
                                            let cl = read(1);
                                            let mut acc = init;
                                            for v in elems {
                                                acc = Context::call_closure(&ctx, cl, vec![acc, v], loc)?;
                                            }
                                            acc
                                        }
                                        "each" => {
                                            for v in elems { Context::call_closure(&ctx, read(0), vec![v], loc)?; }
                                            receiver
                                        }
                                        "find" => {
                                            let mut found = Value::from_bits(0);
                                            for v in elems {
                                                if Context::call_closure(&ctx, read(0), vec![v], loc)?.is_truthy() { found = v; break; }
                                            }
                                            found
                                        }
                                        "some" => {
                                            let mut r = Value::bool(false);
                                            for v in elems {
                                                if Context::call_closure(&ctx, read(0), vec![v], loc)?.is_truthy() { r = Value::bool(true); break; }
                                            }
                                            r
                                        }
                                        "every" => {
                                            let mut r = Value::bool(true);
                                            for v in elems {
                                                if !Context::call_closure(&ctx, read(0), vec![v], loc)?.is_truthy() { r = Value::bool(false); break; }
                                            }
                                            r
                                        }
                                        "flat_map" => {
                                            let mut out = Vec::new();
                                            for v in elems {
                                                let mapped = Context::call_closure(&ctx, read(0), vec![v], loc)?;
                                                if let Some(oid) = mapped.as_obj_id() {
                                                    let objects = ctx.heap.objects.get();
                                                    if let Some(ManagedObject::List(inner)) = objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                                                        out.extend_from_slice(inner); continue;
                                                    }
                                                }
                                                out.push(mapped);
                                            }
                                            ctx.alloc(ManagedObject::List(out))
                                        }
                                        "includes" => Value::bool(
                                            {
                                                #[cfg(feature = "parallel")]
                                                if elems.len() > 10000 {
                                                    elems.par_iter().any(|v| v.to_bits() == read(0).to_bits())
                                                } else {
                                                    elems.iter().any(|v| v.to_bits() == read(0).to_bits())
                                                }
                                                #[cfg(not(feature = "parallel"))]
                                                elems.iter().any(|v| v.to_bits() == read(0).to_bits())
                                            }),
                                        "index_of" => Value::number(elems.iter().position(|v| v.to_bits() == read(0).to_bits()).map(|i| i as f64).unwrap_or(-1.0)),
                                        "sorted" => {
                                            let mut e = elems;
                                            #[cfg(feature = "parallel")]
                                            if e.len() > 10000 {
                                                e.par_sort_unstable_by(|a, b| {
                                                    match (a.as_number(), b.as_number()) {
                                                        (Some(an), Some(bn)) => an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal),
                                                        _ => std::cmp::Ordering::Equal,
                                                    }
                                                });
                                            } else {
                                                sort_insertion(&mut e);
                                            }
                                            #[cfg(not(feature = "parallel"))]
                                            sort_insertion(&mut e);
                                            ctx.alloc(ManagedObject::List(e))
                                        }
                                        "reversed" => ctx.alloc(ManagedObject::List(elems.into_iter().rev().collect())),
                                        "slice" => {
                                            let s = read(0).as_number().unwrap_or(0.0) as usize;
                                            let e = read(1).as_number().map(|n| n as usize).unwrap_or(elems.len());
                                            let (s, e) = (s.min(elems.len()), e.min(elems.len()));
                                            ctx.alloc(ManagedObject::List(elems[s..e].to_vec()))
                                        }
                                        "concat" => {
                                            let mut e = elems;
                                            if let Some(oid) = read(0).as_obj_id() {
                                                let objects = ctx.heap.objects.get();
                                                if let Some(ManagedObject::List(other)) = objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                                                    e.extend_from_slice(other);
                                                }
                                            }
                                            ctx.alloc(ManagedObject::List(e))
                                        }
                                        "flatten" => {
                                            let mut out = Vec::new();
                                            for v in elems {
                                                if let Some(oid) = v.as_obj_id() {
                                                    let objects = ctx.heap.objects.get();
                                                    if let Some(ManagedObject::List(inner)) = objects.get(oid as usize).and_then(|o| o.as_ref()).map(|o| &o.obj) {
                                                        out.extend_from_slice(inner); continue;
                                                    }
                                                }
                                                out.push(v);
                                            }
                                            ctx.alloc(ManagedObject::List(out))
                                        }
                                        "take" => {
                                            let n = read(0).as_number().unwrap_or(0.0) as usize;
                                            ctx.alloc(ManagedObject::List(elems[..n.min(elems.len())].to_vec()))
                                        }
                                        "drop" => {
                                            let n = read(0).as_number().unwrap_or(0.0) as usize;
                                            ctx.alloc(ManagedObject::List(elems[n.min(elems.len())..].to_vec()))
                                        }
                                        "unique" => {
                                            let mut out = Vec::with_capacity(elems.len());
                                            for v in elems {
                                                if !out.iter().any(|x: &Value| x.to_bits() == v.to_bits()) { out.push(v); }
                                            }
                                            ctx.alloc(ManagedObject::List(out))
                                        }
                                        _ => { frames[fi].pc += 1; continue; }
                                    };
                                    if let Some(d) = dst { frames[fi].registers[d] = result; }
                                    frames[fi].pc += 1;
                                    continue;
                                }
                            }

                            // ────────────────────────────────────────────
                            //  Generator .next() dispatch
                            // ────────────────────────────────────────────
                            if method == "next"
                                && let Some(gen_oid) = receiver.as_obj_id()
                            {
                                let is_gen = {
                                    let objects = ctx.heap.objects.get();
                                    objects.get(gen_oid as usize)
                                        .and_then(|o| o.as_ref())
                                        .is_some_and(|o| matches!(o.obj, ManagedObject::Promise(_)))
                                };
                                if is_gen {
                                    // Extract and resume the generator's continuation
                                    let gen_state = {
                                        let objs = ctx.heap.objects.get_mut();
                                        if let Some(Some(slot)) = objs.get_mut(gen_oid as usize) {
                                            match &mut slot.obj {
                                                ManagedObject::Promise(ps) => {
                                                    match std::mem::replace(ps, PromiseState::Resolved(Value::from_bits(0))) {
                                                        PromiseState::Pending { continuation } => {
                                                            (continuation, false)
                                                        }
                                                        PromiseState::Resolved(_v) => (None, true),
                                                        PromiseState::Rejected(_) => (None, true),
                                                        PromiseState::Compound { .. } => (None, true),
                                                    }
                                                }
                                                _ => (None, true),
                                            }
                                        } else { (None, true) }
                                    };
                                    let (cont, done) = gen_state;
                                    if done {
                                        // Generator exhausted
                                        if let Some(d) = dst { frames[fi].registers[d] = Value::from_bits(0); }
                                        frames[fi].pc += 1;
                                        continue;
                                    }
                                    if let Some(frame) = cont {
                                        // Resume the generator — it will run until next yield
                                        let result = execute_bytecode(&frame.instructions, ctx.clone(), frame.registers, frame.pc)?;
                                        if let Some(d) = dst { frames[fi].registers[d] = result; }
                                    } else {
                                        if let Some(d) = dst { frames[fi].registers[d] = Value::from_bits(0); }
                                    }
                                    frames[fi].pc += 1;
                                    continue;
                                }
                            }

                            // ────────────────────────────────────────────
                            //  String method dispatch
                            // ────────────────────────────────────────────
                            if let Some(s) = ctx.value_as_string(receiver) {
                                let args_regs = &*box_data.args_regs;
                                let read_str = |i: usize| args_regs.get(i).map(|&r| frames[fi].registers[r]).and_then(|v| ctx.value_as_string(v));
                                let read_num = |i: usize| args_regs.get(i).map(|&r| frames[fi].registers[r]).and_then(|v| v.as_number());
                                let result = match method {
                                    "len" => Value::number(s.len() as f64),
                                    "upper" => {
                                        let s = s.to_uppercase();
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "lower" => {
                                        let s = s.to_lowercase();
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "trim" => {
                                        let s = s.trim().to_string();
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "starts_with" if args_regs.len() >= 1 => {
                                        Value::bool(read_str(0).map_or(false, |p| s.starts_with(&p)))
                                    }
                                    "ends_with" if args_regs.len() >= 1 => {
                                        Value::bool(read_str(0).map_or(false, |p| s.ends_with(&p)))
                                    }
                                    "contains" if args_regs.len() >= 1 => {
                                        Value::bool(read_str(0).map_or(false, |p| s.contains(&p)))
                                    }
                                    "replace" if args_regs.len() >= 2 => {
                                        let from = read_str(0).unwrap_or_default();
                                        let to = read_str(1).unwrap_or_default();
                                        let s = s.replace(&from, &to);
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "split" if args_regs.len() >= 1 => {
                                        let delim = read_str(0).unwrap_or_default();
                                        let parts: Vec<Value> = if delim.is_empty() {
                                            s.chars().map(|c| {
                                                let cs = c.to_string();
                                                Value::sso(&cs).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(cs))))
                                            }).collect()
                                        } else {
                                            s.split(&delim).map(|part| {
                                                Value::sso(part).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(part.to_string()))))
                                            }).collect()
                                        };
                                        ctx.alloc(ManagedObject::List(parts))
                                    }
                                    "repeat" if args_regs.len() >= 1 => {
                                        let n = read_num(0).unwrap_or(0.0) as usize;
                                        let s = s.repeat(n);
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "slice" => {
                                        let start = read_num(0).unwrap_or(0.0) as usize;
                                        let end = read_num(1).map(|n| n as usize).unwrap_or(s.len());
                                        let (start, end) = (start.min(s.len()), end.min(s.len()));
                                        let sub = &s[start..end];
                                        Value::sso(sub).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(sub.to_string()))))
                                    }
                                    "index_of" if args_regs.len() >= 1 => {
                                        Value::number(read_str(0).map_or(-1.0, |p| s.find(&p).map(|i| i as f64).unwrap_or(-1.0)))
                                    }
                                    "to_number" => {
                                        Value::number(s.parse::<f64>().unwrap_or(0.0))
                                    }
                                    "is_empty" => Value::bool(s.is_empty()),
                                    "chars" => {
                                        let chars: Vec<Value> = s.chars().map(|c| {
                                            let cs = c.to_string();
                                            Value::sso(&cs).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(cs))))
                                        }).collect();
                                        ctx.alloc(ManagedObject::List(chars))
                                    }
                                    _ => { frames[fi].pc += 1; continue; }
                                };
                                if let Some(d) = dst { frames[fi].registers[d] = result; }
                                frames[fi].pc += 1;
                                continue;
                            }

                            // ────────────────────────────────────────────
                            //  Number method dispatch
                            // ────────────────────────────────────────────
                            if let Some(n) = receiver.as_number() {
                                let args_regs = &*box_data.args_regs;
                                let read_num = |i: usize| args_regs.get(i).map(|&r| frames[fi].registers[r]).and_then(|v| v.as_number());
                                let result = match method {
                                    "to_string" => {
                                        let s = stringify_value(&ctx, receiver);
                                        Value::sso(&s).unwrap_or_else(|| ctx.alloc(ManagedObject::String(Arc::from(s))))
                                    }
                                    "ceil" => Value::number(n.ceil()),
                                    "floor" => Value::number(n.floor()),
                                    "round" => Value::number(n.round()),
                                    "abs" => Value::number(n.abs()),
                                    "sqrt" => Value::number(n.sqrt()),
                                    "pow" if args_regs.len() >= 1 => {
                                        Value::number(n.powf(read_num(0).unwrap_or(0.0)))
                                    }
                                    "is_integer" => Value::bool(n.fract() == 0.0),
                                    "to_int" => Value::number(n.trunc() as i64 as f64),
                                    _ => { frames[fi].pc += 1; continue; }
                                };
                                if let Some(d) = dst { frames[fi].registers[d] = result; }
                                frames[fi].pc += 1;
                                continue;
                            }

                            // ────────────────────────────────────────────
                            //  Object method dispatch
                            // ────────────────────────────────────────────
                            if let Some(obj_oid) = receiver.as_obj_id() {
                                let is_object = {
                                    let objects = ctx.heap.objects.get();
                                    objects.get(obj_oid as usize)
                                        .and_then(|o| o.as_ref())
                                        .is_some_and(|o| matches!(o.obj, ManagedObject::Object(_)))
                                };
                                if is_object {
                                    let args_regs = &*box_data.args_regs;
                                    let read_str = |i: usize| args_regs.get(i).map(|&r| frames[fi].registers[r]).and_then(|v| ctx.value_as_string(v));
                                    let result = match method {
                                        "keys" | "values" | "entries" => {
                                            let (keys, vals): (Vec<_>, Vec<_>) = {
                                                let objects = ctx.heap.objects.get();
                                                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                                                    if let ManagedObject::Object(ref fields) = obj.obj {
                                                        let mut ks: Vec<Value> = Vec::with_capacity(fields.len());
                                                        let mut vs: Vec<Value> = Vec::with_capacity(fields.len());
                                                        for (&name_id, v) in fields.iter() {
                                                            let name = ctx.string_pool.get(name_id as usize).map(|s| s.as_ref()).unwrap_or("?");
                                                            ks.push(Value::sso(name).unwrap_or_else(|| Value::pool(name_id)));
                                                            vs.push(*v);
                                                        }
                                                        (ks, vs)
                                                    } else { (Vec::new(), Vec::new()) }
                                                } else { (Vec::new(), Vec::new()) }
                                            };
                                            match method {
                                                "keys" => ctx.alloc(ManagedObject::List(keys)),
                                                "values" => ctx.alloc(ManagedObject::List(vals)),
                                                _ => { // entries
                                                    let entries: Vec<Value> = keys.into_iter().zip(vals.into_iter()).map(|(k, v)| {
                                                        ctx.alloc(ManagedObject::List(vec![k, v]))
                                                    }).collect();
                                                    ctx.alloc(ManagedObject::List(entries))
                                                }
                                            }
                                        }
                                        "has" if args_regs.len() >= 1 => {
                                            let key = read_str(0).unwrap_or_default();
                                            let found = {
                                                let objects = ctx.heap.objects.get();
                                                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                                                    if let ManagedObject::Object(ref fields) = obj.obj {
                                                        ctx.string_pool.iter().position(|s| s.as_ref() == key)
                                                            .map_or(false, |id| fields.contains_key(&(id as u32)))
                                                    } else { false }
                                                } else { false }
                                            };
                                            Value::bool(found)
                                        }
                                        "len" => {
                                            let l = {
                                                let objects = ctx.heap.objects.get();
                                                if let Some(Some(obj)) = objects.get(obj_oid as usize) {
                                                    if let ManagedObject::Object(ref fields) = obj.obj { fields.len() as f64 } else { 0.0 }
                                                } else { 0.0 }
                                            };
                                            Value::number(l)
                                        }
                                        _ => { frames[fi].pc += 1; continue; }
                                    };
                                    if let Some(d) = dst { frames[fi].registers[d] = result; }
                                    frames[fi].pc += 1;
                                    continue;
                                }
                            }

                            // User-defined method dispatch — look up method name
                            // in global callables and call with receiver as arg[0].
                            if let Some(callable) = ctx.get_callable_by_name(method) {
                                let args_regs = &*box_data.args_regs;
                                let callable = callable.clone();
                                match callable {
                                    Callable::Native(nf) => {
                                        let mut args = Vec::with_capacity(args_regs.len() + 1);
                                        args.push(receiver);
                                        for &r in args_regs.iter() {
                                            args.push(frames[fi].registers[r]);
                                        }
                                        let res = nf(&ctx, &args)?;
                                        if let Some(d) = dst { frames[fi].registers[d] = res; }
                                        frames[fi].pc += 1;
                                        continue;
                                    }
                                    Callable::User(f) => {
                                        let total_params = 1 + args_regs.len(); // receiver + args
                                        if total_params != f.params_count {
                                            return Err(JitError::runtime(
                                                format!("Method '{}' arity mismatch: expected {}, got {}",
                                                    method, f.params_count, total_params),
                                                loc.line as usize, loc.col as usize,
                                            ));
                                        }
                                        let mut callee_regs = build_call_registers(
                                            f.locals_count,
                                            args_regs, // placeholder — we'll fill recv manually
                                            &frames[fi].registers,
                                        );
                                        callee_regs[0] = receiver;
                                        for (i, &r) in args_regs.iter().enumerate() {
                                            if i + 1 < f.locals_count {
                                                callee_regs[i + 1] = frames[fi].registers[r];
                                            }
                                        }
                                        let ret = dst.map(|d| ReturnTarget { dst: d });
                                        frames[fi].pc += 1;
                                        frames.push(CallFrame {
                                            instructions: InstrPtr::from_arc(&f.instructions),
                                            instr_arc: f.instructions.clone(),
                                            registers: callee_regs,
                                            pc: 0,
                                            return_to: ret,
                                        });
                                        continue;
                                    }
                                }
                            }

                            return Err(JitError::runtime(
                                format!("No method '{}' found on this object", method),
                                loc.line as usize, loc.col as usize,
                            ));
                        }

                        // Closure dispatch — call a closure's captured function.
                        if let Some(o) = o
                            && let ManagedObject::Closure(cl) = &o.obj
                        {
                            let args_regs = &*box_data.args_regs;
                            let total_args = cl.captures.len() + args_regs.len();
                            let callable = ctx.get_callable(cl.name_id).ok_or_else(|| {
                                JitError::runtime(format!("Closure references unknown function '{}'", ctx.string_pool.get(cl.name_id as usize).map_or("?", |s| s)), loc.line as usize, loc.col as usize)
                            })?;
                            let Callable::User(func) = callable else {
                                return Err(JitError::runtime("Closure must be a user function", loc.line as usize, loc.col as usize));
                            };
                            if total_args != func.params_count {
                                return Err(JitError::runtime(
                                    format!("Closure arity mismatch: expected {}, got {}", func.params_count, total_args),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { dst: d });
                            frames[fi].pc += 1;
                            let callee_regs = build_closure_registers(
                                func.locals_count,
                                &cl.captures,
                                args_regs,
                                &frames[fi].registers,
                            );
                            frames.push(CallFrame {
                                instructions: InstrPtr::from_arc(&func.instructions),
                                instr_arc: func.instructions.clone(),
                                registers: callee_regs,
                                pc: 0,
                                return_to: ret,
                            });
                            continue;
                        }
                    }

                    let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| {
                        let hint = if let Some(b) = callee_val.as_bool() { format!("boolean '{}'", b) }
                            else if callee_val.as_number().is_some() { "number".into() }
                            else { "value".into() };
                        JitError::runtime(format!("Cannot call {} as a function — not a function name or callable", hint), loc.line as usize, loc.col as usize)
                    })?;
                    let callable = ctx.get_callable(name_id).or_else(|| {
                        let name = ctx.string_pool.get(name_id as usize)?;
                        ctx.get_callable_by_name(name)
                    }).ok_or_else(|| JitError::runtime(
                        format!("Dynamic call: unknown function '{}'", ctx.string_pool.get(name_id as usize).map_or("?", |s| s)),
                        loc.line as usize, loc.col as usize,
                    ))?;

                    dispatch_callable(&mut frames, &ctx, callable, &box_data.args_regs, dst, loc)?;
                    frames[fi].pc += 1;
                }
            }

        }
}
