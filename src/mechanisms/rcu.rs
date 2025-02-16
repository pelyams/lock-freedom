use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicBool, Ordering};

struct RCU<T> {
    is_written: AtomicBool,
    data: UnsafeCell<T>,
    data_ptr: AtomicPtr<T>
}

impl<T> RCU<T> {
    pub fn new() -> Self {}
    pub fn read(&self) {

        /// need a counter for old data reads
        ///
        /// need to design a mechanism for releasing after grace period


    }
    pub fn update(&self) {
        ///should create copy of data_ptr deref
        ///
        /// is_written property is not necessary since data_ptr is atomic
        ///
        ///
        /// QUESTION: should we copy-update-insert or copy-update-compare-insert
        /// hence, should we have a lock or wri
    }
}

