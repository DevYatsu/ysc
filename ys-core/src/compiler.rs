//! Compile-time structures for the YatsuScript bytecode.
//!
//! Defines:
//! - [`Value`] — a NaN-boxed 64-bit word that represents all runtime values
//! - [`Instruction`] — the register-based VM instruction set
//! - [`Program`] — the compiled output of the parser
//!
//! The context-dependent helpers `Value::with_str` and `Value::as_string` are
//! intentionally **not** in this crate because they require the runtime heap.
//! They are provided as an extension trait in `ys-runtime`.

use std::sync::Arc;

/// Represents a location in the source code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loc {
    pub line: u32,
    pub col:  u32,
}

impl From<(usize, usize)> for Loc {
    fn from((line, col): (usize, usize)) -> Self {
        Self { line: line as u32, col: col as u32 }
    }
}

//  NaN-Boxing constants

pub const QNAN:     u64 = 0x7ff0000000000000;
pub const TAG_MASK: u64 = 0x000F000000000000;
pub const TAG_BOOL: u64 = 0x0001000000000000;
pub const TAG_OBJ:  u64 = 0x0002000000000000;
pub const TAG_POOL:   u64 = 0x000A000000000000;
pub const TAG_FAILURE: u64 = 0x000B000000000000;

//  Value

/// A NaN-boxed 64-bit value.
///
/// Layout:
/// - Plain f64 numbers use the full 64-bit range when the exponent is not all
///   ones.
/// - Quiet NaN payload encodes tagged types:
///   - `TAG_BOOL` → boolean (low bit = value)
///   - `TAG_OBJ`  → heap object reference (low 32 bits = object ID)
///   - Tags 3–9 → Small String Optimisation (SSO): (tag − 3) = byte length,
///     payload bytes packed into bits 0–47.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Value(pub u64);

impl Value {
    #[inline(always)]
    pub fn number(n: f64) -> Self { Self(n.to_bits()) }

    #[inline(always)]
    pub fn bool(b: bool) -> Self { Self(QNAN | TAG_BOOL | (b as u64)) }

    #[inline(always)]
    pub fn object(id: u32) -> Self { Self(QNAN | TAG_OBJ | (id as u64)) }

    /// Create a Value referencing a compile-time interned (pool) string by its ID.
    #[inline(always)]
    pub fn pool(id: u32) -> Self { Self(QNAN | TAG_POOL | (id as u64)) }

    /// Create a failure value tagged with a failure-type name ID.
    #[inline(always)]
    pub fn failure(name_id: u32) -> Self { Self(QNAN | TAG_FAILURE | (name_id as u64)) }

    /// Inline a string of up to 6 bytes into the NaN payload (SSO).
    /// Returns `None` if the string is too long.
    pub fn sso(s: &str) -> Option<Self> {
        if s.len() > 6 { return None; }
        let bits = QNAN | ((3 + s.len() as u64) << 48);
        let mut payload: u64 = 0;
        for (i, byte) in s.as_bytes().iter().enumerate() {
            payload |= (*byte as u64) << (i * 8);
        }
        Some(Self(bits | payload))
    }

    #[inline(always)]
    pub fn as_number(self) -> Option<f64> {
        if (self.0 & QNAN) != QNAN { Some(f64::from_bits(self.0)) } else { None }
    }

