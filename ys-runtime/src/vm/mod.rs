//! The register-based virtual machine execution loop.
//!
//! [`execute_bytecode`] is the core dispatch loop. It is called recursively
//! for function calls (via a frame stack) and spawned for `spawn` blocks.
//!
//! ## Design notes
//! - Register arrays are `Arc<[AtomicU64]>` — cheaply clonable into spawned
//!   tasks and safely accessible by the GC root-tracer.
//! - The frame stack is a plain `Vec<CallFrame>` on the async task stack.
//!   Using a `Vec` and popping frames avoids indirect-recursion and keeps
//!   stack depth constant from Rust's perspective.
//! - The yield every 16 384 instructions prevents starvation of other tasks.

pub mod setup;

use crate::context::{Callable, Context, TaskRegisters};
use crate::heap::{Generation, ManagedObject};
use crate::value_ext::ValueExt;
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::future::Future;
use tokio::task::JoinSet;
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
pub(crate) fn make_registers(count: usize) -> Arc<[AtomicU64]> {
    let v: Vec<AtomicU64> = (0..count).map(|_| AtomicU64::new(0)).collect();
    Arc::from(v)
}

/// Build a register array pre-populated with call arguments.
fn build_call_registers(locals: usize, args: &[Value]) -> Arc<[AtomicU64]> {
    let regs: Vec<AtomicU64> = (0..locals).map(|_| AtomicU64::new(0)).collect();
    for (i, val) in args.iter().enumerate().take(regs.len()) {
        unsafe { regs.get_unchecked(i).store(val.to_bits(), Ordering::Relaxed) };
    }
    Arc::from(regs)
}

#[inline(always)]
fn load(regs: &[AtomicU64], i: usize) -> Value {
    Value::from_bits(unsafe { regs.get_unchecked(i).load(Ordering::Relaxed) })
}

#[inline(always)]
fn store(regs: &[AtomicU64], i: usize, v: Value) {
    unsafe { regs.get_unchecked(i).store(v.to_bits(), Ordering::Relaxed) }
}

/// Drain a `JoinSet`, returning the first error encountered.
pub(crate) async fn drain_join_set(js: &mut JoinSet<Result<(), JitError>>) -> Result<(), JitError> {
    while let Some(res) = js.join_next().await {
        if let Ok(Err(e)) = res { return Err(e); }
    }
    Ok(())
}

// ── Hot-path arithmetic macros ────────────────────────────────────────────

macro_rules! numeric_bin {
    ($regs:expr, $dst:expr, $l:expr, $r:expr, $op:tt, $loc:expr) => {{
        let lb = unsafe { $regs.get_unchecked($l).load(Ordering::Relaxed) };
        let rb = unsafe { $regs.get_unchecked($r).load(Ordering::Relaxed) };
        if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
            store($regs, $dst, Value::number(f64::from_bits(lb) $op f64::from_bits(rb)));
        } else if let (Some(lv), Some(rv)) = (Value::from_bits(lb).as_number(), Value::from_bits(rb).as_number()) {
            store($regs, $dst, Value::number(lv $op rv));
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
        let lb = unsafe { $regs.get_unchecked($l).load(Ordering::Relaxed) };
        let rb = unsafe { $regs.get_unchecked($r).load(Ordering::Relaxed) };
        let result = if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
            Some(f64::from_bits(lb) $op f64::from_bits(rb))
        } else {
            match (Value::from_bits(lb).as_number(), Value::from_bits(rb).as_number()) {
                (Some(lv), Some(rv)) => Some(lv $op rv),
                _ => None,
            }
        };
        match result {
            Some(b) => store($regs, $dst, Value::bool(b)),
            None    => return Err(JitError::runtime(
                concat!("Compare error: expected numbers for '", stringify!($op), "'"),
                $loc.line as usize, $loc.col as usize,
            )),
        }
    }};
}

