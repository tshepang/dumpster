/*
   dumpster, a cycle-tracking garbage collector for Rust.
   Copyright (C) 2023 Clayton Ramsey.

   This program is free software: you can redistribute it and/or modify
   it under the terms of the GNU General Public License as published by
   the Free Software Foundation, either version 3 of the License, or
   (at your option) any later version.

   This program is distributed in the hope that it will be useful,
   but WITHOUT ANY WARRANTY; without even the implied warranty of
   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
   GNU General Public License for more details.

   You should have received a copy of the GNU General Public License
   along with this program.  If not, see <http://www.gnu.org/licenses/>.
*/

//! Thread-local garbage collection.
//!
//! Most users of this library will want to direct their attention to [`Gc`].
//! If you want to tune the garbage collector's cleanup frequency, take a look at
//! [`set_collect_condition`].
//!
//! # Examples
//!
//! ```
//! use dumpster::{unsync::Gc, Collectable};
//! use std::cell::RefCell;
//!
//! #[derive(Collectable)]
//! struct Foo {
//!     refs: RefCell<Vec<Gc<Self>>>,
//! }
//!
//! let foo = Gc::new(Foo {
//!     refs: RefCell::new(Vec::new()),
//! });
//!
//! // If you had used `Rc`, this would be a memory leak.
//! // `Gc` can collect it, though!
//! foo.refs.borrow_mut().push(foo.clone());
//! ```

use std::{
    alloc::{dealloc, Layout},
    borrow::Borrow,
    cell::Cell,
    num::NonZeroUsize,
    ops::Deref,
    ptr::{addr_of, addr_of_mut, drop_in_place, NonNull},
};

use crate::{Collectable, Visitor};

use self::collect::{Dumpster, COLLECTING, DUMPSTER};

mod collect;
#[cfg(test)]
mod tests;

#[derive(Debug)]
/// A garbage-collected pointer.
///
/// This garbage-collected pointer may be used for data which is not safe to share across threads
/// (such as a [`std::cell::RefCell`]).
/// It can also be used for variably sized data.
///
/// # Examples
///
/// ```
/// use dumpster::unsync::Gc;
///
/// let x: Gc<u8> = Gc::new(3);
///
/// println!("{}", *x); // prints '3'
///                     // x is then freed automatically!
/// ```
pub struct Gc<T: Collectable + ?Sized + 'static> {
    /// A pointer to the heap allocation containing the data under concern.
    /// The pointee box should never be mutated.
    ptr: NonNull<GcBox<T>>,
}

/// Collect all existing unreachable allocations.
///
/// This operation is most useful for making sure that the `Drop` implementation for some data has
/// been called before moving on (such as for a file handle or mutex guard), because the garbage
/// collector is not eager under normal conditions.
/// This only collects the allocations local to the caller's thread.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
/// use dumpster::unsync::{collect, Gc};
/// use std::sync::Mutex;
///
/// static MY_MUTEX: Mutex<()> = Mutex::new(());
///
/// let guard_gc = Gc::new(MY_MUTEX.lock()?);
/// drop(guard_gc);
/// // We're not certain that the handle that was contained in `guard_gc` has been dropped, so we
/// // should force a collection to make sure.
/// collect();
///
/// // We know this won't cause a deadlock because we made sure to run a collection.
/// let _x = MY_MUTEX.lock()?;
/// # Ok(())
/// # }
/// ```
pub fn collect() {
    DUMPSTER.with(Dumpster::collect_all);
}

/// Information passed to a [`CollectCondition`] used to determine whether the garbage collector
/// should start collecting.
pub struct CollectInfo {
    /// Dummy value so this is a private structure.
    _private: (),
}

/// A function which determines whether the garbage collector should start collecting.
/// This function primarily exists so that it can be used with [`set_collect_condition`].
///
/// # Examples
///
/// ```rust
/// use dumpster::unsync::{set_collect_condition, CollectInfo};
///
/// fn always_collect(_: &CollectInfo) -> bool {
///     true
/// }
///
/// set_collect_condition(always_collect);
/// ```
pub type CollectCondition = fn(&CollectInfo) -> bool;

