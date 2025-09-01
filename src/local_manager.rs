/// This module provides the [`LocalManager`].
use std::alloc::{Layout, dealloc};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{mem, thread};
use orengine_utils::clear_with;
use orengine_utils::hints::{likely, unlikely, unwrap_or_bug_hint, unwrap_or_bug_message_hint};
use orengine_utils::OrengineInstant;
use crate::deffered::Deferred;
use crate::shared_manager::SharedManager;

/// A storage of objects that need to be deallocated or dropped.
struct Storage {
    to_deallocate: Vec<(*mut u8, Layout)>,
    to_drop: Vec<Deferred>,
}

impl Storage {
    /// Creates a new empty storage.
    const fn new() -> Self {
        Self {
            to_deallocate: Vec::new(),
            to_drop: Vec::new(),
        }
    }

    /// Clears the storage.
    ///
    /// When #[cfg(test)] is enabled, it also increments the number of bytes deallocated.
    #[allow(unused_variables, reason = "It is used for tests")]
    fn clear(&mut self, shared_manager: &SharedManager) {
        clear_with(&mut self.to_deallocate, |(ptr, layout)| unsafe {
            #[cfg(test)]
            {
                shared_manager.increment_bytes_deallocated(layout.size());
            }

            dealloc(ptr, layout);
        });

        clear_with(&mut self.to_drop, |f| {
            f.call();
        });
    }

    /// Transfers all objects from `other` to `self`.
    fn append(&mut self, other: &mut Self) {
        self.to_deallocate.append(&mut other.to_deallocate);
        self.to_drop.append(&mut other.to_drop);
    }
}

#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "We guarantee that it is `Send`"
)]
unsafe impl Send for Storage {}
unsafe impl Sync for Storage {}

/// A local manager of objects that need to be deallocated or dropped when it is safe.
///
/// It should be created by some runtime with [`SharedManager::register_new_executor`], and only
/// the runtime can call [`LocalManager::deregister`] and [`LocalManager::maybe_pass_epoch`].
/// The runtime should call [`LocalManager::deregister`] when it is stopped and should call
/// [`LocalManager::maybe_pass_epoch`] periodically (it uses cached time to prevent unnecessary
/// attempts to pass the epoch).
///
/// After the registration, the `LocalManager` is stored in thread-local storage and can be
/// accessed with [`local_manager`].
///
/// It allows scheduling objects to be deallocated or dropped when it is safe.
///
/// Use [`LocalManager::schedule_deallocate`] and [`LocalManager::schedule_deallocate_slice`]
/// to schedule deallocation of objects, and [`LocalManager::schedule_drop`] to schedule dropping
/// of objects.
///
/// # Example
///
/// ```rust
/// use std::cell::Cell;
/// use light_qsbr::{local_manager, SharedManager, orengine_utils::OrengineInstant, LocalManager};
///
/// # struct Runtime { tasks: Vec<Box<dyn FnOnce() + Send + 'static>>, is_stopped: Cell<bool> }
/// # struct LockFreeStack<T> { ptr: *mut T }
/// # struct LockFreeStackNode<T> { ptr: *mut T }
/// # impl<T> LockFreeStackNode<T> {
/// #     fn get_node_ptr(&self) -> *mut LockFreeStackNode<T> {
/// #         unreachable!()
/// #     }
/// # }
/// # impl<T> LockFreeStack<T> {
/// #     fn pop(&self) -> LockFreeStackNode<T> { LockFreeStackNode::<T> { ptr: unreachable!() } }
/// # }
/// #
/// fn start_runtime() {
///     let mut runtime = Runtime { tasks: Vec::new(), is_stopped: Cell::new(false) };
///     let shared_manager = SharedManager::new();
///
///     shared_manager.register_new_executor();
///
///     'runtime: loop {
///         for _ in 0..61 {
///             if let Some(task) = runtime.tasks.pop() {
///                 task();
///             } else {
///                 break 'runtime;
///             }
///         }
///
///         if runtime.is_stopped.get() {
///             break;
///         }
///
///         local_manager().maybe_pass_epoch(OrengineInstant::now()); // Free some memory if it is safe
///     }
///
///     unsafe { LocalManager::deregister() };
/// }
///
/// fn one_of_tasks(lock_free_stack: LockFreeStack<usize>) {
///     let node = lock_free_stack.pop();
///     let ptr_to_deallocate: *mut LockFreeStackNode<usize> = node.get_node_ptr();
///     // It's not safe to release the node right now because other threads can load it but still not read it.
///     unsafe {
///         local_manager() // Get the thread-local LocalManager
///             .schedule_deallocate(ptr_to_deallocate);
///     }
///     // The node will be deallocated when it is safe.
/// }
/// ```
pub struct LocalManager {
    current_epoch: usize,
    this_epoch_start: OrengineInstant,
    was_passed_epoch: bool,
    shared_manager: SharedManager,
    was_stopped: bool,

