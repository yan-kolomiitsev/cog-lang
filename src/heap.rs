//! Generational handles and a tracing registry for runtime graph values.
//!
//! Values keep ordinary `Rc` aliasing at the Rust boundary. The registry stores
//! weak object references, traces their Cog-value edges, and breaks unreachable
//! graph cycles before the last strong references are released. Registry growth
//! is fallible; allocation of the `Rc` payload still follows the host allocator's
//! infallible API.

use std::any::Any;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::ops::Deref;
use std::rc::{Rc, Weak};

use crate::error::RuntimeError;
use crate::value::Value;

pub(crate) trait HeapObject: Any {
    fn visit_values(&self, visit: &mut dyn FnMut(&Value));
    fn clear_values(&self);

    fn has_heap_children(&self) -> Result<bool, RuntimeError> {
        let mut has_children = false;
        let mut traversal_error = None;
        self.visit_values(&mut |value| {
            if traversal_error.is_none()
                && let Err(error) = value.visit_heap_objects(&mut |_| has_children = true)
            {
                traversal_error = Some(error);
            }
        });
        traversal_error.map_or(Ok(has_children), Err)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ObjectId {
    slot: usize,
    generation: u64,
}

pub(crate) struct ObjectKey {
    registry: Rc<RefCell<Registry>>,
    id: ObjectId,
}

struct Slot {
    generation: u64,
    object: Option<Weak<dyn HeapObject>>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<usize>,
    allocations_since_collection: usize,
    live: usize,
    collection_threshold: usize,
}

impl Registry {
    fn retire(&mut self, id: ObjectId) {
        let slot = &mut self.slots[id.slot];
        if slot.generation != id.generation || slot.object.is_none() {
            return;
        }
        slot.object = None;
        self.live -= 1;
        if let Some(generation) = slot.generation.checked_add(1) {
            slot.generation = generation;
            // A failed free-list growth only prevents reuse of this dead slot.
            if self.free.try_reserve(1).is_ok() {
                self.free.push(id.slot);
            }
        }
    }
}

thread_local! {
    static REGISTRY: Rc<RefCell<Registry>> = Rc::default();
    static COLLECTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct CollectionFlag;

impl Drop for CollectionFlag {
    fn drop(&mut self) {
        COLLECTING.with(|collecting| collecting.set(false));
    }
}

/// A typed, generational reference to an object managed by the current thread's
/// VM heap. Cloning a handle preserves ordinary Cog aliasing.
pub struct HeapRef<T: 'static> {
    object: Rc<T>,
    registry: Rc<RefCell<Registry>>,
    id: ObjectId,
    drop_hook: fn(&HeapRef<T>),
    marker: PhantomData<T>,
}

impl<T: 'static> Clone for HeapRef<T> {
    fn clone(&self) -> Self {
        Self {
            object: self.object.clone(),
            registry: self.registry.clone(),
            id: self.id,
            drop_hook: self.drop_hook,
            marker: PhantomData,
        }
    }
}

impl<T: 'static> std::fmt::Debug for HeapRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeapRef")
            .field("slot", &self.id.slot)
            .field("generation", &self.id.generation)
            .finish_non_exhaustive()
    }
}

