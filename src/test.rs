//! This module provides tests for the epoch GC (including stress tests).
use std::alloc::{dealloc, Layout};
use std::sync::{Arc, Mutex as SyncMutex, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use orengine_utils::instant::OrengineInstant;
use crate::{local_manager, LocalManager, SharedManager};

struct DroppableElement {
    value: u64,
    slice: Arc<SyncMutex<Vec<u64>>>,
}

impl DroppableElement {
    fn new(value: u64, slice: Arc<SyncMutex<Vec<u64>>>) -> Self {
        Self { value, slice }
    }
}

impl Drop for DroppableElement {
    fn drop(&mut self) {
        self.slice.lock().unwrap().push(self.value);
    }
}

fn wait_new_epoch() {
    let start_epoch = local_manager().shared_manager().current_epoch();

    while start_epoch == local_manager().shared_manager().current_epoch() {
        local_manager().maybe_pass_epoch(OrengineInstant::now());

        thread::sleep(Duration::from_micros(1));
    }
}

#[test]
fn test_basic_deallocation() {
    let value = 42;
    let size = size_of_val(&value);
    let ptr = Box::into_raw(Box::new(value));
    let shared_manager = SharedManager::new();

    shared_manager.register_new_executor();

    let initial_bytes = local_manager().bytes_deallocated();

    unsafe {
        local_manager().schedule_deallocate(ptr);
    }

    assert_eq!(initial_bytes, local_manager().bytes_deallocated());

    wait_new_epoch();

    assert_eq!(
        initial_bytes + size,
        local_manager().bytes_deallocated()
    );

    unsafe { LocalManager::deregister() };
}

#[test]
fn test_slice_deallocation() {
    let len = 5;
    let slice = vec![0; len].into_boxed_slice();
    let ptr = Box::into_raw(slice) as *const i32;
    let expected_bytes = size_of::<i32>() * len;
    let shared_manager = SharedManager::new();

    shared_manager.register_new_executor();

    let initial_bytes = local_manager().bytes_deallocated();

    unsafe {
        local_manager().schedule_deallocate_slice(ptr, len);
    };

    assert_eq!(local_manager().bytes_deallocated(), initial_bytes);

    wait_new_epoch();

    assert_eq!(
        local_manager().bytes_deallocated(),
        initial_bytes + expected_bytes
    );

    unsafe { LocalManager::deregister() };
}

#[test]
fn test_drop_function() {
    let slice = Arc::new(SyncMutex::new(Vec::new()));
    let elem = DroppableElement::new(0, slice.clone());
    let shared_manager = SharedManager::new();

    shared_manager.register_new_executor();

    unsafe {
        local_manager().schedule_drop(move || {
            drop(elem);
        });
    }

    assert_eq!(slice.lock().unwrap().len(), 0);

    wait_new_epoch();

    assert_eq!(slice.lock().unwrap().len(), 1);

    unsafe { LocalManager::deregister() };
}

#[test]
fn test_concurrent_dealloc_and_drop() {
    for _ in 0..3 {
        let slice = Arc::new(SyncMutex::new(Vec::new()));
        let droppable_elem = DroppableElement::new(0, slice.clone());
        let value = 42;
        let size = size_of_val(&value);
        let ptr = Box::into_raw(Box::new(value));
        let shared_manager = SharedManager::new();
        let was_finished = Arc::new(AtomicBool::new(false));

        shared_manager.register_new_executor();

        let initial_bytes = local_manager().bytes_deallocated();

        unsafe {
            local_manager().schedule_deallocate(ptr);
            local_manager().schedule_drop(move || {
                drop(droppable_elem);
            });
        }

        let slice_clone = slice.clone();
        let was_finished_clone = was_finished.clone();
        let t1 = thread::spawn(move || {
            shared_manager.register_new_executor();

            let slice = slice_clone.clone();

            unsafe {
                local_manager().schedule_drop(move || {
                    drop(DroppableElement::new(1, slice_clone));
                });
            }

            while !was_finished_clone.load(Ordering::SeqCst) {
                local_manager().maybe_pass_epoch(OrengineInstant::now());

                thread::yield_now();
            }

            assert_eq!(
                initial_bytes + size,
                local_manager().bytes_deallocated()
            );
            assert_eq!(slice.lock().unwrap().len(), 2);

            unsafe { LocalManager::deregister() };
        });

        thread::sleep(Duration::from_millis(40));

        assert_eq!(initial_bytes, local_manager().bytes_deallocated());
        assert_eq!(slice.lock().unwrap().len(), 0);

        wait_new_epoch(); // current -> prev
        wait_new_epoch(); // prev -> remove sometime
        wait_new_epoch(); // definitely removed

        assert_eq!(
            initial_bytes + size,
            local_manager().bytes_deallocated()
        );
        assert_eq!(slice.lock().unwrap().len(), 2);

        was_finished.store(true, Ordering::SeqCst);

        t1.join().unwrap();

        unsafe { LocalManager::deregister() };
    }
}

mod limited_allocator {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use orengine_utils::instant::OrengineInstant;
    use crate::local_manager;
    use crate::test::wait_new_epoch;

    pub(crate) const MAX_MEMORY_ALLOCATED: usize = if cfg!(miri) {
        5 * 1024
    } else {
        2 * 1024 * 1024
    };

    pub(crate) struct Allocator {
        memory_used_from_start: AtomicUsize,
    }

    impl Allocator {
        pub(crate) const fn new() -> Self {
            Self {
                memory_used_from_start: AtomicUsize::new(0),
            }
        }

        pub(crate) fn reset(&self) {
            for _ in 0..3 {
                wait_new_epoch();
            }

            self.memory_used_from_start.store(0, Ordering::SeqCst);

            assert_eq!(self.memory_used_now(), 0);
        }

        fn memory_used_now(&self) -> usize {
            let mut tries = 0;

            loop {
                let memory_used_from_start = self
                    .memory_used_from_start
                    .load(Ordering::Relaxed);
                let freed = local_manager().bytes_deallocated();

                if freed > memory_used_from_start {
                    // Inconsistent state, retry

                    tries += 1;

                    assert!(tries < 10, "Deallocated memory is more than allocated");

                    local_manager().maybe_pass_epoch(OrengineInstant::now());

                    continue;
                }

                break memory_used_from_start - freed;
            }
        }

        pub(crate) fn allocate<T>(&self) -> *mut T {
            let layout = std::alloc::Layout::new::<T>();

            loop {
                let memory_used = self.memory_used_now();

                if memory_used + layout.size() > MAX_MEMORY_ALLOCATED {
                    wait_new_epoch();

                    continue;
                }

                self
                    .memory_used_from_start
                    .fetch_add(layout.size(), Ordering::AcqRel);

                break unsafe { std::alloc::alloc(layout).cast() };
            }
        }
    }

    static LIMITED_ALLOCATOR: Allocator = Allocator::new();

    pub(crate) fn limited_allocator() -> &'static Allocator {
        &LIMITED_ALLOCATOR
    }
}