    prev_storage: Storage,
    current_storage: Storage,
}

impl LocalManager {
    /// Creates a new `LocalManager`.
    pub(crate) fn new(shared_manager: &SharedManager) -> Self {
        Self {
            current_epoch: shared_manager.current_epoch(),
            this_epoch_start: unsafe { MaybeUninit::zeroed().assume_init() },
            was_passed_epoch: false,
            shared_manager: shared_manager.clone(),
            was_stopped: false,

            prev_storage: Storage::new(),
            current_storage: Storage::new(),
        }
    }

    /// # Panics
    ///
    /// Panics if the [`LocalManager`] is stopped.
    #[cold]
    #[inline(never)]
    fn handle_stopped(&self) {
        assert!(self.was_stopped);

        panic!(
            "`LocalManager` is used after calling `LocalManager::temporary_deregister` \
            and before calling `LocalManager::resume_after_temporary_deregister`."
        )
    }

    /// Returns the number of bytes deallocated since creation of the `SharedManager`.
    #[cfg(test)]
    pub(crate) fn bytes_deallocated(&self) -> usize {
        self.shared_manager.bytes_deallocated()
    }

    /// Returns the current epoch.
    pub fn current_epoch(&self) -> usize {
        self.current_epoch
    }

    /// Returns a reference to the associated [`SharedManager`].
    pub fn shared_manager(&self) -> &SharedManager {
        &self.shared_manager
    }

    /// Schedules deallocation of an object. It will be deallocated when it is safe.
    ///
    /// # Safety
    ///
    /// It requires the same safety conditions as [`dealloc`].
    pub unsafe fn schedule_deallocate<T>(&mut self, ptr: *const T) {
        if unlikely(self.was_stopped) {
            self.handle_stopped();
        }

        self.current_storage
            .to_deallocate
            .push((ptr.cast::<u8>().cast_mut(), Layout::new::<T>()));
    }

    /// Schedules deallocation of the provided slice.
    /// They will be deallocated when it is safe.
    ///
    /// # Safety
    ///
    /// It requires the same safety conditions as [`dealloc`].
    ///
    /// # Panics
    ///
    ///
    pub unsafe fn schedule_deallocate_slice<T>(&mut self, ptr: *const T, len: usize) {
        if unlikely(self.was_stopped) {
            self.handle_stopped();
        }

        self.current_storage.to_deallocate.push((
            ptr.cast::<u8>().cast_mut(),
            unwrap_or_bug_hint(Layout::array::<T>(len)),
        ));
    }

    /// Schedules dropping of the provided function.
    /// It will be dropped when it is safe.
    ///
    /// The function can be a closure; therefore, it can be used to drop any object.
    ///
    /// # Safety
    ///
    /// It requires the same safety conditions as [`mem::ManuallyDrop::drop`].
    pub unsafe fn schedule_drop<F: FnOnce()>(&mut self, func: F) {
        if unlikely(self.was_stopped) {
            self.handle_stopped();
        }

        self.current_storage.to_drop.push(Deferred::new(func));
    }

    /// Collects garbage or the previous epoch's storage.
    pub(crate) fn collect_garbage(&mut self) {
        self.prev_storage.clear(&self.shared_manager);
    }

    /// Reacts to the epoch change.
    fn react_to_epoch_change(&mut self, global_epoch: usize, now: OrengineInstant) {
        debug_assert_eq!(global_epoch, self.current_epoch + 1);

        self.current_epoch = global_epoch;
        self.this_epoch_start = now;
        self.was_passed_epoch = false;

        self.collect_garbage();
    }