    #[inline(always)]
    pub fn as_bool(self) -> Option<bool> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_BOOL) {
            Some((self.0 & 1) != 0)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_obj_id(self) -> Option<u32> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_OBJ) {
            Some((self.0 & 0xFFFFFFFF) as u32)
        } else {
            None
        }
    }

    /// Extract the pool-string ID if this value is a tagged pool string.
    #[inline(always)]
    pub fn as_pool_id(self) -> Option<u32> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_POOL) {
            Some((self.0 & 0xFFFFFFFF) as u32)
        } else {
            None
        }
    }

    /// Extract the failure-type name ID if this value is a failure.
    #[inline(always)]
    pub fn as_failure_id(self) -> Option<u32> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_FAILURE) {
            Some((self.0 & 0xFFFFFFFF) as u32)
        } else {
            None
        }
    }

    /// Decode an SSO string directly from the NaN payload, without heap
    /// access.  Returns `None` if this value is not an SSO string.
    pub fn as_sso_str(self) -> Option<[u8; 6]> {
        let tag = (self.0 & TAG_MASK) >> 48;
        // Must have the QNAN bit set — numbers can have matching tag bits.
        if (3..=9).contains(&tag) && (self.0 & QNAN) == QNAN {
            let len = (tag - 3) as usize;
            let mut bytes = [0u8; 6];
            for (i, byte) in bytes.iter_mut().enumerate().take(len) {
                *byte = ((self.0 >> (i * 8)) & 0xFF) as u8;
            }
            Some(bytes)
        } else {
            None
        }
    }

    /// Extract the SSO string length (tags 3–9 → 0–6 bytes).
    pub fn sso_len(self) -> Option<usize> {
        let tag = (self.0 & TAG_MASK) >> 48;
        if (3..=9).contains(&tag) { Some((tag - 3) as usize) } else { None }
    }

    #[inline(always)]
    pub fn is_truthy(self) -> bool {
        if let Some(b) = self.as_bool() { return b; }
        if let Some(n) = self.as_number() { return n != 0.0 && !n.is_nan(); }
        if self.as_failure_id().is_some() { return false; }
        // Objects and SSO strings are truthy.
        self.0 != 0
    }

    #[inline(always)]
    pub fn to_bits(self)  -> u64  { self.0 }

    #[inline(always)]
    pub fn from_bits(b: u64) -> Self { Self(b) }
}

//  Instructions

