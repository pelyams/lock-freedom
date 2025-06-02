use crate::mechanisms::hp::*;
use std::sync::atomic::{fence, AtomicPtr, Ordering};
use std::ptr;
use std::default::Default;
use crate::utils::backoff::Backoff;

/*
    here, MSQueue is altered to solve a problem of having two CAS operations in enqueue() method:
    one for updating self.tail atomic pointer, and second one for storing next node pointer in 
    previous tail (both being concurrent, hence CAS-based). optimistic approach suggests restructure
    queue, so that it is built upon a doubly-linked list (with notable nuance: it's direction is now
    reversed and 'next's are serve pointing direction from tail to head) that can help to get rid of
    latter CAS and substitute it with a regular store operation.
*/

struct OMSQueue<T: Default> {
    head: AtomicPtr<QueueNode<T>>,
    tail: AtomicPtr<QueueNode<T>>,
}

struct Node<T: Default> {
    data: T,
    next: AtomicPtr<QueueNode<T>>,
    prev: AtomicPtr<QueueNode<T>>,
}

#[repr(transparent)]
pub struct QueueNode<T: Default>(Node<T>);

impl<T: Default> OMSQueue<T> {
    pub fn new() -> OMSQueue<T> {
        let dummy_node = Box::into_raw(Box::new(Node {
            data: T::default(),
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
        })) as *mut QueueNode<T>;
        OMSQueue {
            head: AtomicPtr::new(dummy_node),
            tail: AtomicPtr::new(dummy_node),
        }
    }
    
    /*
    new node atomically stores its next to current self.tail. after that, we try to CAS self.tail
    to the new node ptr. comparing to pessimistic variant, this time we are more diligent: tail 
    shouldn't lag, or algorithm would be incorrect. on success, we store prev value of older tail 
    to point to new node. there could be a case when thread is preempted before the last step: that
    makes queue temporarily inconsistent: chain of 'prev's is broken. for this reason, if dequeueing
    thread accidentally finds head->prev to be a nullptr in non-empty queue, it first runs fix() 
    method
     */
    
