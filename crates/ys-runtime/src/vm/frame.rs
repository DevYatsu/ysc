//! Frame stack abstraction for the register-based VM.
//!
//! Defines [`InstrPtr`], [`ReturnTarget`], [`FrameState`], and [`CallFrame`],
//! along with the [`CURRENT_FRAMES`] thread-local used by the GC to scan
//! roots on the frame stack.

use std::ops::{Index, IndexMut};
use std::sync::Arc;
use ys_core::compiler::{Instruction, Value};

// ─────────────────────────────────────────────────────────────────────────────
//  InstrPtr
// ─────────────────────────────────────────────────────────────────────────────

/// A raw pointer to a slice of instructions.
///
/// Used in the hot dispatch loop to avoid the indirection from `Arc`-based
/// access.  The `Arc` is kept alive by `ctx.callables` or the `instructions`
/// parameter of `execute_bytecode`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct InstrPtr(*const [Instruction]);
unsafe impl Send for InstrPtr {}
unsafe impl Sync for InstrPtr {}
impl InstrPtr {
    /// Build an `InstrPtr` from an `Arc<[Instruction]>`.
    pub fn from_arc(arc: &Arc<[Instruction]>) -> Self {
        Self(&**arc as *const [Instruction])
    }

    /// Return the underlying instruction slice.
    pub fn slice(&self) -> &[Instruction] {
        unsafe { &*self.0 }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  ReturnTarget & FrameState (persistable frame snapshot)
// ─────────────────────────────────────────────────────────────────────────────

/// Where to write the return value when a frame completes.
#[derive(Clone, Copy)]
pub struct ReturnTarget {
    pub dst: usize,
}

/// A snapshot of a call frame that can be stored in a promise continuation.
#[derive(Clone)]
pub struct FrameState {
    pub instructions: Arc<[Instruction]>,
    pub registers: Vec<Value>,
    pub pc: usize,
    pub return_to: Option<ReturnTarget>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  CallFrame (live frame on the stack)
// ─────────────────────────────────────────────────────────────────────────────

/// A single active call frame in the VM dispatch loop.
///
/// `instructions` is a raw pointer into `ctx.callables` (for named functions)
/// or into the initial `instructions` parameter of `execute_bytecode` (for the
/// top-level frame). Both are alive for the entire execution, so we avoid an
/// `Arc::clone` per call.  For async/generator continuations, the `Arc` is
/// reconstructed from `func_name_id` at suspension time.
pub struct CallFrame {
    pub instructions: InstrPtr,
    /// `Some(name_id)` for framed function calls, `None` for the top-level frame.
    /// Used by `Yield`/`Await` to reconstruct an `Arc<[Instruction]>` for the
    /// continuation without paying the atomic increment on every call.
    pub func_name_id: Option<u32>,
    pub registers: Vec<Value>,
    pub pc: usize,
    pub return_to: Option<ReturnTarget>,
    /// Inline cache for `ObjectGet` — remembers recent (object_id, name_id, value)
    /// lookups to skip `FxHashMap` probing on repeated property access.
    pub obj_cache: Vec<(u32, u32, Value)>,
}

impl CallFrame {
    /// Return the instruction slice for this frame.
    pub fn instr_slice(&self) -> &[Instruction] {
        self.instructions.slice()
    }

    /// Read a register value (compiler-validated index).
    #[inline]
    pub fn reg(&self, idx: usize) -> Value {
        unsafe { *self.registers.get_unchecked(idx) }
    }

    /// Write a register value (compiler-validated index).
    #[inline]
    pub fn set_reg(&mut self, idx: usize, val: Value) {
        unsafe {
            *self.registers.get_unchecked_mut(idx) = val;
        }
    }

    /// Advance the program counter by 1.
    #[inline]
    pub fn advance(&mut self) {
        self.pc += 1;
    }

    /// Jump to a target instruction address.
    #[inline]
    pub fn jump_to(&mut self, target: usize) {
        self.pc = target;
    }

    /// Return `true` when execution has reached (or passed) the end of the
    /// instruction stream.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.pc >= self.instr_slice().len()
    }

    /// Read the current instruction without bounds checking.
    #[inline]
    pub fn current_instr(&self) -> &Instruction {
        &self.instr_slice()[self.pc]
    }
}

/// Register-index access — `fr[idx]` reads a register (compiler-validated).
impl Index<usize> for CallFrame {
    type Output = Value;
    #[inline]
    fn index(&self, idx: usize) -> &Value {
        unsafe { self.registers.get_unchecked(idx) }
    }
}

/// Register-index write — `fr[idx] = val` writes a register (compiler-validated).
impl IndexMut<usize> for CallFrame {
    #[inline]
    fn index_mut(&mut self, idx: usize) -> &mut Value {
        unsafe { self.registers.get_unchecked_mut(idx) }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  CURRENT_FRAMES — GC root scanning
// ─────────────────────────────────────────────────────────────────────────────

thread_local! {
    static CURRENT_FRAMES: std::cell::UnsafeCell<*const Vec<CallFrame>> =
        const { std::cell::UnsafeCell::new(std::ptr::null()) };
}

/// Push all object IDs found in the current frame stack onto `worklist`.
///
/// Called by the garbage collector to find root references on the VM stack.
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

/// Set the current frame stack pointer for GC root scanning.
pub(crate) fn set_current_frames(frames: &Vec<CallFrame>) {
    CURRENT_FRAMES.with(|cell| unsafe { *cell.get() = frames as *const Vec<CallFrame> });
}
