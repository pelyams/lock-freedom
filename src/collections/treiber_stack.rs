use crate::mechanisms::hp::{HazardPointerGuard, ProtectionError};
use crate::utils::backoff::Backoff;
use std::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};

use rand::prelude::*;

const ELIMINATION_ARRAY_SIZE: usize = 8;
const ELIMINATION_THRESHOLD: u8 = 4;

// elimination array may have following states:
const EMPTY: usize = 0;
const POP: usize = 1;
// + non-const state: pointer to node placed by push attempt - case slot was EMPTY
// + non-const state: point to node placed by push attempt with LSB 1 - case slot was POP

pub struct TreiberStack<T> {
    head: AtomicPtr<Node<T>>,
    elimination_array: [AtomicUsize; ELIMINATION_ARRAY_SIZE],
}

// todo: need a public wrapper type (for hazard pointer guard typing)
struct Node<T> {
    data: T,
    next: AtomicPtr<Node<T>>,
}

impl<T> TreiberStack<T>
where
    T: Default,
{
    pub fn new() -> Self {
        Self {
            head: AtomicPtr::new(std::ptr::null_mut()),
            elimination_array: [const { AtomicUsize::new(0) }; ELIMINATION_ARRAY_SIZE],
        }
    }

    // no safe reclamation needed for push method, since we don't dereference pointers here
    pub fn push(&self, data: T) {
        assert_ne!(
            align_of::<T>() & 1,
            1,
            "TreiberStack data alignment must be greater than one"
        );

        let new_node = Box::into_raw(Box::new(Node {
            data,
            next: AtomicPtr::new(std::ptr::null_mut()),
        }));

        let mut backoff = Backoff::new();
        let mut loop_counter = 0;

        loop {
            let head = self.head.load(Ordering::Relaxed);
            unsafe { (*new_node).next.store(head, Ordering::Relaxed) };

            // try to swap in our node as the new head
            if self
                .head
                .compare_exchange_weak(head, new_node, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            if loop_counter < ELIMINATION_THRESHOLD {
                backoff.spin();
                loop_counter += 1;
            } else {
                match self.try_elimination_push(new_node) {
                    Ok(_) => return,
                    // actual error doesn't matter here, we just start again
                    Err(_) => {
                        loop_counter = 0;
                        backoff.reset();
                    }
                }
            }
        }
    }

    // user should register thread for hp guard obtaining
    pub fn pop(&self, guard: &HazardPointerGuard<Node<T>>) -> Option<T> {
        let mut hp_backoff = Backoff::new();
        let mut cas_backoff = Backoff::new();
        let mut loop_couter = 0;

        loop {
            let head_ptr = self.head.load(Ordering::Relaxed);

            // yes, hazard pointer time:
            let mut protected_head = match unsafe { guard.protect(head_ptr) } {
                Ok(ptr) => ptr,
                Err(ProtectionError::NoAvailableIndices) => {
                    hp_backoff.spin();
                    continue; // no hazard pointer slots available, retry
                }
                Err(ProtectionError::NullPointer) => return None,
            };
            hp_backoff.reset();

            // recheck head hasn't changed
            if self.head.load(Ordering::Relaxed) != protected_head.as_mut_ptr() {
                continue;
            }

            // safely read the next pointer of the head node
            let next = (*protected_head).next.load(Ordering::Relaxed);

            // try to update the head to the next node
            if self
                .head
                .compare_exchange_weak(
                    protected_head.as_mut_ptr(),
                    next,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // successfully popped the node
                // now return data and retirement
                let data = std::mem::take(&mut (*protected_head).data);
                guard.retire_node(protected_head);
                return Some(data);
            }

            if loop_couter < ELIMINATION_THRESHOLD {
                loop_couter += 1;
                cas_backoff.spin();
            } else {
                match self.try_elimination_pop() {
                    Ok(data) => return Some(data),
                    Err(_) => {
                        loop_couter = 0;
                        cas_backoff.reset();
                    }
                }
            }
        }
    }

    fn try_elimination_push(&self, node: *mut Node<T>) -> Result<(), EliminationError> {
        let mut rng = rand::rng();

        for _ in 0..ELIMINATION_ARRAY_SIZE {
            let slot_id: usize = rng.random_range(0..ELIMINATION_ARRAY_SIZE);
            match self.elimination_array[slot_id].load(Ordering::Relaxed) {
                EMPTY => {
                    fence(Ordering::Acquire);
                    if self.elimination_array[slot_id]
                        .compare_exchange(
                            EMPTY,
                            node as usize,
                            Ordering::Release,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        std::thread::yield_now();
                        let attempts = ELIMINATION_ARRAY_SIZE * 4;
                        for _ in 0..attempts {
                            let slot_value =
                                self.elimination_array[slot_id].load(Ordering::Relaxed);

                            // nano chances are we can face ABA here
                            if slot_value == node as usize {
                                std::hint::spin_loop();
                                continue;
                            }
                            fence(Ordering::Acquire);
                            return Ok(());
                        }

                        return match self.elimination_array[slot_id].compare_exchange(
                            node as usize,
                            EMPTY,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => Err(EliminationError::NoRendezvous),
                            Err(_) => Ok(()),
                        };
                    }
                }
                POP => {
                    fence(Ordering::Acquire);
                    if self.elimination_array[slot_id]
                        .compare_exchange(
                            POP,
                            node as usize | 1,
                            Ordering::Release,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        // here, we are optimistically awaiting pop would proceed and return immediately
                        return Ok(());
                    }
                }
                // ptr and (ptr | 1) are equally disappointing, continue
                _ => (),
            };
        }

        Err(EliminationError::NoSlotsAvailable)
    }

    fn try_elimination_pop(&self) -> Result<T, EliminationError> {
        let mut rng = rand::rng();

        for _ in 0..ELIMINATION_ARRAY_SIZE {
            let slot_id: usize = rng.random_range(0..ELIMINATION_ARRAY_SIZE);
            match self.elimination_array[slot_id].load(Ordering::Relaxed) {
                EMPTY => {
                    fence(Ordering::Acquire);
                    if self.elimination_array[slot_id]
                        .compare_exchange(EMPTY, POP, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        std::thread::yield_now();
                        let attempts = ELIMINATION_ARRAY_SIZE * 4;
                        // how we are waiting if some push updated the slot
                        for _ in 0..attempts {
                            let slot_value =
                                self.elimination_array[slot_id].load(Ordering::Relaxed);
                            if slot_value == POP {
                                std::hint::spin_loop();
                                continue;
                            }
                            fence(Ordering::Acquire);
                            let node_ptr = (slot_value & !1) as *mut Node<T>;
                            self.elimination_array[slot_id].store(EMPTY, Ordering::Release);
                            return Ok(unsafe { Box::from_raw(node_ptr) }.data);
                        }

                        //okay, give up, if nothing changed
                        match self.elimination_array[slot_id].compare_exchange(
                            POP,
                            EMPTY,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => return Err(EliminationError::NoRendezvous),
                            Err(slot_value) => {
                                fence(Ordering::Acquire);
                                let node_ptr = (slot_value & !1) as *mut Node<T>;
                                self.elimination_array[slot_id].store(EMPTY, Ordering::Release);
                                return Ok(unsafe { Box::from_raw(node_ptr) }.data);
                            }
                        }
                    }
                }
                // covers POP case as well
                // can be enhanced for the case when slot contains a tagged pointer
                // like try to cas(&slot, tagged_ptr, POP, .., ..)
                ptr if (ptr & 1 == 1) => (),
                ptr => {
                    fence(Ordering::Acquire);
                    if self.elimination_array[slot_id]
                        .compare_exchange(ptr, EMPTY, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        let node_ptr = (ptr & !1) as *mut Node<T>;
                        return Ok(unsafe { Box::from_raw(node_ptr) }.data);
                    }
                }
            };
        }

        Err(EliminationError::NoSlotsAvailable)
    }
}

unsafe impl<T> Sync for TreiberStack<T> {}

enum EliminationError {
    NoSlotsAvailable,
    NoRendezvous,
}

#[cfg(test)]
mod tests {
    use crate::collections::treiber_stack::TreiberStack;
    use crate::mechanisms::hp::HazardPointerArray;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::LazyLock;

    static HP_ARRAY: LazyLock<HazardPointerArray> = LazyLock::new(|| HazardPointerArray::new());

    #[test]
    fn test_basic_operations() {
        let stack = TreiberStack::new();
        stack.push(-1);
        stack.push(2);
        stack.push(33);
        let mut pop_results = Vec::new();
        let guard = HP_ARRAY.register_thread().ok().unwrap();
        pop_results.push(stack.pop(&guard).unwrap());
        pop_results.push(stack.pop(&guard).unwrap());
        pop_results.push(stack.pop(&guard).unwrap());
        assert_eq!(pop_results, vec![33, 2, -1]);
    }

    #[derive(Default)]
    struct TrackableValue {
        value: usize,
    }

    static NEXT_VALUE: AtomicUsize = AtomicUsize::new(0);

    impl TrackableValue {
        fn new() -> Self {
            TrackableValue {
                value: NEXT_VALUE.fetch_add(1, Ordering::Relaxed),
            }
        }
    }

    #[test]
    fn test_concurrent() {
        let stack = TreiberStack::new();
        let stack_ref = &stack;

        let thread_count = 4;
        let per_thread_ops = 16;
        let expected_values: HashSet<usize> = (0..thread_count * per_thread_ops).collect();

        for _ in 0..55_000 {
            let collected_values = std::sync::Mutex::new(std::vec![]);
            let values_ref = &collected_values;

            std::thread::scope(|s| {
                (0..thread_count).for_each(|_| {
                    s.spawn(|| {
                        let guard = HP_ARRAY.register_thread().ok().unwrap();

                        // first batch of pushes and pops
                        for  _ in 0..per_thread_ops/2 {
                            stack_ref.push(TrackableValue::new());
                        }
                        
                        for  _ in 0..per_thread_ops/2 {
                            let popped = stack_ref.pop(&guard);
                            let mut lock = values_ref.lock().unwrap();
                            (*lock).push(popped.unwrap().value);
                        }
                        
                        // second batch
                        for  _ in per_thread_ops / 2..per_thread_ops {
                            stack_ref.push(TrackableValue::new());
                        }
                        
                        for  _ in per_thread_ops / 2..per_thread_ops {
                            let popped = stack_ref.pop(&guard);
                            let mut lock = values_ref.lock().unwrap();
                            (*lock).push(popped.unwrap().value);
                        }
                    });
                });
            });

            let actual_values = HashSet::<usize>::from_iter(collected_values.into_inner().unwrap());
            assert_eq!(actual_values, expected_values);
            NEXT_VALUE.store(0, Ordering::Relaxed)
        }
    }
}