mod lock_free_stack {
    use super::limited_allocator::limited_allocator;
    use std::panic::UnwindSafe;
    use std::ptr;
    use std::ptr::null_mut;
    use std::sync::atomic::{AtomicPtr, Ordering};

    pub(crate) struct Node<T> {
        data: T,
        next: AtomicPtr<Node<T>>,
    }

    fn allocate_node<T>(data: T) -> *mut Node<T> {
        let node_ptr: *mut Node<T> = limited_allocator().allocate();

        unsafe {
            node_ptr.write(Node {
                data,
                next: AtomicPtr::new(null_mut()),
            });
        };

        node_ptr
    }

    pub struct LockFreeStack<T, D: Fn(*mut Node<T>)> {
        head: AtomicPtr<Node<T>>,
        drop_fn: D,
    }

    impl<T, D: Fn(*mut Node<T>)> LockFreeStack<T, D> {
        pub fn new(drop_fn: D) -> Self {
            Self {
                head: AtomicPtr::new(null_mut()),
                drop_fn,
            }
        }

        #[allow(clippy::future_not_send, reason = "It is a test.")]
        pub fn push(&self, data: T) {
            let new_node = allocate_node(data);
            let mut old_head = self.head.load(Ordering::Relaxed);

            loop {
                if !cfg!(miri) {
                    *(unsafe { &mut *new_node }.next.get_mut()) = old_head;
                } else {
                    unsafe { &*new_node }.next.store(old_head, Ordering::Relaxed);
                }

                match self.head.compare_exchange(
                    old_head,
                    new_node,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        break;
                    }
                    Err(current_head) => {
                        old_head = current_head;
                    }
                }
            }
        }