#[must_use]
/// The default collection condition used by the garbage collector.
///
/// There are no guarantees about what this function returns, other than that it will return `true`
/// with sufficient frequency to ensure that all `Gc` operations are amortized _O(1)_ in runtime.
///
/// This function isn't really meant to be called by users, but rather it's supposed to be handed
/// off to [`set_collect_condition`] to return to the default operating mode of the library.
///
/// This collection condition applies locally, i.e. only to this thread.
/// If you want it to apply globally, you'll have to update it every time you spawn a thread.
///
/// # Examples
///
/// ```rust
/// use dumpster::unsync::{default_collect_condition, set_collect_condition};
///
/// set_collect_condition(default_collect_condition);
/// ```
pub fn default_collect_condition(info: &CollectInfo) -> bool {
    info.n_gcs_dropped_since_last_collect() > info.n_gcs_existing()
}

#[allow(clippy::missing_panics_doc)]
/// Set the function which determines whether the garbage collector should be run.
///
/// `f` will be periodically called by the garbage collector to determine whether it should perform
/// a full cleanup of the heap.
/// When `f` returns true, a cleanup will begin.
///
/// # Examples
///
/// ```
/// use dumpster::unsync::{set_collect_condition, CollectInfo};
///
/// /// This function will make sure a GC cleanup never happens unless directly activated.
/// fn never_collect(_: &CollectInfo) -> bool {
///     false
/// }
///
/// set_collect_condition(never_collect);
/// ```
pub fn set_collect_condition(f: CollectCondition) {
    DUMPSTER.with(|d| d.collect_condition.set(f));
}

#[repr(C)]
/// The underlying heap allocation for a [`Gc`].
struct GcBox<T: Collectable + ?Sized> {
    /// The number of extant references to this garbage-collected data.
    /// If the stored reference count is zero, then this value is a "zombie" - in the process of
    /// being dropped - and should not be dropped again.
    ref_count: Cell<NonZeroUsize>,
    /// The stored value inside this garbage-collected box.
    value: T,
}

impl<T: Collectable + ?Sized> Gc<T> {
    /// Construct a new garbage-collected allocation, with `value` as its value.
    pub fn new(value: T) -> Gc<T>
    where
        T: Sized,
    {
        DUMPSTER.with(Dumpster::notify_created_gc);
        Gc {
            ptr: Box::leak(Box::new(GcBox {
                ref_count: Cell::new(NonZeroUsize::MIN),
                value,
            }))
            .into(),
        }
    }
}

impl<T: Collectable + ?Sized> Deref for Gc<T> {
    type Target = T;

    /// Dereference this pointer, creating a reference to the contained value `T`.
    ///
    /// # Panics
    ///
    /// This function may panic if it is called from within the implementation of `std::ops::Drop`
    /// of its owning value, since returning such a reference could cause a use-after-free.
    /// It is not guaranteed to panic.
    ///
    /// # Examples
    ///
    /// The following is a correct time to dereference a `Gc`.
    ///
    /// ```
    /// use dumpster::unsync::Gc;
    ///
    /// let my_gc = Gc::new(0u8);
    /// let my_ref: &u8 = &my_gc;
    /// ```
    ///
    /// Dereferencing a `Gc` while dropping is not correct.
    ///
    /// ```should_panic
    /// // This is wrong!
    /// use std::cell::RefCell;
    /// use dumpster::{unsync::Gc, Collectable};
    ///
    /// #[derive(Collectable)]
    /// struct Bad {
    ///     s: String,
    ///     cycle: RefCell<Option<Gc<Bad>>>,
    /// }
    ///
    /// impl Drop for Bad {
    ///     fn drop(&mut self) {
    ///         // The second time this `print` is executed it will try to
    ///         // print a `String` that has already been dropped.
    ///         println!("{}", self.cycle.borrow().as_ref().unwrap().s)
    ///     }
    /// }
    ///
    /// let foo = Gc::new(Bad {
    ///     s: "foo".to_string(),
    ///     cycle: RefCell::new(None),
    /// });
    /// ```
    fn deref(&self) -> &Self::Target {
        assert!(
            !COLLECTING.with(Cell::get),
            "dereferencing GC to already-collected object"
        );
        unsafe { &self.ptr.as_ref().value }
    }
}

