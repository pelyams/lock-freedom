use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use crate::utils::backoff::Backoff;

static CONTROL_BIT: usize = 1;
static RCU_ID: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    // nested reads counters per rcu
    static THREAD_RECORD: RefCell<HashMap<usize, [usize;2]>> = RefCell::new(HashMap::new());
}

pub struct Rcu<T: Sync> {
    /*
        since this is a single-writer RCU, there could be only 2 possible versions of
        underlying data at a time. so, instead of having two separate fields, we can have
        a combined value, that can both ensure consistency and simplify design:
        current epoch value can be stored to the least significant bit of pointer,
        that is commonly out of use, unless it has alignment of 1. as an obvious drawback,
        we can't use T with alignment of 1. as a workaround to this drawback we can
        introduce some Padded wrapper type for T (not implemented here)
    */
    ptr_and_epoch: AtomicPtr<T>,
    /*
        store previous pointer for safer memory reclamation in the next synchronize().
        this should solve an issue:
        if we try to free pointer from ptr_and_epoch right after updating its value,
        we can fall into following case. consider the case: reader and writer are accessing rcu
        simultaneously. currently there's no active 'readers' for current epoch. first, reader
        thread obtains the pointer, then scheduler preempts to writer. writer updates the value+
        epoch and runs synchronize(). since reader hasn't updated 'readers' yet, writer is free
        to free the previous pointer. now reader updates 'readers' for previous epoch and obtains
        a guard with a dangling pointer. to rule out this risk, we delay previous pointer
        reclamation to the next synchronize() invocation
    */
    previous_ptr: RefCell<*mut T>,
    rcu_id: usize,
    // reading threads counters for both rcu epochs
    readers: [AtomicUsize; 2],
}

impl<T: Sync> Rcu<T> {
    pub fn new(data: T) -> Self {
        assert!(std::mem::align_of::<T>() & 1 == 0);
        let id = RCU_ID.fetch_add(1, Ordering::Relaxed);
        let data_ptr = Box::into_raw(Box::new(data));
        Rcu {
            ptr_and_epoch: AtomicPtr::new(data_ptr),
            previous_ptr: RefCell::new(ptr::null_mut()),
            rcu_id: id,
            readers: [const { AtomicUsize::new(0) }; 2],
        }
    }

    pub fn read(&self) -> RcuReadGuard<T> {
        let ptr_and_epoch = self.ptr_and_epoch.load(Ordering::Relaxed);
        let epoch = ptr_and_epoch as usize & CONTROL_BIT;
        THREAD_RECORD.with(|tr| {
            let mut rcu_nested_map = tr.borrow_mut();
            if rcu_nested_map.contains_key(&self.rcu_id) {
                if rcu_nested_map[&self.rcu_id][epoch] == 0 {
                    self.readers[epoch].fetch_add(1, Ordering::Release);
                }
            } else {
                self.readers[epoch].fetch_add(1, Ordering::Release);
                rcu_nested_map.insert(self.rcu_id, [0, 0]);
            }
            let nested = rcu_nested_map.get_mut(&self.rcu_id).unwrap();
            nested[epoch] += 1;
        });
        RcuReadGuard {
            rcu: self,
            ptr: (ptr_and_epoch as usize & !CONTROL_BIT) as *const T,
            epoch,
        }
    }
    pub fn update(&self, data: T) {
        let new_data_ptr = Box::into_raw(Box::new(data));
        let mut backoff = Backoff::new();
        
        loop {
            let current_ptr_and_epoch = self.ptr_and_epoch.load(Ordering::Acquire);
            let next_epoch = (current_ptr_and_epoch as usize & CONTROL_BIT) ^ CONTROL_BIT;
            let current_ptr = (current_ptr_and_epoch as usize & !CONTROL_BIT) as *mut T;
            
            self.synchronize(next_epoch, current_ptr);
            
            let new_ptr_and_epoch = (new_data_ptr as usize | next_epoch) as *mut T;
            
            match self.ptr_and_epoch.compare_exchange(
                current_ptr_and_epoch,
                new_ptr_and_epoch,
                Ordering::Release,
                Ordering::Relaxed
            ) {
                Ok(_) => {
                    break;
                },
                Err(_) => {
                    backoff.spin_yield();
                    continue;
                }
            } 
        }
    }
    
