//! Heap allocation, object types, and the generational garbage collector.
//!
//! The heap stores all complex YatsuScript values that do not fit in the
//! 64-bit NaN-boxed [`Value`] inline (strings > 6 bytes, lists, objects,
//! ranges, timestamps, and bound methods).
//!
//! # Generational GC
//!
//! Objects begin life in the *nursery* generation. A minor collection only
//! scans nursery objects; a major collection (every 5th GC) scans everything.
//! Tenured objects that hold references to nursery objects are tracked in the
//! *remembered set* via a write barrier.
//!
//! # Thread safety
//!
//! With spawn removed, the heap is only accessed by a single task at a time.
//! Locks are replaced with `SyncCell` — a safe `UnsafeCell` wrapper for
//! single-threaded-but-shared interior mutability.

use std::cell::UnsafeCell;
use rustc_hash::FxHashSet;
use std::sync::Arc;
use ys_core::compiler::Value;

use crate::context::Context;

//  SyncCell: single-threaded interior mutability

/// Interior-mutability cell for single-threaded contexts shared behind `Arc`.
///
/// Like `UnsafeCell` but `Sync` so it can live in an `Arc<Context>`.
/// Safe because spawn is removed — only one task runs at a time.
#[repr(transparent)]
pub struct SyncCell<T>(UnsafeCell<T>);
unsafe impl<T: Send> Sync for SyncCell<T> {}
unsafe impl<T: Send> Send for SyncCell<T> {}

impl<T> SyncCell<T> {
    #[inline(always)]
    pub fn new(val: T) -> Self { Self(UnsafeCell::new(val)) }
    #[inline(always)]
    pub fn get(&self) -> &T { unsafe { &*self.0.get() } }
    #[inline(always)]
    pub fn get_mut(&self) -> &mut T { unsafe { &mut *self.0.get() } }
}

//  Object variants

/// A closure — a function bundled with its captured environment.
/// Uses a name_id into the unified callables map (same as named calls).
pub struct Closure {
    pub name_id: u32,
    pub captures: Vec<Value>,
}

/// Every kind of value that lives on the heap.
pub enum ManagedObject {
    /// A heap-allocated string (longer than 6 bytes).
    String(Arc<str>),
    /// A growable list of NaN-boxed values.
    List(Vec<Value>),
    /// A hash map from interned name IDs to NaN-boxed values.
    Object(rustc_hash::FxHashMap<u32, Value>),
    /// A point-in-time snapshot (`Instant::now()`).
    Timestamp(std::time::Instant),
    /// An inclusive range with an optional step.
    Range { start: f64, end: f64, step: f64 },
    /// A method reference bound to a receiver (e.g. `list.pad`).
    BoundMethod { receiver: Value, name_id: u32 },
    /// A closure bundling a function index with captured values.
    Closure(Closure),
    /// A Promise — used by async/await (JS-style).
    Promise(crate::vm::PromiseState),
}

impl ManagedObject {
    /// Walk all object-reference children, calling `f` with each object ID.
    /// Used by the GC to trace the object graph.
    pub fn visit_children<F: FnMut(u32)>(&self, mut f: F) {
        match self {
            ManagedObject::List(elements) => {
                for v in elements.iter() {
                    if let Some(id) = v.as_obj_id() {
                        f(id);
                    }
                }
            }
            ManagedObject::Object(fields) => {
                for v in fields.values() {
                    if let Some(id) = v.as_obj_id() {
                        f(id);
                    }
                }
            }
            ManagedObject::BoundMethod { receiver, .. } => {
                if let Some(id) = receiver.as_obj_id() { f(id); }
            }
            ManagedObject::Closure(cl) => {
                for v in cl.captures.iter() {
                    if let Some(id) = v.as_obj_id() {
                        f(id);
                    }
                }
            }
            ManagedObject::Promise(state) => {
                match state {
                    crate::vm::PromiseState::Resolved(v) => {
                        if let Some(id) = v.as_obj_id() {
                            f(id);
                        }
                    }
                    crate::vm::PromiseState::Rejected(_) => {} // No heap children — name_id is a string-pool index
                    crate::vm::PromiseState::Pending { continuation } => {
                        if let Some(frame) = continuation {
                            for v in frame.registers.iter() {
                                if let Some(id) = v.as_obj_id() {
                                    f(id);
                                }
                            }
                        }
                    }
                }
            }
            // Leaf types — no children.
            ManagedObject::String(_) | ManagedObject::Timestamp(_)
            | ManagedObject::Range { .. } => {}
        }
    }
}

//  Heap slot

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Generation { Nursery, Tenured }

/// A single slot in the heap, combining the object with its GC metadata.
pub struct HeapObject {
    pub obj:        ManagedObject,
    pub last_gc_id: u32,
    pub generation: Generation,
}

//  GC bookkeeping

/// GC bookkeeping data.
pub struct HeapMetadata {
    pub free_list:      Vec<u32>,
    pub nursery_ids:    Vec<u32>,
    pub remembered_set: FxHashSet<u32>,
}

//  Heap

