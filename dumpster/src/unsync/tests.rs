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

//! Simple tests using manual implementations of [`Collectable`].

use crate::Visitor;

use super::*;
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};

#[test]
/// Test a simple data structure
fn simple() {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct Foo(u8);

    impl Drop for Foo {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::Relaxed);
        }
    }

    unsafe impl Collectable for Foo {
        fn accept<V: Visitor>(&self, _: &mut V) -> Result<(), ()> {
            Ok(())
        }
    }

    let gc1 = Gc::new(Foo(1));
    let gc2 = Gc::clone(&gc1);

    assert!(!DROPPED.load(Ordering::Relaxed));

    drop(gc1);

    assert!(!DROPPED.load(Ordering::Relaxed));

    drop(gc2);

    assert!(DROPPED.load(Ordering::Relaxed));
}

#[derive(Debug)]
struct MultiRef {
    refs: RefCell<Vec<Gc<MultiRef>>>,
    drop_count: &'static AtomicUsize,
}

unsafe impl Collectable for MultiRef {
    fn accept<V: Visitor>(&self, visitor: &mut V) -> Result<(), ()> {
        self.refs.accept(visitor)
    }
}

impl Drop for MultiRef {
    fn drop(&mut self) {
        self.drop_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn self_referential() {
    static DROPPED: AtomicU8 = AtomicU8::new(0);
    struct Foo(RefCell<Option<Gc<Foo>>>);

    unsafe impl Collectable for Foo {
        #[inline]
        fn accept<V: Visitor>(&self, visitor: &mut V) -> Result<(), ()> {
            self.0.accept(visitor)
        }
    }

    impl Drop for Foo {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    let gc = Gc::new(Foo(RefCell::new(None)));
    gc.0.replace(Some(Gc::clone(&gc)));

    assert_eq!(DROPPED.load(Ordering::Relaxed), 0);
    drop(gc);
    collect();
    assert_eq!(DROPPED.load(Ordering::Relaxed), 1);
}

#[test]
fn cyclic() {
    static DROPPED: AtomicU8 = AtomicU8::new(0);
    struct Foo(RefCell<Option<Gc<Foo>>>);

    unsafe impl Collectable for Foo {
        #[inline]
        fn accept<V: Visitor>(&self, visitor: &mut V) -> Result<(), ()> {
            self.0.accept(visitor)
        }
    }

    impl Drop for Foo {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    let foo1 = Gc::new(Foo(RefCell::new(None)));
    let foo2 = Gc::new(Foo(RefCell::new(Some(Gc::clone(&foo1)))));
    foo1.0.replace(Some(Gc::clone(&foo2)));

    assert_eq!(DROPPED.load(Ordering::Relaxed), 0);
    drop(foo1);
    assert_eq!(DROPPED.load(Ordering::Relaxed), 0);
    drop(foo2);
    collect();
    assert_eq!(DROPPED.load(Ordering::Relaxed), 2);
}

/// Construct a complete graph of garbage-collected
fn complete_graph(detectors: &'static [AtomicUsize]) -> Vec<Gc<MultiRef>> {
    let mut gcs = Vec::new();
    for d in detectors {
        let gc = Gc::new(MultiRef {
            refs: RefCell::new(Vec::new()),
            drop_count: d,
        });
        for x in &gcs {
            gc.refs.borrow_mut().push(Gc::clone(x));
            x.refs.borrow_mut().push(Gc::clone(&gc));
        }
        gcs.push(gc);
    }

    gcs
}

#[test]
fn complete4() {
    static DETECTORS: [AtomicUsize; 4] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];

    let mut gcs = complete_graph(&DETECTORS);

    for _ in 0..3 {
        gcs.pop();
    }

    for detector in &DETECTORS {
        assert_eq!(detector.load(Ordering::Relaxed), 0);
    }

    drop(gcs);
    collect();

    for detector in &DETECTORS {
        assert_eq!(detector.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn parallel_loop() {
    static COUNT_1: AtomicUsize = AtomicUsize::new(0);
    static COUNT_2: AtomicUsize = AtomicUsize::new(0);
    static COUNT_3: AtomicUsize = AtomicUsize::new(0);
    static COUNT_4: AtomicUsize = AtomicUsize::new(0);

    let gc1 = Gc::new(MultiRef {
        drop_count: &COUNT_1,
        refs: RefCell::new(Vec::new()),
    });
    let gc2 = Gc::new(MultiRef {
        drop_count: &COUNT_2,
        refs: RefCell::new(vec![Gc::clone(&gc1)]),
    });
    let gc3 = Gc::new(MultiRef {
        drop_count: &COUNT_3,
        refs: RefCell::new(vec![Gc::clone(&gc1)]),
    });
    let gc4 = Gc::new(MultiRef {
        drop_count: &COUNT_4,
        refs: RefCell::new(vec![Gc::clone(&gc2), Gc::clone(&gc3)]),
    });
    gc1.refs.borrow_mut().push(Gc::clone(&gc4));

    assert_eq!(COUNT_1.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_2.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_3.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_4.load(Ordering::Relaxed), 0);
    drop(gc1);
    assert_eq!(COUNT_1.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_2.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_3.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_4.load(Ordering::Relaxed), 0);
    drop(gc2);
    assert_eq!(COUNT_1.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_2.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_3.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_4.load(Ordering::Relaxed), 0);
    drop(gc3);
    assert_eq!(COUNT_1.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_2.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_3.load(Ordering::Relaxed), 0);
    assert_eq!(COUNT_4.load(Ordering::Relaxed), 0);
    drop(gc4);
    collect();
    assert_eq!(COUNT_1.load(Ordering::Relaxed), 1);
    assert_eq!(COUNT_2.load(Ordering::Relaxed), 1);
    assert_eq!(COUNT_3.load(Ordering::Relaxed), 1);
    assert_eq!(COUNT_4.load(Ordering::Relaxed), 1);
}

#[test]
/// Check that we can drop a Gc which points to some allocation with a borrowed `RefCell` in it.
fn double_borrow() {
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    let gc = Gc::new(MultiRef {
        refs: RefCell::new(Vec::new()),
        drop_count: &DROP_COUNT,
    });
    gc.refs.borrow_mut().push(gc.clone());
    let mut my_borrow = gc.refs.borrow_mut();
    my_borrow.pop();
    drop(my_borrow);

    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 0);
    collect();
    drop(gc);
    collect();
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);
}

#[test]
#[cfg(feature = "coerce-unsized")]
fn coerce_array() {
    let gc1: Gc<[u8; 3]> = Gc::new([0, 0, 0]);
    let gc2: Gc<[u8]> = gc1;
    assert_eq!(gc2.len(), 3);
    assert_eq!(
        std::mem::size_of::<Gc<[u8]>>(),
        2 * std::mem::size_of::<usize>()
    );
}
