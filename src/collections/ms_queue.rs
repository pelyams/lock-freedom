use crate::mechanisms::hp::{HazardPointerGuard, ProtectionError};
use std::default::Default;
use std::ptr;
use std::sync::atomic::{fence, AtomicPtr, Ordering};
use crate::utils::backoff::Backoff;

pub struct MSQueue<T> {
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
}

pub struct Node<T> {
    data: T,
    next: AtomicPtr<Node<T>>,
}

impl<T> MSQueue<T>
where
    T: Default,
{
    pub fn new() -> MSQueue<T> {
        //head should point to a dummy node
        let dummy_node = Box::into_raw(Box::new(Node {
            data: T::default(),
            next: AtomicPtr::new(ptr::null_mut()),
        }));
        MSQueue {
            head: AtomicPtr::new(dummy_node),
            tail: AtomicPtr::new(dummy_node),
        }
    }

    // user should register thread to obtain guard
    pub fn enqueue(&self, value: T, guard: &HazardPointerGuard<Node<T>>) -> bool {
        let mut backoff = Backoff::new();

        let new_node = Box::into_raw(Box::new(Node {
            data: value,
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        let mut tail_ptr = std::mem::MaybeUninit::<*mut Node<T>>::uninit();

        loop {
            tail_ptr.write(self.tail.load(Ordering::Relaxed));
            let mut protected_tail = match unsafe { guard.protect(tail_ptr.assume_init()) } {
                Ok(ptr) => {
                    fence(Ordering::Acquire);
                    ptr
                }
                Err(_) => {
                    backoff.spin();
                    continue; // no hazard pointer slots available, retry
                }
            };

            // first, check if tail is located correctly
            let tail_next = (*protected_tail).next.load(Ordering::Acquire);
            if tail_next != ptr::null_mut() {
                _ = self.tail.compare_exchange_weak(
                    protected_tail.as_mut_ptr(),
                    tail_next,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
                // regardless succeed we or not need to protect new tail node pointer
                continue;
            }

            if (*protected_tail)
                .next
                .compare_exchange_weak(
                    ptr::null_mut(),
                    new_node,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
        }
        // attempt to proceed tail; on fail, tail will be proceeded by others
        _ = self.tail.compare_exchange_weak(
            unsafe { tail_ptr.assume_init_read() },
            new_node,
            Ordering::Release,
            Ordering::Relaxed,
        );
        true
    }

    // user should register thread to obtain guard
    pub fn dequeue(&self, guard: &HazardPointerGuard<Node<T>>) -> Option<T> {
        let mut backoff = Backoff::new();

        let mut head_ptr = std::mem::MaybeUninit::<*mut Node<T>>::uninit();
        let mut head_next = std::mem::MaybeUninit::<*mut Node<T>>::uninit();

        loop {
            head_ptr.write(self.head.load(Ordering::Relaxed));
            let mut protected_head = match unsafe { guard.protect(head_ptr.assume_init_read()) } {
                Ok(ptr) => {
                    fence(Ordering::Acquire);
                    ptr
                }
                Err(ProtectionError::NoAvailableIndices) => {
                    backoff.spin();
                    continue; // no hazard pointer slots available, retry
                }
                // should rather return ! aka never type
                Err(ProtectionError::NullPointer) => return None,
            };

            head_next.write(unsafe { (*protected_head).next.load(Ordering::Relaxed) });
            let mut protected_head_next =
                match unsafe { guard.protect(head_next.assume_init_read()) } {
                    Ok(ptr) => {
                        fence(Ordering::Acquire);
                        ptr
                    }
                    Err(_) => {
                        backoff.spin();
                        continue; // no hazard pointer slots available, retry
                    }
                    Err(ProtectionError::NullPointer) => return None,
                };

            if self.head.load(Ordering::Relaxed) != protected_head.as_mut_ptr() {
                continue;
            }

            if self
                .head
                .compare_exchange_weak(
                    protected_head.as_mut_ptr(),
                    protected_head_next.as_mut_ptr(),
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // here, we proceed tail only on successful cas
                // (stolen from "Formal Verification of a Practical Lock-Free Queue Algorithm" by
                // S.Doherty et al., 2004)
                loop {
                    let tail = self.tail.load(Ordering::Relaxed);
                    if tail == protected_head.as_mut_ptr() {
                        fence(Ordering::Acquire);
                        if self
                            .tail
                            .compare_exchange_weak(
                                protected_head.as_mut_ptr(),
                                protected_head_next.as_mut_ptr(),
                                Ordering::Release,
                                Ordering::Relaxed,
                            )
                            .is_err()
                        {
                            continue;
                        }
                    }
                    break;
                }
                guard.retire_node(protected_head);
                return Some(std::mem::take(&mut (*protected_head_next).data));
            }
        }
    }
}

unsafe impl<T: Default> Sync for MSQueue<T> {}

#[cfg(test)]
mod tests {
    use super::MSQueue;
    use crate::mechanisms::hp::HazardPointerArray;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::LazyLock;

    static HP_ARRAY: LazyLock<HazardPointerArray> = LazyLock::new(|| HazardPointerArray::new());

    #[test]
    fn test_basic_operations() {
        let q = MSQueue::new();
        let guard = HP_ARRAY.register_thread().ok().unwrap();

        q.enqueue(1, &guard);
        q.enqueue(2, &guard);
        q.enqueue(3, &guard);
        q.enqueue(4, &guard);

        let results = vec![
            q.dequeue(&guard).unwrap(),
            q.dequeue(&guard).unwrap(),
            q.dequeue(&guard).unwrap(),
            q.dequeue(&guard).unwrap(),
        ];

        assert_eq!(results, vec![1, 2, 3, 4]);
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
        let q = MSQueue::new();
        let q_ref = &q;

        let thread_count = 8;
        let per_thread_ops = 64;
        let expected_values: HashSet<usize> = (0..thread_count * per_thread_ops).collect();

        for _ in 0..15000 {
            let collected_values = std::sync::Mutex::new(Vec::new());
            let values_ref = &collected_values;

            std::thread::scope(|scope| {
                for _ in 0..thread_count {
                    scope.spawn(|| {
                        let guard = HP_ARRAY.register_thread().ok().unwrap();

                        // first batch: enqueue half and eventual dequeue half
                        for _ in 0..per_thread_ops / 2 {
                            q_ref.enqueue(TrackableValue::new(), &guard);
                        }

                        for _ in 0..per_thread_ops / 2 {
                            // some busy looping for the case of protected_head -> next is
                            // accidentally null
                            loop {
                                if let Some(value) = q_ref.dequeue(&guard) {
                                    values_ref.lock().unwrap().push(value.value);
                                    break;
                                }
                            }
                        }

                        // second batch
                        for _ in 0..per_thread_ops / 2 {
                            q_ref.enqueue(TrackableValue::new(), &guard);
                        }

                        for _ in 0..per_thread_ops / 2 {
                            loop {
                                if let Some(value) = q_ref.dequeue(&guard) {
                                    values_ref.lock().unwrap().push(value.value);
                                    break;
                                }
                            }
                        }
                    });
                }
            });

            let actual_values: HashSet<usize> =
                collected_values.into_inner().unwrap().into_iter().collect();
            assert_eq!(actual_values, expected_values);

            NEXT_VALUE.store(0, Ordering::Relaxed);
        }
    }
}