        pub fn pop(&self) -> Option<T> {
            let mut old_head = self.head.load(Ordering::Relaxed);

            loop {
                if old_head.is_null() {
                    return None;
                }

                let node_ref = unsafe { &*old_head };
                let next = node_ref.next.load(Ordering::Acquire);

                match self.head.compare_exchange(
                    old_head,
                    next,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let data = unsafe { ptr::read(&node_ref.data) };

                        (self.drop_fn)(old_head);

                        return Some(data);
                    }
                    Err(current_head) => {
                        old_head = current_head;
                    }
                }
            }
        }

        fn clear(&mut self) {
            let mut node = self.head.load(Ordering::Acquire);

            self.head.store(null_mut(), Ordering::Release);

            while !node.is_null() {
                let node_ref = unsafe { &*node };
                let next = node_ref.next.load(Ordering::Acquire);

                (self.drop_fn)(node);

                node = next;
            }
        }
    }

    impl<T, D: Fn(*mut Node<T>)> UnwindSafe for LockFreeStack<T, D> {}

    impl<T, D: Fn(*mut Node<T>)> Drop for LockFreeStack<T, D> {
        fn drop(&mut self) {
            self.clear();
        }
    }
}

const SIZE: usize = 128;

type StackForTests<F> = lock_free_stack::LockFreeStack<[u8; SIZE], F>;

#[cfg(test)]
fn stress_test_lock_free_stack<F, Creator>(creator: Creator, shared_manager: &SharedManager)
where
    F: Fn(*mut lock_free_stack::Node<[u8; SIZE]>) + Send + Sync + 'static,
    Creator: Fn() -> StackForTests<F>,
{
    const TRIES: usize = 3;
    const PAR: usize = 2;
    const N: usize = if cfg!(miri) {
        120
    } else {
        210_000
    };
    const COUNT_PER_THREAD: usize = N / PAR;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    let test_lock = TEST_LOCK
        .lock()
        .unwrap_or_else(|e| {
            TEST_LOCK.clear_poison();

            e.into_inner()
        });

    shared_manager.register_new_executor();

    limited_allocator::limited_allocator().reset();

    unsafe { LocalManager::deregister() };

    for _ in 0..TRIES {
        let stack = Arc::new(creator());
        let handles = (0..PAR)
            .map(|_| {
                let shared_manager = shared_manager.clone();
                let stack = stack.clone();

                thread::spawn(move || {
                    shared_manager.register_new_executor();

                    for i in 0..COUNT_PER_THREAD {
                        stack.push([0u8; SIZE]);

                        if i % 1000 == 0 {
                            local_manager().maybe_pass_epoch(OrengineInstant::now());
                        }

                        while stack.pop().is_none() {
                            local_manager().maybe_pass_epoch(OrengineInstant::now());
                        }
                    }

                    drop(stack);

                    unsafe { LocalManager::deregister() };
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(stack.pop().is_none());
    }

    drop(test_lock);
}

#[test]
fn stress_test_lock_free_stack_deallocate() {
    let shared_manager = SharedManager::new();

    stress_test_lock_free_stack(|| {
        lock_free_stack::LockFreeStack::new(|ptr| unsafe {
            local_manager().schedule_deallocate(ptr);
        })
    }, &shared_manager);
}

#[test]
fn stress_test_lock_free_stack_drop() {
    let shared_manager = SharedManager::new();
    let shared_manager2 = shared_manager.clone();

    stress_test_lock_free_stack(|| {
        let shared_manager = shared_manager.clone();

        lock_free_stack::LockFreeStack::new(move |ptr| unsafe {
            let shared_manager_ref = &shared_manager;

            assert!(!ptr.is_null());

            local_manager().schedule_drop(move || {
                dealloc(ptr.cast(), Layout::for_value(&*ptr));

                shared_manager_ref.increment_bytes_deallocated(Layout::for_value(&*ptr).size());
            });
        })
    }, &shared_manager2);

    drop(shared_manager);
}