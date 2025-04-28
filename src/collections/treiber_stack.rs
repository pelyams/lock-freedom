use crate::mechanisms::hp::*;
use std::sync::atomic::{AtomicPtr, Ordering};


// todo: alter to lazy static
// static init hazardpointers
static HP_ARRAY: HazardPointerArray<Node<u32>> = HazardPointerArray::new();

struct TreiberStack<T> {
    head: AtomicPtr<Node<T>>,
}

struct Node<T> {
    data: T,
    next: AtomicPtr<Node<T>>,
}

impl<T> TreiberStack<T>
where T: Default {
    pub fn new() -> Self {
        Self { head: AtomicPtr::new(std::ptr::null_mut()) }
    }

    // no safe reclamation needed for push method, since we don't dereference pointers here
    pub fn push(&mut self, data: T) {
        let mut backoff = Backoff::new();
        let new_node = Box::into_raw(Box::new(Node {
            data,
            next: AtomicPtr::new(std::ptr::null_mut()),
        }));

        loop {
            let head = self.head.load(Ordering::Relaxed);
            unsafe { (*new_node).next.store(head, Ordering::Relaxed) };

            // try to swap in our node as the new head
            if self.head.compare_exchange_weak(
                head,
                new_node,
                Ordering::Release,
                Ordering::Relaxed
            ).is_ok() {
                return;
            }

            // if backoff elimination succeed, we can return
            // backoff.elimination_strategy_for_push()

            // ok, retry after spinning
            backoff.spin();
        }
    }

    // user should register thread for hp guard obtaining
    pub fn pop(&self, guard: &HazardPointerGuard<Node<T>>) -> Option<T> {
        let mut backoff = Backoff::new();

        loop {
            let head_ptr = self.head.load(Ordering::Relaxed);
            if head_ptr.is_null() {
                return None;
            }

            // yes, hazard pointer time:
            let mut protected_head = match guard.protect(head_ptr) {
                Ok(ptr) => ptr,
                Err(_) => {
                    backoff.spin();
                    continue; // no hazard pointer slots available, retry
                }
            };

            // recheck that head hasn't changed
            // (maybe redundant: following CAS will also fail if head moved)
            if self.head.load(Ordering::Relaxed) != *protected_head {
                backoff.spin();
                continue;
            }

            // safely read the next pointer of the head node
            let next = unsafe { (**protected_head).next.load(Ordering::Relaxed) };

            // try to update the head to the next node
            if self.head.compare_exchange_weak(
                *protected_head,
                next,
                Ordering::Acquire,
                Ordering::Relaxed
            ).is_ok() {
                // successfully popped the node
                // now return data and retirement
                let data = unsafe {
                    std::mem::take(&mut (**protected_head).data)
                };
                guard.retire_node(protected_head);
                return Some(data);
            }

            // if backoff elimination succeed, we can return
            // backoff.elimination_strategy_for_pop()

            // ok, retry after spinning
            // we have the same spinning backoff state for all the cases🥴
            backoff.spin();
        }
    }
}


struct Backoff {
    initial: u32,
    threshold: u32,
    current: u32,
}

impl Backoff {
    fn new() -> Self {
        Self {
            initial: 1,
            threshold: 8191,
            current: 1,
        }
    }

    fn spin(&mut self) {
        for _ in 0..self.current {
            std::hint::spin_loop();
        }
       self.current = (self.current << 1) | self.threshold;
    }

    fn reset(&mut self) {
        self.current = self.initial;
    }

    fn try_elimination_push(&self) -> bool {
        // TODO: implement
        false
    }

    fn try_elimination_pop(&self) -> bool {
        // TODO: implement
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::collections::treiber_stack::{TreiberStack, HP_ARRAY};

    #[test]
    fn it_works() {
        let mut stack = TreiberStack::new();
        stack.push(1);
        stack.push(2);
        stack.push(33);
        let mut pop_results = Vec::new();
        let guard = HP_ARRAY.register_thread().ok().unwrap();
        pop_results.push(stack.pop(&guard).unwrap());
        pop_results.push(stack.pop(&guard).unwrap());
        pop_results.push(stack.pop(&guard).unwrap());
        assert_eq!(pop_results, vec![33,2,1]);
    }
}