macro_rules! atomic_inc {
    ($ptr:expr) => {{
        let mut old = $ptr.load(Ordering::Relaxed);
        loop {
            let next = if (old & QNAN) != QNAN {
                Value::number(f64::from_bits(old) + 1.0)
            } else if let Some(n) = Value::from_bits(old).as_number() {
                Value::number(n + 1.0)
            } else { break; };
            match $ptr.compare_exchange_weak(old, next.to_bits(), Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => break,
                Err(a) => old = a,
            }
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
    regs: &[AtomicU64],
    list: usize,
    index_reg: usize,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let list_val  = load(regs, list);
    let index_val = load(regs, index_reg);
    let idx = match index_val.as_number() {
        Some(n) => n as usize,
        None => return GetResult::Error("List index must be a number".into()),
    };

    if let Some(oid) = list_val.as_obj_id() {
        let heap = ctx.heap.objects.read();
        if let Some(Some(obj)) = heap.get(oid as usize) {
            return match &obj.obj {
                ManagedObject::List(elems) => {
                    let r = elems.read();
                    if idx < r.len() {
                        GetResult::Value(Value::from_bits(r[idx].load(Ordering::Relaxed)))
                    } else {
                        GetResult::Error(format!("List index {} out of bounds (len={})", idx, r.len()))
                    }
                }
                ManagedObject::String(s) => {
                    if idx < s.len() {
                        let byte = s.as_bytes()[idx];
                        GetResult::Value(Value::sso(&(byte as char).to_string()).unwrap_or(Value::from_bits(0)))
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
        GetResult::Value(Value::sso(&(byte as char).to_string()).unwrap_or(Value::from_bits(0)))
    } else {
        GetResult::Error("Expected a list or string for index".into())
    }
}

fn handle_list_set(
    regs: &[AtomicU64],
    list: usize,
    index_reg: usize,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let list_val  = load(regs, list);
    let index_val = load(regs, index_reg);
    let src_val   = load(regs, src);
    let idx = index_val.as_number()
        .ok_or_else(|| "List index must be a number".to_string())? as usize;
    let oid = list_val.as_obj_id()
        .ok_or_else(|| "Expected list for index assignment".to_string())?;
    let heap = ctx.heap.objects.read();
    let obj = heap.get(oid as usize).and_then(|o| o.as_ref())
        .ok_or_else(|| "Expected list for index assignment".to_string())?;
    let ManagedObject::List(elems) = &obj.obj else {
        return Err("Expected list for index assignment".to_string());
    };
    let r = elems.read();
    if idx < r.len() {
        r[idx].store(src_val.to_bits(), Ordering::Relaxed);
    } else {
        drop(r);
        let mut w = elems.write();
        if idx >= w.len() { w.resize_with(idx + 1, || AtomicU64::new(0)); }
        w[idx].store(src_val.to_bits(), Ordering::Relaxed);
    }
    Ok(())
}

fn handle_object_get(
    regs: &[AtomicU64],
    obj: usize,
    name_id: u32,
    ctx: &Context,
    _loc: Loc,
) -> GetResult {
    let obj_val = load(regs, obj);
    if let Some(oid) = obj_val.as_obj_id() {
        let heap = ctx.heap.objects.read();
        if let Some(Some(o)) = heap.get(oid as usize) {
            return match &o.obj {
                ManagedObject::Object(fields) => {
                    let r = fields.read();
                    if let Some(slot) = r.get(&name_id) {
                        GetResult::Value(Value::from_bits(slot.load(Ordering::Relaxed)))
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
    regs: &[AtomicU64],
    obj: usize,
    name_id: u32,
    src: usize,
    ctx: &Context,
    _loc: Loc,
) -> Result<(), String> {
    let obj_val = load(regs, obj);
    let src_val = load(regs, src);
    let oid = obj_val.as_obj_id()
        .ok_or_else(|| "Expected object for property assignment".to_string())?;
    let heap = ctx.heap.objects.read();
    let o = heap.get(oid as usize).and_then(|s| s.as_ref())
        .ok_or_else(|| "Expected object for property assignment".to_string())?;
    let ManagedObject::Object(fields) = &o.obj else {
        return Err("Expected object for property assignment".to_string());
    };
    {
        let rf = fields.read();
        if let Some(slot) = rf.get(&name_id) {
            slot.store(src_val.to_bits(), Ordering::Relaxed);
        } else {
            drop(rf);
            fields.write().insert(name_id, AtomicU64::new(src_val.to_bits()));
        }
    }

    // Write barrier — track tenured → nursery pointers.
    let src_bits = src_val.to_bits();
    if o.generation == Generation::Tenured
        && (src_bits & QNAN) == QNAN
        && let Some(src_oid) = Value::from_bits(src_bits).as_obj_id()
        && let Some(Some(src_obj)) = heap.get(src_oid as usize)
        && src_obj.generation == Generation::Nursery
    {
        ctx.heap.metadata.lock().remembered_set.insert(oid);
    }
    Ok(())
}

// ── Call frame ────────────────────────────────────────────────────────────────

struct ReturnTarget {
    registers: Arc<[AtomicU64]>,
    dst:       usize,
}

struct CallFrame {
    instructions: Arc<[Instruction]>,
    registers:    Arc<[AtomicU64]>,
    pc:           usize,
    return_to:    Option<ReturnTarget>,
}

// ── Main dispatch loop ────────────────────────────────────────────────────────

pub fn execute_bytecode<'a>(
    instructions: &'a Arc<[Instruction]>,
    ctx:          Arc<Context>,
    join_set:     &'a mut JoinSet<Result<(), JitError>>,
    registers:    Arc<[AtomicU64]>,
    dst_reg:      Option<&'a AtomicU64>,
    task_roots:   TaskRegisters,
) -> Pin<Box<dyn Future<Output = Result<Value, JitError>> + Send + 'a>> {
    Box::pin(async move {
        // RAII guard — restores the root stack length on exit.
        struct RegGuard { task_roots: TaskRegisters, initial_len: usize }
        impl Drop for RegGuard {
            fn drop(&mut self) { self.task_roots.lock().truncate(self.initial_len); }
        }
        let initial_root_len = {
            let mut roots = task_roots.lock();
            let len = roots.len();
            roots.push(registers.clone());
            len
        };
        let _guard = RegGuard { task_roots: task_roots.clone(), initial_len: initial_root_len };

        // ── Frame stack ───────────────────────────────────────────────────
        let mut tick: u32 = 0;
        let mut frames = vec![CallFrame {
            instructions: Arc::clone(instructions),
            registers: registers.clone(),
            pc: 0,
            return_to: None,
        }];

        loop {
            if frames.is_empty() { return Ok(Value::from_bits(0)); }
            tick = tick.wrapping_add(1);

            let fi = frames.len() - 1;

            // Implicit return at end of frame.
            if frames[fi].pc >= frames[fi].instructions.len() {
                let frame    = frames.pop().unwrap();
                let ret_val  = Value::from_bits(0);
                task_roots.lock().pop();
                if let Some(t) = frame.return_to { store(&t.registers, t.dst, ret_val); continue; }
                if let Some(d) = dst_reg { d.store(ret_val.to_bits(), Ordering::Relaxed); }
                return Ok(ret_val);
            }

            let instr = unsafe { frames[fi].instructions.get_unchecked(frames[fi].pc).clone() };

            match instr {
                // ── Memory ───────────────────────────────────────────────
                Instruction::LoadLiteral { dst, val } => {
                    store(&frames[fi].registers, dst, val);
                    frames[fi].pc += 1;
                }
                Instruction::Move { dst, src } => {
                    let v = load(&frames[fi].registers, src);
                    store(&frames[fi].registers, dst, v);
                    frames[fi].pc += 1;
                }
                Instruction::LoadGlobal { dst, global } => {
                    let v = load(&ctx.globals, global);
                    store(&frames[fi].registers, dst, v);
                    frames[fi].pc += 1;
                }
                Instruction::StoreGlobal { global, src } => {
                    let v = load(&frames[fi].registers, src);
                    unsafe { ctx.globals.get_unchecked(global).store(v.to_bits(), Ordering::Relaxed); }
                    frames[fi].pc += 1;
                }

                // ── Control flow ──────────────────────────────────────────
                Instruction::Jump(target) => { frames[fi].pc = target; continue; }
                Instruction::JumpIfFalse { cond, target } => {
                    if !load(&frames[fi].registers, cond).is_truthy() {
                        frames[fi].pc = target;
                    } else {
                        frames[fi].pc += 1;
                    }
                    continue;
                }
                Instruction::Return(val_reg) => {
                    let ret = val_reg
                        .map(|r| load(&frames[fi].registers, r))
                        .unwrap_or_else(|| Value::from_bits(0));
                    let frame = frames.pop().unwrap();
                    task_roots.lock().pop();
                    if let Some(t) = frame.return_to { store(&t.registers, t.dst, ret); continue; }
                    if let Some(d) = dst_reg { d.store(ret.to_bits(), Ordering::Relaxed); }
                    return Ok(ret);
                }

                // ── Arithmetic ────────────────────────────────────────────
                Instruction::Add { dst, lhs, rhs, loc } => {
                    let lv = load(&frames[fi].registers, lhs);
                    let rv = load(&frames[fi].registers, rhs);
                    let lb = lv.to_bits();
                    let rb = rv.to_bits();

                    if (lb & QNAN) != QNAN && (rb & QNAN) != QNAN {
                        store(&frames[fi].registers, dst, Value::number(f64::from_bits(lb) + f64::from_bits(rb)));
                    } else if let (Some(lv), Some(rv)) = (lv.as_number(), rv.as_number()) {
                        store(&frames[fi].registers, dst, Value::number(lv + rv));
                    } else {
                        // String concatenation
                        let combined = lv.with_str(&ctx, |l| rv.with_str(&ctx, |r| {
                            let mut s = String::with_capacity(l.len() + r.len());
                            s.push_str(l); s.push_str(r); s
                        })).flatten();
                        match combined {
                            Some(s) if Value::sso(&s).is_some() => {
                                store(&frames[fi].registers, dst, Value::sso(&s).unwrap());
                            }
                            Some(s) => {
                                ctx.alloc(ManagedObject::String(Arc::from(s)), unsafe {
                                    frames[fi].registers.get_unchecked(dst)
                                });
                            }
                            None => return Err(JitError::runtime(
                                "Add error: expected numbers or strings",
                                loc.line as usize, loc.col as usize,
                            )),
                        }
                    }
                    frames[fi].pc += 1;
                }
                Instruction::Sub { dst, lhs, rhs, loc } => {
                    numeric_bin!(&frames[fi].registers, dst, lhs, rhs, -, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Mul { dst, lhs, rhs, loc } => {
                    numeric_bin!(&frames[fi].registers, dst, lhs, rhs, *, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Div { dst, lhs, rhs, loc } => {
                    numeric_bin!(&frames[fi].registers, dst, lhs, rhs, /, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Not { dst, src, .. } => {
                    store(&frames[fi].registers, dst,
                          Value::bool(!load(&frames[fi].registers, src).is_truthy()));
                    frames[fi].pc += 1;
                }

                // ── Comparisons ───────────────────────────────────────────
                Instruction::Eq { dst, lhs, rhs } => {
                    let lb = unsafe { frames[fi].registers.get_unchecked(lhs).load(Ordering::Relaxed) };
                    let rb = unsafe { frames[fi].registers.get_unchecked(rhs).load(Ordering::Relaxed) };
                    store(&frames[fi].registers, dst, Value::bool(eq_fast(&ctx, lb, rb)));
                    frames[fi].pc += 1;
                }
                Instruction::Ne { dst, lhs, rhs } => {
                    let lb = unsafe { frames[fi].registers.get_unchecked(lhs).load(Ordering::Relaxed) };
                    let rb = unsafe { frames[fi].registers.get_unchecked(rhs).load(Ordering::Relaxed) };
                    store(&frames[fi].registers, dst, Value::bool(!eq_fast(&ctx, lb, rb)));
                    frames[fi].pc += 1;
                }
                Instruction::Lt { dst, lhs, rhs, loc } => {
                    compare_op!(&frames[fi].registers, dst, lhs, rhs, <, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Le { dst, lhs, rhs, loc } => {
                    compare_op!(&frames[fi].registers, dst, lhs, rhs, <=, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Gt { dst, lhs, rhs, loc } => {
                    compare_op!(&frames[fi].registers, dst, lhs, rhs, >, loc);
                    frames[fi].pc += 1;
                }
                Instruction::Ge { dst, lhs, rhs, loc } => {
                    compare_op!(&frames[fi].registers, dst, lhs, rhs, >=, loc);
                    frames[fi].pc += 1;
                }

                // ── Increments ────────────────────────────────────────────
                Instruction::Increment(reg) => {
                    let ptr = unsafe { frames[fi].registers.get_unchecked(reg) };
                    atomic_inc!(ptr);
                    frames[fi].pc += 1;
                }
                Instruction::IncrementGlobal(global) => {
                    let ptr = unsafe { ctx.globals.get_unchecked(global) };
                    atomic_inc!(ptr);
                    frames[fi].pc += 1;
                }

                // ── Ranges ────────────────────────────────────────────────
                Instruction::Range { dst, start, end, step, loc } => {
                    let s = load(&frames[fi].registers, start).as_number()
                        .ok_or_else(|| JitError::runtime("Range start must be a number", loc.line as usize, loc.col as usize))?;
                    let e = load(&frames[fi].registers, end).as_number()
                        .ok_or_else(|| JitError::runtime("Range end must be a number", loc.line as usize, loc.col as usize))?;
                    let st = if let Some(sr) = step {
                        load(&frames[fi].registers, sr).as_number()
                            .ok_or_else(|| JitError::runtime("Range step must be a number", loc.line as usize, loc.col as usize))?
                    } else { 1.0 };
                    ctx.alloc(ManagedObject::Range { start: s, end: e, step: st }, unsafe {
                        frames[fi].registers.get_unchecked(dst)
                    });
                    frames[fi].pc += 1;
                }
                Instruction::RangeInfo { range, start_dst, end_dst, step_dst } => {
                    let rv  = load(&frames[fi].registers, range);
                    let (s, e, st) = if let Some(oid) = rv.as_obj_id() {
                        let heap = ctx.heap.objects.read();
                        if let Some(Some(o)) = heap.get(oid as usize)
                            && let ManagedObject::Range { start, end, step } = &o.obj
                        { (*start, *end, *step) } else { (0.0, 0.0, 1.0) }
                    } else { (0.0, 0.0, 1.0) };
                    store(&frames[fi].registers, start_dst, Value::number(s));
                    store(&frames[fi].registers, end_dst,   Value::number(e));
                    store(&frames[fi].registers, step_dst,  Value::number(st));
                    frames[fi].pc += 1;
                }

                // ── Collections ───────────────────────────────────────────
                Instruction::NewList { dst, len } => {
                    let elems: Vec<AtomicU64> = (0..len).map(|_| AtomicU64::new(0)).collect();
                    ctx.alloc(
                        ManagedObject::List(parking_lot::RwLock::new(elems)),
                        unsafe { frames[fi].registers.get_unchecked(dst) },
                    );
                    frames[fi].pc += 1;
                }
                Instruction::ListGet { dst, list, index_reg, loc } => {
                    match handle_list_get(&frames[fi].registers, list, index_reg, &ctx, loc) {
                        GetResult::Value(v) => store(&frames[fi].registers, dst, v),
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                        _ => unreachable!(),
                    }
                    frames[fi].pc += 1;
                }
                Instruction::ListSet { list, index_reg, src, loc } => {
                    if let Err(msg) = handle_list_set(&frames[fi].registers, list, index_reg, src, &ctx, loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames[fi].pc += 1;
                }
                Instruction::NewObject { dst, .. } => {
                    ctx.alloc(
                        ManagedObject::Object(parking_lot::RwLock::new(rustc_hash::FxHashMap::default())),
                        unsafe { frames[fi].registers.get_unchecked(dst) },
                    );
                    frames[fi].pc += 1;
                }
                Instruction::ObjectGet { dst, obj, name_id, loc } => {
                    match handle_object_get(&frames[fi].registers, obj, name_id, &ctx, loc) {
                        GetResult::Value(v) => store(&frames[fi].registers, dst, v),
                        GetResult::BoundMethod(receiver) => {
                            let temp = AtomicU64::new(0);
                            ctx.alloc(ManagedObject::BoundMethod { receiver, name_id }, &temp);
                            store(&frames[fi].registers, dst, Value::from_bits(temp.load(Ordering::Relaxed)));
                        }
                        GetResult::Error(msg) => return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize)),
                    }
                    frames[fi].pc += 1;
                }
                Instruction::ObjectSet { obj, name_id, src, loc } => {
                    if let Err(msg) = handle_object_set(&frames[fi].registers, obj, name_id, src, &ctx, loc) {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                    frames[fi].pc += 1;
                }

                // ── Calls ─────────────────────────────────────────────────
                Instruction::Call(box_data) => {
                    let ys_core::compiler::CallData { name_id, args_regs, dst, loc } = *box_data;
                    let callable = ctx.get_callable(name_id).cloned().ok_or_else(|| {
                        JitError::runtime(
                            format!("Unknown function: {}", ctx.string_pool.get(name_id as usize).map_or("?", |s| s)),
                            loc.line as usize, loc.col as usize,
                        )
                    })?;

                    if let Callable::User(ref f) = callable
                        && args_regs.len() != f.params_count
                    {
                        return Err(JitError::runtime(
                            format!("Function arity mismatch: expected {}, got {}", f.params_count, args_regs.len()),
                            loc.line as usize, loc.col as usize,
                        ));
                    }

                    let args: Vec<Value> = args_regs.iter().map(|&r| load(&frames[fi].registers, r)).collect();

                    match callable {
                        Callable::Native(nf) => {
                            let res = nf(ctx.clone(), args, loc).await?;
                            if let Some(d) = dst { store(&frames[fi].registers, d, res); }
                            frames[fi].pc += 1;
                        }
                        Callable::User(f) => {
                            let ret = dst.map(|d| ReturnTarget { registers: Arc::clone(&frames[fi].registers), dst: d });
                            frames[fi].pc += 1;
                            let callee_regs = build_call_registers(f.locals_count, &args);
                            task_roots.lock().push(callee_regs.clone());
                            frames.push(CallFrame { instructions: f.instructions, registers: callee_regs, pc: 0, return_to: ret });
                        }
                    }
                }

                Instruction::CallDynamic(box_data) => {
                    let ys_core::compiler::CallDynamicData { callee_reg, args_regs, dst, loc } = *box_data;
                    let callee_val = load(&frames[fi].registers, callee_reg);
                    let args: Vec<Value> = args_regs.iter().map(|&r| load(&frames[fi].registers, r)).collect();

                    // BoundMethod dispatch (range.step, list.pad, …)
                    if let Some(oid) = callee_val.as_obj_id() {
                        let heap = ctx.heap.objects.read();
                        if let Some(Some(o)) = heap.get(oid as usize)
                            && let ManagedObject::BoundMethod { receiver, name_id } = &o.obj
                        {
                            let method   = ctx.string_pool.get(*name_id as usize).map(|s| s.to_string()).unwrap_or_default();
                            let receiver = *receiver;
                            drop(heap);

                            if method == "pad" {
                                if let Some(list_oid) = receiver.as_obj_id() {
                                    let n        = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                                    let fill     = args.get(1).copied().unwrap_or(Value::from_bits(0)).to_bits();
                                    let heap     = ctx.heap.objects.read();
                                    if let Some(Some(lo)) = heap.get(list_oid as usize)
                                        && let ManagedObject::List(elems) = &lo.obj
                                    {
                                        let mut w = elems.write();
                                        if w.len() < n { w.resize_with(n, || AtomicU64::new(fill)); }
                                    }
                                }
                                frames[fi].pc += 1;
                                continue;
                            } else if method == "step" {
                                if let Some(r_oid) = receiver.as_obj_id() {
                                    let range_vals = {
                                        let heap = ctx.heap.objects.read();
                                        heap.get(r_oid as usize)
                                            .and_then(|s| s.as_ref())
                                            .and_then(|o| if let ManagedObject::Range { start, end, .. } = &o.obj { Some((*start, *end)) } else { None })
                                    };
                                    if let Some((start, end)) = range_vals {
                                        let new_step = args.first().and_then(|v| v.as_number()).unwrap_or(1.0);
                                        let temp = AtomicU64::new(0);
                                        ctx.alloc(ManagedObject::Range { start, end, step: new_step }, &temp);
                                        if let Some(d) = dst {
                                            store(&frames[fi].registers, d, Value::from_bits(temp.load(Ordering::Relaxed)));
                                        }
                                        frames[fi].pc += 1;
                                        continue;
                                    }
                                }
                            }
                            return Err(JitError::runtime(format!("Unknown method '{}'", method), loc.line as usize, loc.col as usize));
                        }
                    }

                    let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| JitError::runtime(
                        "Callee is not a known function name", loc.line as usize, loc.col as usize,
                    ))?;
                    let callable = ctx.get_callable(name_id).cloned().ok_or_else(|| JitError::runtime(
                        format!("Dynamic call: unknown function '{}'", ctx.string_pool.get(name_id as usize).map_or("?", |s| s)),
                        loc.line as usize, loc.col as usize,
                    ))?;

                    match callable {
                        Callable::Native(nf) => {
                            let res = nf(ctx.clone(), args, loc).await?;
                            if let Some(d) = dst { store(&frames[fi].registers, d, res); }
                            frames[fi].pc += 1;
                        }
                        Callable::User(f) => {
                            if args_regs.len() != f.params_count {
                                return Err(JitError::runtime(
                                    format!("Function arity mismatch: expected {}, got {}", f.params_count, args_regs.len()),
                                    loc.line as usize, loc.col as usize,
                                ));
                            }
                            let ret = dst.map(|d| ReturnTarget { registers: Arc::clone(&frames[fi].registers), dst: d });
                            frames[fi].pc += 1;
                            let callee_regs = build_call_registers(f.locals_count, &args);
                            task_roots.lock().push(callee_regs.clone());
                            frames.push(CallFrame { instructions: f.instructions, registers: callee_regs, pc: 0, return_to: ret });
                        }
                    }
                }

                // ── Concurrency ───────────────────────────────────────────
                Instruction::Spawn(box_data) => {
                    let ys_core::compiler::SpawnData { instructions: t_instrs, locals_count, captures } = *box_data;
                    let s_ctx = ctx.clone();
                    let t_regs: Vec<AtomicU64> = (0..locals_count).map(|_| AtomicU64::new(0)).collect();
                    let parent = Arc::clone(&frames[fi].registers);
                    for &reg in captures.iter() {
                        let bits = unsafe { parent.get_unchecked(reg).load(Ordering::Relaxed) };
                        t_regs[reg].store(bits, Ordering::Relaxed);
                    }
                    let thread_regs: Arc<[AtomicU64]> = Arc::from(t_regs);

                    join_set.spawn(async move {
                        let mut js     = JoinSet::new();
                        let t_roots    = Arc::new(Mutex::new(Vec::with_capacity(16)));
                        s_ctx.active_registers.lock().push(t_roots.clone());
                        let res = execute_bytecode(&t_instrs, s_ctx, &mut js, thread_regs, None, t_roots).await;
                        drain_join_set(&mut js).await?;
                        res.map(|_| ())
                    });
                    frames[fi].pc += 1;
                }
            }

            // Cooperative yield every 16 384 instructions.
            if tick & 0x3FFF == 0 { tokio::task::yield_now().await; }
        }
    })
}