    pub fn try_update(&self, data: T) -> bool {
        let current_ptr_and_epoch = self.ptr_and_epoch.load(Ordering::Acquire);
        let next_epoch = (current_ptr_and_epoch as usize & CONTROL_BIT) ^ CONTROL_BIT;
        let current_ptr = (current_ptr_and_epoch as usize & !CONTROL_BIT) as *mut T;
        if !self.try_synchronize(next_epoch, current_ptr) {
            return false;
        }
        
        let new_data_ptr = Box::into_raw(Box::new(data));
        let packed_ptr_and_epoch = (new_data_ptr as usize | next_epoch) as *mut T;
        match self.ptr_and_epoch.compare_exchange(
            current_ptr_and_epoch,
            packed_ptr_and_epoch, 
            Ordering::Release,
            Ordering::Relaxed
        ) {
            Ok(_) => { true },
            Err(_) => {
                unsafe { drop(Box::from_raw(new_data_ptr)) };
                false
            }
        }
        
    }

    fn synchronize(&self, sync_epoch: usize, ptr: *mut T) {
        let mut backoff = Backoff::new();

        // wait for readers of sync_epoch to finish
        while self.readers[sync_epoch].load(Ordering::Acquire) != 0 {
            backoff.spin_yield();
        }

        let previous_ptr = self.previous_ptr.replace(ptr);

        // someone could have already succeeded with synchronization
        // free if we have a previous pointer and it's different from current
        if !previous_ptr.is_null() && previous_ptr != ptr {
            unsafe { drop(Box::from_raw(previous_ptr)) };
        }
    }

    fn try_synchronize(&self, sync_epoch: usize, ptr: *mut T) -> bool  {
        if self.readers[sync_epoch].load(Ordering::Acquire) != 0 {
            return false;
        }
        let previous_ptr = self.previous_ptr.replace(ptr);
        if !previous_ptr.is_null() && previous_ptr != ptr {
            unsafe { drop(Box::from_raw(previous_ptr)) };
        }
        true
    }
}

unsafe impl<T: Sync> Sync for Rcu<T> {}

pub struct RcuReadGuard<'a, T: Sync> {
    rcu: &'a Rcu<T>,
    /*
       raw pointer to current underlying data version
       if we instead read ptr from rcu reference, we may accidentally access an updated
       pointer data and have different data versions:
       1. reader A accesses data once: RCU -> RcuReadGuard -> rcu -> atomic ptr -> current
       data version
       2. writer B: RCU -> update(new_data) -> atomic ptr updates
       3. reader A accesses second time: RcuReadGuard -> rcu -> atomic ptr -> newer data
    */
    ptr: *const T,
    // here, let be standalone, for clarity
    epoch: usize,
}

impl<'a, T: Sync> Deref for RcuReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // safe because reader count > 0 and pointer is valid when the guard was created
        unsafe { &*self.ptr }
    }
}

impl<'a, T: Sync> Drop for RcuReadGuard<'a, T> {
    fn drop(&mut self) {
        THREAD_RECORD.with(|tr| {
            let mut rcu_nested_map = tr.borrow_mut();
            let nested = rcu_nested_map.get_mut(&self.rcu.rcu_id).unwrap();
            nested[self.epoch] -= 1;
            if nested[self.epoch] == 0 {
                self.rcu.readers[self.epoch].fetch_sub(1, Ordering::Release);
                if nested[self.epoch ^ 1] == 0 {
                    rcu_nested_map.remove(&self.rcu.rcu_id);
                }
            }
        });
    }
}

impl<T: Sync> Drop for Rcu<T> {
    fn drop(&mut self) {
        let ptr = (self.ptr_and_epoch.load(Ordering::Acquire) as usize & !CONTROL_BIT) as *mut T;
        // this is safe, because RcuReadGuards, providing a reference to underlying data
        // wouldn't outlive rcu
        if !ptr.is_null() {
            unsafe { drop(Box::from_raw(ptr)); }
            let prev_ptr = self.previous_ptr.replace(ptr::null_mut());
            if !prev_ptr.is_null() {
                unsafe {drop(Box::from_raw(prev_ptr)); }
            }
        }
    }
}