#[allow(private_bounds)]
impl<T: HeapObject> HeapRef<T> {
    /// Allocate a runtime-managed object. Most embedders should obtain values
    /// from Cog or the SDK constructors instead of building graph nodes by hand.
    pub fn new(value: T) -> Self {
        Self::try_new(value).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Allocate a runtime-managed object, returning an error if the heap registry
    /// cannot grow. The Rust standard library does not expose fallible `Rc`
    /// allocation, so this does not convert host-wide out-of-memory into an
    /// error.
    pub fn try_new(value: T) -> Result<Self, RuntimeError> {
        REGISTRY.with(|registry| Self::try_new_in(registry.clone(), value))
    }

    fn try_new_in(registry: Rc<RefCell<Registry>>, value: T) -> Result<Self, RuntimeError> {
        let mut registry_state =
            registry
                .try_borrow_mut()
                .map_err(|_| RuntimeError::InvalidArgument {
                    op: "VM heap allocation",
                })?;
        if registry_state.free.is_empty() {
            registry_state
                .slots
                .try_reserve(1)
                .map_err(|_| RuntimeError::ResourceLimit {
                    resource: "VM heap registry",
                })?;
        }

        // Reserve registry storage before allocating the Rc so a failed slot
        // growth cannot leave an unregistered managed object behind.
        let object = Rc::new(value);
        let erased: Rc<dyn HeapObject> = object.clone();
        let weak = Rc::downgrade(&erased);
        drop(erased);

        registry_state.allocations_since_collection = registry_state
            .allocations_since_collection
            .saturating_add(1);
        let id = if let Some(slot) = registry_state.free.pop() {
            let generation = registry_state.slots[slot].generation;
            registry_state.slots[slot].object = Some(weak);
            ObjectId { slot, generation }
        } else {
            let slot = registry_state.slots.len();
            registry_state.slots.push(Slot {
                generation: 0,
                object: Some(weak),
            });
            ObjectId {
                slot,
                generation: 0,
            }
        };
        registry_state.live += 1;
        drop(registry_state);

        Ok(Self {
            object,
            registry,
            id,
            drop_hook: collect_on_drop::<T>,
            marker: PhantomData,
        })
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> ObjectId {
        self.id
    }

    pub(crate) fn key(&self) -> ObjectKey {
        ObjectKey {
            registry: self.registry.clone(),
            id: self.id,
        }
    }

    pub(crate) fn strong_count(&self) -> usize {
        Rc::strong_count(&self.object)
    }

    pub(crate) fn identity(&self) -> usize {
        Rc::as_ptr(&self.object) as usize
    }
}

impl<T: 'static> Deref for HeapRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.object
    }
}

impl<T: 'static> Drop for HeapRef<T> {
    fn drop(&mut self) {
        (self.drop_hook)(self);
    }
}

// Pin immediate children before clearing a uniquely owned node. Releasing the
// pins through this work list prevents recursive destruction without tracing
// unrelated live objects. Shared nodes (including cycles) stay for tracing GC.
type PendingDrop = (Rc<RefCell<Registry>>, ObjectId, Rc<dyn HeapObject>);

fn pin_children(object: &dyn HeapObject, work: &mut Vec<PendingDrop>) -> Result<(), RuntimeError> {
    let mut result = Ok(());
    object.visit_values(&mut |value| {
        if result.is_err() {
            return;
        }
        let traversal = value.visit_heap_objects(&mut |child| {
            if result.is_err() {
                return;
            }
            if work.try_reserve(1).is_err() {
                result = Err(RuntimeError::ResourceLimit {
                    resource: "VM heap cleanup",
                });
                return;
            }
            let object = {
                let state = child.registry.borrow();
                state
                    .slots
                    .get(child.id.slot)
                    .filter(|slot| slot.generation == child.id.generation)
                    .and_then(|slot| slot.object.as_ref())
                    .and_then(Weak::upgrade)
            };
            if let Some(object) = object {
                work.push((child.registry, child.id, object));
            }
        });
        if let Err(error) = traversal {
            result = Err(error);
        }
    });
    result
}

fn collect_on_drop<T: HeapObject>(value: &HeapRef<T>) {
    if COLLECTING.with(std::cell::Cell::get) || Rc::strong_count(&value.object) != 1 {
        return;
    }
    match value.object.has_heap_children() {
        Ok(false) => {
            value.registry.borrow_mut().retire(value.id);
            return;
        }
        Ok(true) => {}
        Err(_) => {
            std::mem::forget(value.object.clone());
            return;
        }
    }
    COLLECTING.with(|collecting| collecting.set(true));
    let _flag = CollectionFlag;
    let mut work = Vec::new();
    if pin_children(&*value.object, &mut work).is_err() {
        // Preserve the graph if cleanup storage is unavailable; ordinary Drop
        // could overflow the host stack. Never release partially pinned chains.
        std::mem::forget(work);
        std::mem::forget(value.object.clone());
        return;
    }
    value.object.clear_values();
    value.registry.borrow_mut().retire(value.id);
    while let Some((registry, id, object)) = work.pop() {
        if Rc::strong_count(&object) != 1 {
            continue;
        }
        if pin_children(&*object, &mut work).is_err() {
            std::mem::forget(work);
            std::mem::forget(object);
            return;
        }
        object.clear_values();
        registry.borrow_mut().retire(id);
    }
}

