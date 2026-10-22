//! Reference-counted interning: equal values are stored once and referred to by a `u32` id.
//!
//! The index interns every repeated string (tenants, namespaces, service names, SDK versions,
//! header names and values) and every service shape (`index::ServiceShape`): the versions of a
//! tenant's service almost always share one, and tenants running the same code share it across
//! tenants. A value is freed when its last reference is released, so the pools hold exactly what
//! the live endpoints use.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

/// An interned value's id in its [`Pool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(u32);

#[derive(Debug)]
pub struct Pool<T: ?Sized> {
    ids: HashMap<Arc<T>, Id>,
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

#[derive(Debug)]
struct Slot<T: ?Sized> {
    value: Option<Arc<T>>,
    refs: u32,
}

impl<T: ?Sized> Default for Pool<T> {
    fn default() -> Self {
        Self {
            ids: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T: ?Sized + Hash + Eq> Pool<T> {
    /// The id of `value`, interning it if needed, with one more reference.
    pub fn intern<Q>(&mut self, value: &Q) -> Id
    where
        Q: ?Sized + Hash + Eq,
        Arc<T>: Borrow<Q> + for<'a> From<&'a Q>,
    {
        if let Some(&id) = self.ids.get(value) {
            self.slots[id.0 as usize].refs += 1;
            return id;
        }
        self.insert(Arc::from(value))
    }

    /// [`Pool::intern`] of an owned value: no copy when it's new.
    pub fn intern_owned(&mut self, value: T) -> Id
    where
        T: Sized,
    {
        if let Some(&id) = self.ids.get(&value) {
            self.slots[id.0 as usize].refs += 1;
            return id;
        }
        self.insert(Arc::new(value))
    }

    fn insert(&mut self, value: Arc<T>) -> Id {
        let slot = Slot {
            value: Some(Arc::clone(&value)),
            refs: 1,
        };
        let id = match self.free.pop() {
            Some(index) => {
                self.slots[index as usize] = slot;
                Id(index)
            }
            None => {
                let index = u32::try_from(self.slots.len()).expect("fewer than 2^32 values");
                self.slots.push(slot);
                Id(index)
            }
        };
        self.ids.insert(value, id);
        id
    }

    /// The id of `value` if it's interned, without taking a reference.
    pub fn find<Q>(&self, value: &Q) -> Option<Id>
    where
        Q: ?Sized + Hash + Eq,
        Arc<T>: Borrow<Q>,
    {
        self.ids.get(value).copied()
    }

    /// Drops a reference to `id`, freeing the value with the last one.
    pub fn release(&mut self, id: Id) {
        let slot = &mut self.slots[id.0 as usize];
        slot.refs -= 1;
        if slot.refs == 0 {
            let value = slot.value.take().expect("a referenced slot has its value");
            self.ids.remove(&value);
            self.free.push(id.0);
        }
    }

    pub fn get(&self, id: Id) -> &T {
        self.slots[id.0 as usize]
            .value
            .as_deref()
            .expect("a referenced slot has its value")
    }

    /// A shared handle on the value, usable outside the index's lock.
    pub fn get_arc(&self, id: Id) -> Arc<T> {
        Arc::clone(
            self.slots[id.0 as usize]
                .value
                .as_ref()
                .expect("a referenced slot has its value"),
        )
    }

    /// The number of distinct live values.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_values_share_an_id() {
        let mut pool: Pool<str> = Pool::default();
        let a = pool.intern("checkout");
        let b = pool.intern("checkout");
        let c = pool.intern("cart");

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.get(a), "checkout");
    }

    #[test]
    fn the_last_release_frees_the_value() {
        let mut pool: Pool<str> = Pool::default();
        let a = pool.intern("checkout");
        pool.intern("checkout");

        pool.release(a);
        assert_eq!(pool.find("checkout"), Some(a));
        pool.release(a);

        assert_eq!(pool.find("checkout"), None);
        assert_eq!(pool.len(), 0);
        // The slot is reused.
        assert_eq!(pool.intern("cart"), a);
    }

    #[test]
    fn owned_values() {
        let mut pool: Pool<Vec<u8>> = Pool::default();
        let a = pool.intern_owned(vec![1, 2]);
        let b = pool.intern_owned(vec![1, 2]);

        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }
}