    /// Maybe passes the epoch and frees some memory if it is safe.
    ///
    /// This function accepts the current time as a parameter to avoid
    /// very often epoch passing.
    ///
    /// While at least one thread doesn't pass the epoch,
    /// all other threads can't free memory.
    pub fn maybe_pass_epoch(&mut self, now: impl Into<OrengineInstant>) {
        #[cfg(not(test))]
        const EXPECTED_EPOCH_DURATION: Duration = Duration::from_millis(10);

        #[cfg(test)]
        const EXPECTED_EPOCH_DURATION: Duration = Duration::from_micros(100);

        let now = now.into();

        if unlikely(self.was_stopped) {
            self.handle_stopped();
        }

        if likely(now - self.this_epoch_start < EXPECTED_EPOCH_DURATION) {
            return;
        }

        let global_epoch = self.shared_manager.current_epoch();

        if unlikely(self.current_epoch < global_epoch) {
            debug_assert!(self.was_passed_epoch);

            self.react_to_epoch_change(global_epoch, now);

            return;
        }

        debug_assert_eq!(self.current_epoch, global_epoch);

        if likely(self.was_passed_epoch) {
            return;
        }

        self.was_passed_epoch = true;

        debug_assert!(
            self.prev_storage.to_drop.is_empty() && self.prev_storage.to_deallocate.is_empty()
        );
        mem::swap(&mut self.prev_storage, &mut self.current_storage);

        let was_changed = self.shared_manager.executor_passed_epoch();
        if unlikely(was_changed) {
            self.react_to_epoch_change(global_epoch + 1, now);
        }
    }

    /// Deregisters the thread-local `LocalManager`.
    /// After that the calling [`local_manager`] can cause undefined behavior before the next
    /// [`registration`].
    ///
    /// # Safety
    ///
    /// * The thread must be registered in the [`SharedManager`].
    /// * It is called only once for one [`registration`].
    /// * After calling this function, the caller doesn't call [`local_manager`] before the next
    ///   [`registration`].
    ///
    /// # Panics
    ///
    /// It the thread is not registered.
    ///
    /// [`registration`]: SharedManager::register_new_executor
    pub unsafe fn deregister() {
        struct DeregisterInNewEpochArgs {
            epoch_at_start: usize,
            storage: Storage,
            shared_manager: SharedManager,
        }

        fn deregister_in_new_epoch(
            mut args: DeregisterInNewEpochArgs
        ) {
            fn wait_new_epoch_and_clear(
                shared_manager: &SharedManager,
                mut storage: Storage,
                epoch_at_start: usize
            ) {
                while shared_manager.current_epoch() == epoch_at_start {
                    thread::sleep(Duration::from_millis(1));
                }

                storage.clear(shared_manager);
            }

            let is_new_epoch = args.shared_manager.deregister_executor();

            if is_new_epoch {
                debug_assert_ne!(args.epoch_at_start, args.shared_manager.current_epoch());

                // The executor passed the current epoch and has been deregistered
                args.storage.clear(&args.shared_manager);

                return;
            }

            wait_new_epoch_and_clear(&args.shared_manager, args.storage, args.epoch_at_start);
        }

        let mut local_manager = LOCAL_MANAGER
            .with(|local_manager_| unsafe {
                (*local_manager_.get())
                    .take()
                    .expect("LocalManager is not registered in this thread")
            });

        let epoch_at_start = local_manager.current_epoch;
        let mut full_storage = mem::replace(&mut local_manager.current_storage, Storage::new());

        full_storage.append(&mut local_manager.prev_storage);

        let mut args = DeregisterInNewEpochArgs {
            epoch_at_start,
            storage: full_storage,
            shared_manager: local_manager
                .shared_manager
                .clone(),
        };

        // Maybe we still have not passed the current epoch
        if !local_manager.was_passed_epoch {
            deregister_in_new_epoch(args);

            return;
        }

        while args.shared_manager.current_epoch() == args.epoch_at_start {
            thread::sleep(Duration::from_millis(1));
        }

        args.epoch_at_start += 1;

        deregister_in_new_epoch(args);
    }