pub(crate) fn collection_due() -> bool {
    REGISTRY.with(|registry| {
        let state = registry.borrow();
        state.allocations_since_collection >= state.collection_threshold.max(256)
    })
}

pub(crate) fn live_objects() -> usize {
    REGISTRY.with(|registry| registry.borrow().live)
}

/// Collect unreachable managed cycles on the current thread's Cog heap.
/// Strongly held `Value` and `HeapRef` aliases remain roots.
pub fn collect_cycles() -> Result<usize, RuntimeError> {
    collect(&[])
}

/// Trace all objects, clear unreachable graph edges, then retire their slots.
/// `discarded` contains execution roots that are about to leave scope; their
/// references do not keep objects alive during this final collection.
pub(crate) fn collect(discarded: &[Value]) -> Result<usize, RuntimeError> {
    collect_with_excluded(discarded, &[])
}

fn collect_with_excluded(
    discarded_values: &[Value],
    discarded_ids: &[ObjectId],
) -> Result<usize, RuntimeError> {
    if COLLECTING.with(|collecting| collecting.replace(true)) {
        return Ok(0);
    }
    let _collection_flag = CollectionFlag;
    let registry = REGISTRY.with(Rc::clone);
    let (slot_count, snapshot) = {
        let state = registry.borrow();
        let count = state
            .slots
            .iter()
            .filter(|slot| slot.object.is_some())
            .count();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(count)
            .map_err(|_| RuntimeError::ResourceLimit {
                resource: "VM heap",
            })?;
        for (slot, entry) in state.slots.iter().enumerate() {
            if let Some(weak) = &entry.object {
                snapshot.push((
                    ObjectId {
                        slot,
                        generation: entry.generation,
                    },
                    weak.clone(),
                ));
            }
        }
        (state.slots.len(), snapshot)
    };
    let mut slot_to_index = Vec::new();
    slot_to_index
        .try_reserve_exact(slot_count)
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    slot_to_index.resize(slot_count, None);
    for (index, (id, _)) in snapshot.iter().enumerate() {
        slot_to_index[id.slot] = Some(index);
    }

    let mut incoming = Vec::new();
    incoming
        .try_reserve_exact(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    incoming.resize(snapshot.len(), 0usize);
    let mut edges = Vec::new();
    edges
        .try_reserve_exact(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    edges.resize_with(snapshot.len(), Vec::new);
    let mut edge_allocation_failed = false;
    for (index, (_, weak)) in snapshot.iter().enumerate() {
        let Some(object) = weak.upgrade() else {
            continue;
        };

        object.visit_values(&mut |value| {
            if edge_allocation_failed {
                return;
            }
            let result = value.visit_heap_objects(&mut |child| {
                let child_index = slot_to_index.get(child.id.slot).and_then(|index| *index);
                if Rc::ptr_eq(&registry, &child.registry)
                    && child_index.is_some_and(|child_index| snapshot[child_index].0 == child.id)
                {
                    let child_index = child_index.unwrap();
                    if edges[index].try_reserve(1).is_err() {
                        edge_allocation_failed = true;
                        return;
                    }
                    incoming[child_index] = incoming[child_index].saturating_add(1);
                    edges[index].push(child_index);
                }
            });
            if result.is_err() {
                edge_allocation_failed = true;
            }
        });
    }
    if edge_allocation_failed {
        return Err(RuntimeError::ResourceLimit {
            resource: "VM heap",
        });
    }

    let mut discarded_counts = Vec::new();
    discarded_counts
        .try_reserve_exact(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    discarded_counts.resize(snapshot.len(), 0usize);
    for value in discarded_values {
        value.visit_heap_objects(&mut |object| {
            let index = slot_to_index.get(object.id.slot).and_then(|index| *index);
            if Rc::ptr_eq(&registry, &object.registry)
                && index.is_some_and(|index| snapshot[index].0 == object.id)
            {
                let index = index.unwrap();
                discarded_counts[index] = discarded_counts[index].saturating_add(1);
            }
        })?;
    }
    for id in discarded_ids {
        let index = slot_to_index.get(id.slot).and_then(|index| *index);
        if index.is_some_and(|index| snapshot[index].0 == *id) {
            let index = index.unwrap();
            discarded_counts[index] = discarded_counts[index].saturating_add(1);
        }
    }

    let mut marked = Vec::new();
    marked
        .try_reserve_exact(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    marked.resize(snapshot.len(), false);
    let mut work = Vec::new();
    work.try_reserve_exact(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    for (index, (_, weak)) in snapshot.iter().enumerate() {
        let strong = weak.strong_count();
        let accounted = incoming[index].saturating_add(discarded_counts[index]);
        if strong > accounted {
            marked[index] = true;
            work.push(index);
        }
    }
    while let Some(index) = work.pop() {
        for &child in &edges[index] {
            if !marked[child] {
                marked[child] = true;
                work.push(child);
            }
        }
    }

    let unreachable = marked
        .iter()
        .zip(&snapshot)
        .filter(|(marked, (_, weak))| !**marked && weak.strong_count() != 0)
        .count();
    registry
        .borrow_mut()
        .free
        .try_reserve(snapshot.len())
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(unreachable)
        .map_err(|_| RuntimeError::ResourceLimit {
            resource: "VM heap",
        })?;
    for (index, (id, weak)) in snapshot.iter().enumerate() {
        if !marked[index]
            && let Some(object) = weak.upgrade()
        {
            objects.push((*id, object));
        }
    }

    // Keep every unreachable object alive while all edges are cleared. This
    // makes destruction iterative even for very deep chains.
    for (_, object) in &objects {
        object.clear_values();
    }
    drop(objects);

    let mut state = registry.borrow_mut();
    for (id, weak) in snapshot {
        if weak.strong_count() != 0 {
            continue;
        }
        let Some(slot) = state.slots.get_mut(id.slot) else {
            continue;
        };
        if slot.generation != id.generation
            || slot
                .object
                .as_ref()
                .is_none_or(|current| current.strong_count() != 0)
        {
            continue;
        }
        state.retire(id);
    }
    state.allocations_since_collection = 0;
    state.collection_threshold = state.live.max(256);
    Ok(unreachable)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::value::{ArrayValue, ClosureCaptures, RecordValue};

    #[test]
    fn collection_intervals_grow_with_the_surviving_heap() {
        collect_cycles().unwrap();
        let mut roots = Vec::new();
        let mut collections = 0;
        for _ in 0..16_384 {
            roots.push(HeapRef::new(RefCell::new(Vec::<u8>::new())));
            if collection_due() {
                collect_cycles().unwrap();
                collections += 1;
            }
        }
        assert!(
            collections <= 8,
            "{collections} complete traces for a growing live heap"
        );
        assert_eq!(live_objects(), roots.len());
        drop(roots);
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn temporary_container_cleanup_does_not_trace_unrelated_or_shared_graphs() {
        struct Counted(Rc<std::cell::Cell<usize>>);
        impl HeapObject for Counted {
            fn visit_values(&self, _: &mut dyn FnMut(&Value)) {
                self.0.set(self.0.get() + 1);
            }
            fn clear_values(&self) {}
        }
        let visits = Rc::new(std::cell::Cell::new(0));
        let roots = (0..2048)
            .map(|_| HeapRef::new(Counted(visits.clone())))
            .collect::<Vec<_>>();
        let shared = HeapRef::new(ArrayValue::new(vec![Value::Int(42)]).unwrap());
        for _ in 0..256 {
            let child = HeapRef::new(ArrayValue::new(vec![Value::Array(shared.clone())]).unwrap());
            drop(HeapRef::new(RecordValue::new(HashMap::from([(
                "child".into(),
                Value::Array(child),
            )]))));
            assert_eq!(live_objects(), roots.len() + 1);
        }
        assert_eq!(
            visits.get(),
            0,
            "temporary drops traced the entire registry"
        );
        assert_eq!(shared.borrow().len(), 1);
        drop(roots);
        drop(shared);
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn forgotten_cleanup_roots_remain_in_the_live_object_budget() {
        let before = live_objects();
        let value = HeapRef::new(RecordValue::new(HashMap::new()));
        std::mem::forget(value);
        assert_eq!(live_objects(), before + 1);
    }

    #[test]
    fn collection_breaks_cycles_and_reuses_slots_with_a_new_generation() {
        let value = HeapRef::new(RecordValue::new(HashMap::new()));
        value
            .borrow_mut()
            .insert("self".into(), Value::Record(value.clone()));
        let before = value.id();
        assert_eq!(collect(&[]).unwrap(), 0);
        drop(value);

        assert_eq!(collect(&[]).unwrap(), 1);
        let next = HeapRef::new(ArrayValue::new(Vec::new()).unwrap());
        assert_eq!(next.id().slot, before.slot);
        assert_ne!(next.id().generation, before.generation);
    }

    #[test]
    fn live_external_alias_is_a_root() {
        let array = HeapRef::new(ArrayValue::new(vec![Value::Int(1)]).unwrap());
        let alias = array.clone();
        assert_eq!(collect(&[]).unwrap(), 0);
        assert_eq!(alias.borrow().len(), 1);
    }

    #[test]
    fn collection_traces_cycles_through_closure_captures() {
        let cell = HeapRef::new(RefCell::new(Value::Nil));
        let captures = HeapRef::new(ClosureCaptures::new(vec![Value::Cell(cell.clone())]));
        *cell.borrow_mut() = Value::Closure(7, captures.clone());
        drop(cell);
        drop(captures);
        assert_eq!(collect(&[]).unwrap(), 2);
    }

    #[test]
    fn external_root_keeps_entire_ring_alive_across_graph_sizes() {
        for count in 1..=128 {
            let cells = (0..count)
                .map(|_| HeapRef::new(RefCell::new(Value::Nil)))
                .collect::<Vec<_>>();
            for index in 0..count {
                *cells[index].borrow_mut() = Value::Cell(cells[(index + 1) % count].clone());
            }

            let root = cells[count / 2].clone();
            drop(cells);
            assert_eq!(collect(&[]).unwrap(), 0, "ring size {count}");
            assert!(matches!(*root.borrow(), Value::Cell(_)));

            drop(root);
            assert_eq!(collect(&[]).unwrap(), count, "ring size {count}");
            assert_eq!(live_objects(), 0, "ring size {count}");
        }
    }

    #[test]
    fn duplicated_edges_and_an_external_alias_are_counted_as_roots() {
        let record = HeapRef::new(RecordValue::new(HashMap::new()));
        let cell = HeapRef::new(RefCell::new(Value::Nil));
        {
            let mut fields = record.borrow_mut();
            fields.insert("first".into(), Value::Cell(cell.clone()));
            fields.insert("second".into(), Value::Cell(cell.clone()));
        }
        *cell.borrow_mut() = Value::Record(record.clone());

        let root = cell.clone();
        drop(record);
        drop(cell);
        assert_eq!(collect(&[]).unwrap(), 0);
        assert!(matches!(*root.borrow(), Value::Record(_)));

        drop(root);
        assert_eq!(collect(&[]).unwrap(), 2);
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn dropping_a_deep_record_chain_is_iterative() {
        let mut value = Value::Int(0);
        for _ in 0..100_000 {
            value = Value::Record(HeapRef::new(RecordValue::new(HashMap::from([(
                "next".into(),
                value,
            )]))));
        }
        drop(value);
        assert_eq!(live_objects(), 0);
    }
}
