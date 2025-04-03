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
        let pointers: [AtomicPtr<()>; MAX_THREADS * HP_PER_THREAD] = [NULL_PTR; MAX_THREADS * HP_PER_THREAD];

        assert!(MAX_THREADS <= 64, "MAX_THREADS must be less or equal to 64");
        let thread_registry = !0 >> (64 - MAX_THREADS);

        Self {
            p_list: unsafe { std::mem::transmute(pointers) },
            thread_registry: AtomicU64::new(thread_registry),
        }
    }

    // todo: change to Result
    pub const fn register_thread(&self) -> Option<HazardPointerGuard<T>> {
        // вообще долбиться в цикле тоже не хочется, конечно
        // мб следует как-то помечать забронированные индексы?
        loop {
            let thread_registry = self.thread_registry.load(Ordering::Relaxed);
            if thread_registry == 0 {
                return None;
            } else {
                let tr_first_slot = thread_registry.trailing_zeros() as usize;
                if self.thread_registry.compare_exchange_weak(
                    thread_registry,
                    thread_registry ^ (1 << tr_first_slot),
                    Ordering::AcqRel,
                    Ordering::Relaxed)
                    .is_ok() {
                    return Some(
                        HazardPointerGuard {
                            array: &self,
                            starting_idx: tr_first_slot * HP_PER_THREAD,
                            available_indices: !0 >> (64 - HP_PER_THREAD),
                            d_list: Vec::<T>::new(),
                        });

                }
            }
        }
    }
}

pub struct HazardPointerGuard<'a, T> {
    array: &'a HazardPointerArray<T>,
    starting_idx: usize,
    available_indices: u64,
    d_list: Vec<T>,
}

impl<T> HazardPointerGuard<T> {
    pub fn protect(&mut self, data: * mut T) -> Result<ProtectedPointer<T>, ProtectionError> {
        if self.available_indices == 0 {
            Err(ProtectionError::NoAvailableIndices)
        }
        let offset = self.available_indices.trailing_zeros() as usize;
        self.available_indices &= !(1u64 << offset);
        self.array.p_list[self.starting_idx + offset].store(data, Ordering::Release);
        Ok(ProtectedPointer {
            ptr: data,
            index: offset,
            guard: self,
        })
    }

    pub fn retire_node(
        &mut self,
        node: &ProtectedPointer<T>,
    ) {
        //release
        // populate d_list with a new node
        let retired_node = self.release(node);
        self.d_list.push(retired_node);
        if self.d_list.len() > SCAN_THRESHOLD {
            self.scan();
        }
    }

    // turn this method public and get things hazard fr
    fn release(
        &mut self,
        node: &ProtectedPointer<T>,
    ) -> * mut T {
        // extract & remove entry from p_lsit
        self.array.p_list[self.starting_idx + node.index].store(core::ptr::null_mut(), Ordering::Release);
        let released_ptr = node.ptr;

        // hmm
        core::mem::forget(node);

        // add return idx to self.indices
        self.available_indices |= node.index as u64;

        //return extracted pointer
        released_ptr
    }

    // here, we perform 'thread-local' scan
    fn scan(&mut self) {

        // sort + filter out duplicates from p_list

        // if not found in p_list then deallocate
        // else push to new_d_list
        self.d_list.iter().for_each(|item| {

        })
    }

}

impl<T> Drop for HazardPointerGuard<T> {
    fn drop(&mut self) {
        //do clean_up in p_list


        self.scan();
        //fetch_sub hazard_pointer_array.thread_count
        self.array.thread_registry.fetch_or(1 << (self.starting_idx / HP_PER_THREAD), Ordering::Relaxed);

    }
}


struct ProtectedPointer<'a, T> {
    ptr: *mut T,
    index: usize,
    guard: &'a mut HazardPointerGuard<'_, T>
}

impl<'a, T> core::ops::Deref for ProtectedPointer<'a, T> {
    type Target = *mut T;
    fn deref(&self) -> &Self::Target {
        &self.ptr
    }
}

impl<'a, T> Drop for ProtectedPointer<'a, T> {
    fn drop(&mut self) {
        self.guard.retire_node(self);
    }
}

enum ProtectionError {
    NoAvailableIndices,
}