    /// Deregisters the thread-local `LocalManager` without reclaiming memory.
    ///
    /// It is the unsafest function in the library.
    /// It is expected to be called only before the thread is stopped
    /// and to be __resumed__ by calling [`LocalManager::resume_after_temporary_deregister`]
    /// after the thread is resumed.
    ///
    /// After this function is called and before calling
    /// [`LocalManager::resume_after_temporary_deregister`], the thread-local `LocalManager`
    /// is invalid.
    ///
    /// You should completely deregister the thread-local `LocalManager` by calling
    /// [`LocalManager::deregister`] before the thread is terminated.
    ///
    /// # Safety
    ///
    /// * The thread must be registered in the [`SharedManager`].
    /// * After calling this function, the [`LocalManager`] is not used before calling
    ///   [`LocalManager::resume_after_temporary_deregister`].
    /// * [`LocalManager::deregister`] must be called before the thread is terminated.
    ///
    /// # Example
    ///
    /// ```rust
    /// use light_qsbr::{local_manager, LocalManager, SharedManager};
    ///
    /// let shared_manager = SharedManager::new();
    ///
    /// shared_manager.register_new_executor();
    ///
    /// // Do some work
    ///
    /// // While thread is stopped it can't call `local_manager.maybe_pass_epoch`.
    /// // But completely deregister is excessive.
    /// // While `local_manager().temporary_deregister()` is inexpensive
    /// // and allows other threads to pass epochs.
    ///
    /// unsafe { local_manager().temporary_deregister(); }
    ///
    /// std::thread::sleep(std::time::Duration::from_secs(1));
    ///
    /// unsafe { local_manager().resume_after_temporary_deregister(); }
    ///
    /// // Do some work
    ///
    ///  unsafe { LocalManager::deregister(); }
    /// ```
    pub unsafe fn temporary_deregister(&mut self) {
        self.was_stopped = true;

        self.shared_manager().deregister_executor();
    }

    /// Resumes the thread-local `LocalManager` after calling [`LocalManager::temporary_deregister`].
    ///
    /// # Safety
    ///
    /// * The thread must be registered (before calling [`LocalManager::temporary_deregister`])
    ///   in the [`SharedManager`].
    /// * [`LocalManager::temporary_deregister`] must be called before calling this function.
    ///
    /// # Example
    ///
    /// You can find an example in [`LocalManager::temporary_deregister`].
    pub unsafe fn resume_after_temporary_deregister(&mut self) {
        self.was_stopped = false;

        self.shared_manager().register_executor_again();
    }
}

thread_local! {
    /// A thread-local [`LocalManager`].
    pub(crate) static LOCAL_MANAGER: UnsafeCell<Option<LocalManager>> = const { UnsafeCell::new(None) };
}

/// Returns a reference to the thread-local [`LocalManager`].
///
/// # Panics
///
/// It the thread is not registered.
///
/// # Undefined behavior
///
/// If the thread is not registered when `cfg(debug_assertions)` is disabled,
/// it causes undefined behavior.
pub fn local_manager() -> &'static mut LocalManager {
    LOCAL_MANAGER
        .with(|local_manager| unsafe {
            unwrap_or_bug_message_hint(
                (*local_manager.get()).as_mut(),
                "Local manager is not registered in this thread."
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};
    use super::*;

    #[test]
    fn test_temporary_deregister() {
        let shared_manager = SharedManager::new();
        let was_started = Arc::new((Mutex::new(false), Condvar::new()));
        let was_started_clone = was_started.clone();
        let was_passed = Arc::new((Mutex::new(false), Condvar::new()));
        let was_passed_clone = was_passed.clone();

        shared_manager.register_new_executor();

        let handle = thread::spawn(move || {
            shared_manager.register_new_executor();

            *was_started.0.lock().unwrap() = true;

            was_started.1.notify_one();

            for _ in 0..10 {
                local_manager().maybe_pass_epoch(OrengineInstant::now());

                thread::sleep(Duration::from_millis(1));
            }

            *was_passed.0.lock().unwrap() = true;

            was_passed.1.notify_one();

            unsafe { LocalManager::deregister(); }
        });

        let mut started = was_started_clone.0.lock().unwrap();
        while !*started {
            started = was_started_clone.1.wait(started).unwrap();
        }

        drop(started);

        unsafe { local_manager().temporary_deregister(); }

        let mut passed = was_passed_clone.0.lock().unwrap();
        let mut timeout_error;
        while !*passed {
            (passed, timeout_error) = was_passed_clone.1.wait_timeout(passed, Duration::from_secs(3)).unwrap();

            assert!(!timeout_error.timed_out());
        }

        drop(passed);

        unsafe {
            local_manager().resume_after_temporary_deregister();
            LocalManager::deregister();
        }

        handle.join().unwrap();
    }
}