/// The instruction set for the register-based YatsuScript VM.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// Load a constant `Value` into a destination register.
    LoadLiteral { dst: usize, val: Value },
    /// Copy a value from one register to another.
    Move { dst: usize, src: usize },
    /// Load a value from a global variable slot into a register.
    LoadGlobal  { dst: usize, global: usize },
    /// Store a register value into a global variable slot.
    StoreGlobal { global: usize, src: usize },
    /// Unconditional jump to a target instruction index.
    Jump(usize),
    /// Conditional jump: jump to `target` when `cond` register is falsy.
    JumpIfFalse { cond: usize, target: usize },
    /// Jump to `target` when `src` register is **not** a failure value.
    /// Used by the `or` / `except` operators to skip the fallback path.
    JumpIfNotFail { src: usize, target: usize },
    /// Jump to `target` when `var >= end` (for‑loop step‑>0 optimisation).
    /// Merges the common `Lt cond,var,end` + `JumpIfFalse cond,target` pair
    /// into a single instruction, saving a register and a dispatch cycle.
    JumpIfNotLess { var: usize, end: usize, target: usize },

    //  Arithmetic
    /// Add numbers (no NaN check — both operands MUST be plain f64).
    AddNum { dst: usize, lhs: usize, rhs: usize },
    /// Add numbers or concatenate strings.
    Add { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    /// Subtract two numbers.
    Sub { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    /// Multiply two numbers.
    Mul { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    /// Divide two numbers.
    Div { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    /// Modulus of two numbers.
    Mod { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    /// Logical NOT of a value.
    Not { dst: usize, src: usize, loc: Loc },
    /// Arithmetic negation of a number.
    Neg { dst: usize, src: usize, loc: Loc },
    /// Produce a failure value with the given failure-type name ID.
    Fail { dst: usize, name_id: u32 },
    /// Atomic increment of a local register (expected to contain a number).
    Increment(usize),
    /// Atomic increment of a global variable slot.
    IncrementGlobal(usize),

    //  Comparisons
    Eq { dst: usize, lhs: usize, rhs: usize },
    Ne { dst: usize, lhs: usize, rhs: usize },
    Lt { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    Le { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    Gt { dst: usize, lhs: usize, rhs: usize, loc: Loc },
    Ge { dst: usize, lhs: usize, rhs: usize, loc: Loc },

    //  Collections
    /// Create a new list object on the heap with an initial element count.
    NewList { dst: usize, len: usize },
    /// Create a list from pre‑evaluated element registers (list literal).
    /// Replaces the pattern `NewList` + N×(`LoadLiteral` + `ListSet`).
    NewListFrom { dst: usize, elems: Arc<[usize]> },
    /// Read an element from a list.
    ListGet { dst: usize, list: usize, index_reg: usize, loc: Loc },
    /// Write an element to a list.
    ListSet { list: usize, index_reg: usize, src: usize, loc: Loc },
    /// Create a new object (hash map) on the heap.
    NewObject { dst: usize, capacity: usize },
    /// Create a list by repeating a value N times (Rust-style `[val; count]`).
    NewListRepeat { dst: usize, val: usize, count: usize },
    /// Create an object from pre‑evaluated name→value pairs (object literal).
    /// Replaces the pattern `NewObject` + N×`ObjectSet`.
    NewObjectFrom { dst: usize, fields: Arc<[(u32, usize)]> },
    /// Read a property from an object by interned name ID.
    ObjectGet { dst: usize, obj: usize, name_id: u32, loc: Loc },
    /// Write a property to an object by interned name ID.
    ObjectSet { obj: usize, name_id: u32, src: usize, loc: Loc },

    //  Closures
    /// Create a closure that captures current register values.
    /// The closure calls a function by its interned name_id through the
    /// unified callables map — the same path as any named function call.
    MakeClosure { dst: usize, name_id: u32, captures: Arc<[usize]> },

    //  Async
    /// Wrap a value in a resolved Promise.
    MakePromise { dst: usize, src: usize },
    /// Create a pending Promise (no continuation) — used as async function return value.
    MakePendingPromise { dst: usize },
    /// Resolve a pending Promise with the given value.
    ResolvePromise { promise: usize, value: usize },

    //  Calls
    /// Call a statically-known function by its string-pool name ID.
    Call(Box<CallData>),
    /// Call a function whose name is looked up at runtime from a register.
    CallDynamic(Box<CallDynamicData>),

    //  Ranges
    /// Construct a Range object on the heap.
    Range { dst: usize, start: usize, end: usize, step: Option<usize>, loc: Loc },
    /// Destructure a Range into its start, end, and step components.
    RangeInfo { range: usize, start_dst: usize, end_dst: usize, step_dst: usize },

    //  Control
    /// Return from the current call frame.
    Return(Option<usize>),
    /// Await a Promise — yields to the event loop if pending.
    Await { dst: usize, promise: usize, loc: Loc },
}

//  Instruction payloads

#[derive(Debug, Clone, PartialEq)]
pub struct CallData {
    pub name_id:   u32,
    pub args_regs: Arc<[usize]>,
    pub dst:       Option<usize>,
    pub loc:       Loc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallDynamicData {
    pub callee_reg: usize,
    pub args_regs:  Arc<[usize]>,
    pub dst:        Option<usize>,
    pub loc:        Loc,
}

//  User-defined function

/// A compiled user-defined function.
#[derive(Debug, Clone, PartialEq)]
pub struct UserFunction {
    /// ID of the function name in the string pool.
    #[allow(dead_code)]
    pub name_id:      u32,
    /// Bytecode instructions.
    pub instructions: Arc<[Instruction]>,
    /// Total registers required by this function's stack frame.
    pub locals_count: usize,
    /// Number of parameters the function accepts (used for arity checking).
    pub params_count: usize,
}

//  Program

/// The fully compiled output of the parser — ready for execution.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Entry-point bytecode (main module body).
    pub instructions: Arc<[Instruction]>,
    /// All compiled user-defined functions.
    pub functions:    Arc<[UserFunction]>,
    /// Interned string pool shared across lexer and runtime.
    pub string_pool:  Arc<[Arc<str>]>,
    /// Number of local registers required by the main module.
    pub locals_count: usize,
    /// Number of global variable slots required.
    pub globals_count: usize,
}