impl<T: Collectable + ?Sized> Clone for Gc<T> {
    #[allow(clippy::clone_on_copy)]
    /// Create a duplicate reference to the same data pointed to by `self`.
    /// This does not duplicate the data.
    fn clone(&self) -> Self {
        unsafe {
            let box_ref = self.ptr.as_ref();
            box_ref
                .ref_count
                .set(box_ref.ref_count.get().saturating_add(1));
        }
        DUMPSTER.with(|d| {
            d.notify_created_gc();
            // d.mark_cleaned(self.ptr);
        });
        Self {
            ptr: self.ptr.clone(),
        }
    }
}

impl<T: Collectable + ?Sized> Drop for Gc<T> {
    /// Destroy this garbage-collected pointer.
    ///
    /// If this is the last reference which can reach the pointed-to data, the allocation that it
    /// points to will be destroyed.
    fn drop(&mut self) {
        if COLLECTING.with(Cell::get) {
            return;
        }
        DUMPSTER.with(|d| {
            let box_ref = unsafe { self.ptr.as_ref() };
            match box_ref.ref_count.get() {
                NonZeroUsize::MIN => {
                    d.mark_cleaned(self.ptr);
                    unsafe {
                        // this was the last reference, drop unconditionally
                        drop_in_place(addr_of_mut!(self.ptr.as_mut().value));
                        // note: `box_ref` is no longer usable
                        dealloc(
                            self.ptr.as_ptr().cast::<u8>(),
                            Layout::for_value(self.ptr.as_ref()),
                        );
                    }
                }
                n => {
                    // decrement the ref count - but another reference to this data still
                    // lives
                    box_ref
                        .ref_count
                        .set(NonZeroUsize::new(n.get() - 1).unwrap());
                    // remaining references could be a cycle - therefore, mark it as dirty
                    // so we can check later
                    d.mark_dirty(self.ptr);
                }
            }
            // Notify that a GC has been dropped, potentially triggering a cleanup
            d.notify_dropped_gc();
        });
    }
}

impl CollectInfo {
    #[must_use]
    /// Get the number of times that a [`Gc`] has been dropped since the last time a collection
    /// operation was performed.
    ///
    /// # Examples
    ///
    /// ```
    /// use dumpster::unsync::{set_collect_condition, CollectInfo};
    ///
    /// // Collection condition for whether many Gc's have been dropped.
    /// fn have_many_gcs_dropped(info: &CollectInfo) -> bool {
    ///     info.n_gcs_dropped_since_last_collect() > 100
    /// }
    ///
    /// set_collect_condition(have_many_gcs_dropped);
    /// ```
    pub fn n_gcs_dropped_since_last_collect(&self) -> usize {
        DUMPSTER.with(|d| d.n_ref_drops.get())
    }

    #[must_use]
    /// Get the total number of [`Gc`]s which currently exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use dumpster::unsync::{set_collect_condition, CollectInfo};
    ///
    /// // Collection condition for whether many Gc's currently exist.
    /// fn do_many_gcs_exist(info: &CollectInfo) -> bool {
    ///     info.n_gcs_existing() > 100
    /// }
    ///
    /// set_collect_condition(do_many_gcs_exist);
    /// ```
    pub fn n_gcs_existing(&self) -> usize {
        DUMPSTER.with(|d| d.n_refs_living.get())
    }
}

unsafe impl<T: Collectable + ?Sized> Collectable for Gc<T> {
    fn accept<V: Visitor>(&self, visitor: &mut V) -> Result<(), ()> {
        visitor.visit_unsync(self);
        Ok(())
    }
}

impl<T: Collectable + ?Sized> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        // DUMPSTER.with(|d| d.mark_cleaned(self.ptr));
        unsafe { addr_of!(self.ptr.as_ref().value).as_ref().unwrap() }
    }
}

impl<T: Collectable + ?Sized> Borrow<T> for Gc<T> {
    fn borrow(&self) -> &T {
        self.as_ref()
    }
}

impl<T: Collectable + Default> Default for Gc<T> {
    fn default() -> Self {
        Gc::new(T::default())
    }
}

impl<T: Collectable + ?Sized> std::fmt::Pointer for Gc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Pointer::fmt(&addr_of!(**self), f)
    }
}

#[cfg(feature = "coerce-unsized")]
impl<T, U> std::ops::CoerceUnsized<Gc<U>> for Gc<T>
where
    T: std::marker::Unsize<U> + Collectable + ?Sized,
    U: Collectable + ?Sized,
{
}
