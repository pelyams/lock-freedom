use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

// limited by HazardPointerArray's thread_registry bitmap size, i.e. 64
const MAX_THREADS: usize = 4;
// limited by HazardPointerGuard's available_indices bitmap size, i.e. 64
const HP_PER_THREAD: usize = 16;
const SCAN_THRESHOLD: usize = 2 * HP_PER_THREAD;

pub struct HazardPointerArray<T> {
    p_list: [AtomicPtr<T>; MAX_THREADS * HP_PER_THREAD],
    // in this bitmap, 1's stand for ready-to-use slots (sub-arrays) in p_array
    thread_registry: AtomicU64,
}

impl<T> HazardPointerArray<T> {
    pub const fn new() -> Self {
        const NULL_PTR: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
        let pointers: [AtomicPtr<()>; MAX_THREADS * HP_PER_THREAD] =
            [NULL_PTR; MAX_THREADS * HP_PER_THREAD];

        assert!(MAX_THREADS <= 64, "MAX_THREADS must be less or equal to 64");
        let thread_registry = !0 >> (64 - MAX_THREADS);

        Self {
            p_list: unsafe { std::mem::transmute(pointers) },
            thread_registry: AtomicU64::new(thread_registry),
        }
    }

    pub fn register_thread(&self) -> Result<HazardPointerGuard<T>, RegisterThreadError> {
        // вообще долбиться в цикле тоже не хочется, конечно
        // мб следует как-то помечать забронированные индексы?
        loop {
            let thread_registry = self.thread_registry.load(Ordering::Relaxed);
            if thread_registry == 0 {
                return Err(RegisterThreadError::NoAvailableIndices);
            } else {
                let tr_first_slot = thread_registry.trailing_zeros() as usize;
                if self
                    .thread_registry
                    .compare_exchange_weak(
                        thread_registry,
                        thread_registry ^ (1 << tr_first_slot),
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return Ok(HazardPointerGuard {
                        array: &self,
                        starting_idx: tr_first_slot * HP_PER_THREAD,
                        available_indices: Cell::new(!0 >> (64 - HP_PER_THREAD)),
                        d_list: RefCell::new(Vec::new()),
                    });
                }
            }
        }
    }
}

unsafe impl<T> Send for HazardPointerArray<T> {}
// no Send impl for HazardPointerGuard since it is supposed for static usage

pub struct HazardPointerGuard<'a, T> {
    array: &'a HazardPointerArray<T>,
    starting_idx: usize,
    available_indices: std::cell::Cell<u64>,
    d_list: std::cell::RefCell<Vec<*mut T>>,
}

impl<T> HazardPointerGuard<'_, T> {
    pub fn protect(&self, data: *mut T) -> Result<ProtectedPointer<T>, ProtectionError> {
        let current = self.available_indices.get();
        if current == 0 {
            return Err(ProtectionError::NoAvailableIndices);
        }

        let offset = current.trailing_zeros() as usize;
        self.available_indices.set(current & !(1u64 << offset));
        self.array.p_list[self.starting_idx + offset].store(data, Ordering::Release);

        Ok(ProtectedPointer {
            ptr: data,
            index: offset,
            guard: self,
        })
    }

    pub fn retire_node(&self, ptr: *mut T, index: usize) {
        self.array.p_list[self.starting_idx + index]
            .store(core::ptr::null_mut(), Ordering::Release);
        let indices = self.available_indices.get();
        self.available_indices.set(indices | (1u64 << index));
        let mut d_list = self.d_list.borrow_mut();
        d_list.push(ptr);
        if d_list.len() > SCAN_THRESHOLD {
            drop(d_list);
            self.scan();
        }
    }

    pub fn retire_raw_pointer(&mut self, ptr: *mut T) {
        let mut d_list = self.d_list.borrow_mut();
        d_list.push(ptr);
        if d_list.len() > SCAN_THRESHOLD {
            drop(d_list);
            self.scan();
        }
    }

    // here, we perform 'thread-local' scan
    fn scan(&self) {
        let mut p_list_snapshot = self
            .array
            .p_list
            .iter()
            .filter_map(|e| {
                let ptr = e.load(Ordering::Acquire);
                if !ptr.is_null() {
                    return Some(ptr);
                }
                None
            })
            .collect::<Vec<_>>();
        p_list_snapshot.dedup();
        p_list_snapshot.sort();
        // if not found in p_list then deallocate
        // else push to new_d_list
        let mut d_list = self.d_list.borrow_mut();
        let old_list = std::mem::take(&mut *d_list);

        *d_list = old_list
            .into_iter()
            .filter_map(|item| {
                if p_list_snapshot.binary_search(&item).is_err() {
                    unsafe {
                        let _ = Box::from_raw(item);
                    }
                    None
                } else {
                    Some(item)
                }
            })
            .collect();
    }
}

impl<'a, T> Drop for HazardPointerGuard<'a, T> {
    fn drop(&mut self) {
        self.scan();
        self.array
            .thread_registry
            .fetch_or(1 << (self.starting_idx / HP_PER_THREAD), Ordering::Release);
    }
}

pub struct ProtectedPointer<'a, T> {
    ptr: *mut T,
    index: usize,
    guard: &'a HazardPointerGuard<'a, T>,
}

impl<'a, T> ProtectedPointer<'a, T> {
    // unsafe fr!
    // consumes protected pointer, unprotects it and returns underlying raw pointer
    // caller must ensure the memory remains valid as long as needed
    // pointer must not be freed directly, only through retire_raw_pointer
    pub unsafe fn into_raw(self) -> *mut T {
        let ptr = self.ptr;
        self.guard.array.p_list[self.guard.starting_idx + self.index]
            .store(std::ptr::null_mut(), Ordering::Release);
        let indices = self.guard.available_indices.get();
        self.guard
            .available_indices
            .set(indices | (1u64 << self.index));
        std::mem::forget(self);
        ptr
    }
}

impl<'a, T> std::ops::Deref for ProtectedPointer<'a, T> {
    type Target = *mut T;
    fn deref(&self) -> &Self::Target {
        &self.ptr
    }
}

impl<'a, T> Drop for ProtectedPointer<'a, T> {
    fn drop(&mut self) {
        self.guard.retire_node(self.ptr, self.index);
    }
}

pub enum ProtectionError {
    NoAvailableIndices,
}

pub enum RegisterThreadError {
    NoAvailableIndices,
}
