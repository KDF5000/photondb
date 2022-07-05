use std::{
    mem::MaybeUninit,
    ops::Index,
    ptr::null_mut,
    sync::{
        atomic::{AtomicPtr, AtomicUsize, Ordering},
        Arc,
    },
};

use crossbeam_epoch::Guard;

#[derive(Clone)]
pub struct PageTable {
    inner: Arc<Inner>,
}

impl PageTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }

    pub fn get(&self, id: usize) -> usize {
        self.inner[id].load(Ordering::Acquire)
    }

    pub fn set(&self, id: usize, ptr: usize) {
        self.inner[id].store(ptr, Ordering::Release);
    }

    pub fn cas(&self, id: usize, old: usize, new: usize) -> Result<usize, usize> {
        self.inner[id].compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
    }

    pub fn alloc(&self, _: &Guard) -> Option<usize> {
        self.inner.alloc()
    }

    pub fn dealloc(&self, id: usize, guard: &Guard) {
        let inner = self.inner.clone();
        guard.defer(move || {
            inner.dealloc(id);
        })
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::new()
    }
}

struct Inner {
    // Level 0: [0, L0_MAX)
    l0: Box<L0<L0_LEN>>,
    // Level 1: [L0_MAX, L1_MAX)
    l1: Box<L1<L1_LEN>>,
    // Level 2: [L1_MAX, L2_MAX)
    l2: Box<L2<L2_LEN>>,
    // The next id to allocate.
    next: AtomicUsize,
    // The head of the free list.
    free: AtomicUsize,
}

impl Inner {
    fn new() -> Self {
        Self {
            l0: Box::default(),
            l1: Box::default(),
            l2: Box::default(),
            next: AtomicUsize::new(0),
            free: AtomicUsize::new(L2_MAX),
        }
    }

    pub fn alloc(&self) -> Option<usize> {
        let mut id = self.free.load(Ordering::Acquire);
        while id != L2_MAX {
            let next = self.index(id).load(Ordering::Acquire);
            match self
                .free
                .compare_exchange(id, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => id = actual,
            }
        }
        if id == L2_MAX {
            id = self.next.load(Ordering::Relaxed);
            if id < L2_MAX {
                id = self.next.fetch_add(1, Ordering::Relaxed);
            }
        }
        if id < L2_MAX {
            Some(id)
        } else {
            None
        }
    }

    pub fn dealloc(&self, id: usize) {
        let mut next = self.free.load(Ordering::Acquire);
        loop {
            self.index(id).store(next, Ordering::Release);
            match self
                .free
                .compare_exchange(next, id, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => next = actual,
            }
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<usize> for Inner {
    type Output = AtomicUsize;

    fn index(&self, index: usize) -> &Self::Output {
        if index < L0_MAX {
            self.l0.index(index)
        } else if index < L1_MAX {
            self.l1.index(index - L0_MAX)
        } else if index < L2_MAX {
            self.l2.index(index - L1_MAX)
        } else {
            unreachable!()
        }
    }
}

struct L0<const N: usize>([AtomicUsize; N]);

impl<const N: usize> Default for L0<N> {
    fn default() -> Self {
        Self(unsafe { MaybeUninit::zeroed().assume_init() })
    }
}

impl<const N: usize> Index<usize> for L0<N> {
    type Output = AtomicUsize;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

macro_rules! define_level {
    ($level:ident, $child:ty, $fanout:expr) => {
        struct $level<const N: usize>([AtomicPtr<$child>; N]);

        impl<const N: usize> Default for $level<N> {
            fn default() -> Self {
                Self(unsafe { MaybeUninit::zeroed().assume_init() })
            }
        }

        impl<const N: usize> Drop for $level<N> {
            fn drop(&mut self) {
                for child in &self.0 {
                    let ptr = child.load(Ordering::Relaxed);
                    if !ptr.is_null() {
                        unsafe {
                            Box::from_raw(ptr);
                        }
                    }
                }
            }
        }

        impl<const N: usize> Index<usize> for $level<N> {
            type Output = AtomicUsize;

            fn index(&self, index: usize) -> &Self::Output {
                let i = index / $fanout;
                let j = index % $fanout;
                let p = self.0[i].load(Ordering::Relaxed);
                let child = unsafe {
                    p.as_ref()
                        .unwrap_or_else(|| self.install_or_acquire_child(i))
                };
                child.index(j)
            }
        }

        impl<const N: usize> $level<N> {
            #[cold]
            fn install_or_acquire_child(&self, index: usize) -> &$child {
                let mut child = Box::into_raw(Box::default());
                if let Err(current) = self.0[index].compare_exchange(
                    null_mut(),
                    child,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    unsafe {
                        Box::from_raw(child);
                    }
                    child = current;
                }
                unsafe { &*child }
            }
        }
    };
}

const FANOUT: usize = 1 << 16;
const L0_LEN: usize = FANOUT;
const L1_LEN: usize = FANOUT - 1;
const L2_LEN: usize = FANOUT - 1;
const L0_MAX: usize = FANOUT;
const L1_MAX: usize = L0_MAX * FANOUT;
const L2_MAX: usize = L1_MAX * FANOUT;

define_level!(L1, L0<FANOUT>, L0_MAX);
define_level!(L2, L1<FANOUT>, L1_MAX);

#[cfg(test)]
mod test {
    extern crate test;

    use crossbeam_epoch::unprotected;
    use test::{black_box, Bencher};

    use super::*;

    const N: usize = 1 << 10;

    #[test]
    fn test_alloc() {
        let guard = unsafe { unprotected() };
        let table = PageTable::default();
        assert_eq!(table.alloc(guard), Some(0));
        assert_eq!(table.alloc(guard), Some(1));
        table.dealloc(0, guard);
        table.dealloc(1, guard);
        assert_eq!(table.alloc(guard), Some(1));
        assert_eq!(table.alloc(guard), Some(0));
    }

    #[test]
    fn test_index() {
        let table = PageTable::default();
        for i in [0, L0_MAX - 1, L0_MAX, L1_MAX - 1, L1_MAX, L2_MAX - 1] {
            table.set(i, i);
            assert_eq!(table.get(i), i);
        }
    }

    fn bench<T: Default + Index<usize>>(b: &mut Bencher, start: usize) {
        let l: Box<T> = Box::default();
        for i in start..(start + N) {
            l.index(i);
        }
        b.iter(|| {
            for i in start..(start + N) {
                black_box(l.index(i));
            }
        })
    }

    #[bench]
    fn bench_l0(b: &mut Bencher) {
        bench::<L0<FANOUT>>(b, 0);
    }

    #[bench]
    fn bench_l1(b: &mut Bencher) {
        bench::<L1<FANOUT>>(b, L0_MAX);
    }

    #[bench]
    fn bench_l2(b: &mut Bencher) {
        bench::<L2<FANOUT>>(b, L1_MAX);
    }

    #[bench]
    fn bench_inner_l0(b: &mut Bencher) {
        bench::<Inner>(b, 0);
    }

    #[bench]
    fn bench_inner_l1(b: &mut Bencher) {
        bench::<Inner>(b, L0_MAX);
    }

    #[bench]
    fn bench_inner_l2(b: &mut Bencher) {
        bench::<Inner>(b, L1_MAX);
    }
}
