use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::ops::Deref;


pub struct Rcu<T: Sync> {
    ptr: AtomicPtr<T>,
    readers: AtomicUsize,
}

pub struct RcuReadGuard<'a, T> {
    rcu: &'a Rcu<T>,
    ptr: *const T,
}

impl<'a, T> Deref for RcuReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // safe because reader count > 0 and pointer is valid when the guard was created.
        unsafe { &*self.ptr }
    }
}

impl<'a, T> Drop for RcuReadGuard<'a, T> {
    fn drop(&mut self) {
        self.rcu.readers.fetch_sub(1, Ordering::Release);
    }
}

impl<T> Rcu<T> {
    pub fn new(data: T) -> Self {
        let boxed = Box::new(data);
        Rcu {
            ptr: AtomicPtr::new(Box::into_raw(boxed)),
            readers: AtomicUsize::new(0),
        }
    }

    // RCU read-side critical section
    // returns a guard that dereferences to &T that releases on drop
    pub fn read(&self) -> RcuReadGuard<T> {
        self.readers.fetch_add(1, Ordering::Release);
        let ptr = self.ptr.load(Ordering::Acquire);
        RcuReadGuard { rcu: self, ptr }
    }

    // update the data by creating a new version from the old one
    // need to be reconsidered
    pub fn update<F>(&self, f: F)
    where
        F: FnOnce(&T) -> T,
    {
        let old_ptr = self.ptr.load(Ordering::Acquire);
        let old_ref: &T = unsafe { &*old_ptr };
        let new_data = f(old_ref);
        let new_ptr = Box::into_raw(Box::new(new_data));
        // hmhm
        self.ptr.store(new_ptr, Ordering::Release);

        //grace-period things..
        // need to revise once again
        while self.readers.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }

        // so, here, it should be safe to free the old data (if implemetaion is correct ofc :)
        unsafe {
            drop(Box::from_raw(old_ptr));
        }
    }
}

impl<T> Drop for Rcu<T> {
    fn drop(&mut self) {
        // free the underlying data before saying goodbye
        let ptr = self.ptr.load(Ordering::Acquire);
        if !ptr.is_null() {
            unsafe { drop(Box::from_raw(ptr)); }
        }
    }
}