    pub fn enqueue(&self, data: T, guard: &HazardPointerGuard<QueueNode<T>>) -> bool {
        let new_node = Box::into_raw(Box::new(Node {
            data,
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
        })) as *mut QueueNode<T>;

        let mut backoff = Backoff::new();
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let mut protected_tail = match unsafe { guard.protect(tail) } {
                Ok(ptr) => {
                    fence(Ordering::Acquire);
                    ptr
                },
                Err(ProtectionError::NoAvailableIndices) => {
                    backoff.spin();
                    continue;
                },
                Err(ProtectionError::NullPointer) => {
                    panic!("OMSQueue::enqueue(): found null pointer while protecting tail");
                },
            };

            unsafe {
                (*(new_node as *mut Node<T>)).next.store(tail, Ordering::Release);
            }

            if protected_tail.as_mut_ptr() != self.tail.load(Ordering::Relaxed) { continue; }
            if self.tail.compare_exchange(protected_tail.as_mut_ptr(), new_node, Ordering::Release, Ordering::Relaxed).is_ok() {
                // attempt to store new_node in older tail prev
                unsafe { &*protected_tail.as_mut_ptr() }.0.prev.store(new_node, Ordering::Release);
                return true;
            }
        };
    }
    
    pub fn dequeue(&self, guard: &HazardPointerGuard<QueueNode<T>>) -> Option<T> {
        let mut hp_backoff = Backoff::new();
        
        let mut head_ptr = std::mem::MaybeUninit::<*mut QueueNode<T>>::uninit();
        let mut tail_ptr = std::mem::MaybeUninit::<*mut QueueNode<T>>::uninit();

        loop {
            head_ptr.write(self.head.load(Ordering::Relaxed));

            let mut protected_head = match unsafe { guard.protect(head_ptr.assume_init_read()) } {
                Ok(ptr) => { fence(Ordering::Acquire); ptr },
                // head can't be empty, ignore ProtectionError::NullPointer 
                Err(ProtectionError::NoAvailableIndices) => {
                    hp_backoff.spin();
                    continue;
                },
                Err(ProtectionError::NullPointer) => {
                    panic!("OMSQueue::dequeue(): found null pointer while protecting head");
                }
            };
            
            tail_ptr.write(self.tail.load(Ordering::Relaxed));
            let protected_tail = match unsafe {  guard.protect(tail_ptr.assume_init_read()) }{
                Ok(ptr) => { fence(Ordering::Acquire); ptr },
                Err(ProtectionError::NoAvailableIndices) => {
                    hp_backoff.spin();
                    continue;
                },
                Err(ProtectionError::NullPointer) => { 
                    panic!("OMSQueue::dequeue(): found null pointer while protecting tail");
                },
            };
            
            if protected_head.as_ptr() != self.head.load(Ordering::Relaxed) ||
                protected_tail.as_ptr() != self.tail.load(Ordering::Relaxed) { continue; }

            if protected_head.as_ptr() != protected_tail.as_ptr(){

                if !protected_head.0.prev.load(Ordering::Relaxed).is_null() {
                    // okay, behead queue and proceed to (protected) prev
                    if protected_head.as_ptr() != self.head.load(Ordering::Relaxed) {
                        continue;
                    }

                    let head_prev = protected_head.0.prev.load(Ordering::Relaxed);

                    let mut protected_head_prev = loop {
                        match unsafe { guard.protect(head_prev)} {

                            Ok(ptr) => {
                                fence(Ordering::Acquire);
                                break ptr;
                            },
                            Err(ProtectionError::NoAvailableIndices) => {
                                hp_backoff.spin();
                                continue;
                            },
                            // never case, since head != tail and tail doesn't lag
                            Err(ProtectionError::NullPointer) => {
                                panic!("OMSQueue::dequeue(): found null pointer while protecting head_prev");
                            }
                        }
                    };

                    if self.head.load(Ordering::Relaxed) != protected_head.as_mut_ptr() {
                        continue;
                    }

                    if self.head.compare_exchange(protected_head.as_mut_ptr(), protected_head_prev.as_mut_ptr(), Ordering::Release, Ordering::Relaxed ).is_ok(){
                        guard.retire_node(protected_head);
                        return Some(std::mem::take(&mut protected_head_prev.0.data));
                    };
                }
                self.fix(protected_head, protected_tail, guard);
                continue;
            }
            return None;
        }
    }
    
    fn fix(
        &self,
        head: ProtectedPointer<QueueNode<T>>, 
        tail: ProtectedPointer<QueueNode<T>>, 
        guard: &HazardPointerGuard<QueueNode<T>>
    ) {
        let mut backoff = Backoff::new();
        let mut current = tail;
        
        // we also check protected head doesn't have a stale ptr: another thread could succeed in 
        // fixing things and likely there were several consecutive dequeues. if we ignore such a case
        // current_next might read after free
        while current.as_ptr() != head.as_ptr() && head.as_ptr() == self.head.load(Ordering::Relaxed) {
            let current_next = match unsafe { guard.protect((*current).0.next.load(Ordering::Relaxed)) } {
                Ok(ptr) => {
                    fence(Ordering::Acquire);
                    ptr
                },
                Err(ProtectionError::NoAvailableIndices) => { 
                    backoff.spin();
                    continue;
                },
                Err(ProtectionError::NullPointer) => {
                    if head.as_ptr() != self.head.load(Ordering::Relaxed) { return }
                    panic!("OMSQueue:: fix(): found null pointer while protecting nodes");
                },
            };
            backoff.reset();
            if  current_next.0.prev.load(Ordering::Relaxed).is_null() {
                 current_next.0.prev.store(current.as_mut_ptr(),Ordering::Release) ;
            } 
            current = current_next;
        }
    }
}


unsafe impl<T: Default> Sync for OMSQueue<T> {}

#[cfg(test)]
mod tests {
    use super::OMSQueue;
    use crate::mechanisms::hp::HazardPointerArray;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::LazyLock;

    static HP_ARRAY: LazyLock<HazardPointerArray> = LazyLock::new(|| HazardPointerArray::new());

    #[test]
    fn test_basic_operations() {
        let q = OMSQueue::new();
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
        let q = OMSQueue::new();
        let q_ref = &q;

        let thread_count = 8;
        let per_thread_ops = 64;
        let expected_values: HashSet<usize> = (0..thread_count * per_thread_ops).collect();

        for _ in 0..255000{
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