/// The managed object store with a generational GC.
///
/// **Single-threaded only.** All fields are accessed directly without locks.
pub struct Heap {
    /// All allocated objects (indexed by u32 object ID).
    pub objects:       SyncCell<Vec<Option<HeapObject>>>,
    /// GC bookkeeping (free list, nursery set, remembered set).
    pub metadata:      SyncCell<HeapMetadata>,
    /// Running count of GC cycles — every 5th triggers a major collection.
    pub gc_count:      SyncCell<u32>,
    /// Allocations since the last GC (triggers GC at 100 000).
    pub alloc_since_gc: SyncCell<usize>,
}

impl Heap {
    /// Trigger either a minor or major collection.
    pub fn collect_garbage(&self, ctx: &Context) {
        let gc_id = {
            let count = self.gc_count.get_mut();
            *count += 1;
            *count
        };
        if gc_id.is_multiple_of(5) {
            self.major_gc(gc_id, ctx);
        } else {
            self.minor_gc(gc_id, ctx);
        }
    }

    /// Scan all live objects and free anything not reachable from roots.
    pub fn major_gc(&self, gc_id: u32, ctx: &Context) {
        let objects  = self.objects.get_mut();
        let mut worklist = Vec::new();
        self.trace_roots(ctx, &mut worklist);

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                obj.obj.visit_children(|child_id| worklist.push(child_id));
            }
        }

        let meta = self.metadata.get_mut();
        meta.remembered_set.clear();
        meta.nursery_ids.clear();

        for (i, slot) in objects.iter_mut().enumerate() {
            if let Some(obj) = slot {
                if obj.last_gc_id != gc_id {
                    *slot = None;
                    meta.free_list.push(i as u32);
                } else {
                    obj.generation = Generation::Tenured;
                }
            }
        }
    }

    /// Scan only nursery objects and objects in the remembered set.
    pub fn minor_gc(&self, gc_id: u32, ctx: &Context) {
        let objects  = self.objects.get_mut();
        let mut worklist = Vec::new();
        self.trace_roots(ctx, &mut worklist);
        {
            let meta = self.metadata.get();
            worklist.extend(meta.remembered_set.iter());
        }

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                obj.obj.visit_children(|child_id| worklist.push(child_id));
            }
        }

        let meta        = self.metadata.get_mut();
        let mut promoted    = Vec::new();
        let nursery_ids: Vec<u32> = meta.nursery_ids.drain(..).collect();

        for id in nursery_ids {
            if let Some(Some(obj)) = objects.get_mut(id as usize) {
                if obj.last_gc_id != gc_id {
                    objects[id as usize] = None;
                    meta.free_list.push(id);
                } else {
                    obj.generation = Generation::Tenured;
                    promoted.push(id);
                }
            }
        }

        // Rebuild the remembered set from tenured objects still pointing at nursery.
        let new_from_old: Vec<u32> = meta
            .remembered_set
            .iter()
            .filter(|&&id| {
                objects.get(id as usize)
                    .and_then(|s| s.as_ref())
                    .map(|o| o.generation == Generation::Tenured
                             && self.points_to_nursery(o, objects))
                    .unwrap_or(false)
            })
            .copied()
            .collect();

        let new_from_promoted: Vec<u32> = promoted
            .into_iter()
            .filter(|&id| {
                objects.get(id as usize)
                    .and_then(|s| s.as_ref())
                    .map(|o| self.points_to_nursery(o, objects))
                    .unwrap_or(false)
            })
            .collect();

        let mut new_set = FxHashSet::default();
        new_set.extend(new_from_old);
        new_set.extend(new_from_promoted);
        meta.remembered_set = new_set;
    }

    fn trace_roots(&self, ctx: &Context, worklist: &mut Vec<u32>) {
        // Note: pool strings use their own Value tag (TAG_POOL), separate from
        // heap objects (TAG_OBJ), so we don't need to add pool IDs as roots.
        for g in ctx.globals.get().iter() {
            if let Some(id) = g.as_obj_id() {
                worklist.push(id);
            }
        }
        // Scan the currently-executing task's full call frame stack.
        crate::vm::scan_current_frames(worklist);
    }

    pub fn alloc(&self, obj: ManagedObject, ctx: &Context) -> Value {
        {
            let count = self.alloc_since_gc.get_mut();
            let old = *count;
            *count += 1;
            if old > 100_000 {
                *count = 0;
                self.collect_garbage(ctx);
            }
        }

        let meta = self.metadata.get_mut();
        let id = match meta.free_list.pop() {
            Some(i) => i,
            None => {
                let objects = self.objects.get_mut();
                let i = objects.len() as u32;
                objects.push(None);
                i
            }
        };

        meta.nursery_ids.push(id);
        // Drop the metadata borrow before objects.

        let objects = self.objects.get_mut();
        objects[id as usize] = Some(HeapObject {
            obj,
            last_gc_id: 0,
            generation: Generation::Nursery,
        });

        Value::object(id)
    }

    /// Return `true` when `obj` holds a reference to any nursery-generation object.
    pub fn points_to_nursery(&self, obj: &HeapObject, heap: &[Option<HeapObject>]) -> bool {
        let mut found = false;
        obj.obj.visit_children(|child_id| {
            if !found
                && let Some(Some(child)) = heap.get(child_id as usize)
                && child.generation == Generation::Nursery
            {
                found = true;
            }
        });
        found
    }
}
