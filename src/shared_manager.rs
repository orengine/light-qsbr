/// This module provides the [`SharedManager`].
use std::sync::atomic::Ordering::{AcqRel, Relaxed};
use orengine_utils::cache_padded::CachePaddedAtomicUsize;
use orengine_utils::hints::unlikely;
use orengine_utils::light_arc::LightArc;
use crate::local_manager::{LocalManager, LOCAL_MANAGER};
use crate::number_of_executors::NumberOfExecutorsInEpoch;

/// The internal state of the [`SharedManager`].
///
/// This struct is reference-counted via [`LightArc`] and shared between all
/// [`SharedManager`] instances. It contains the global epoch, the number of
/// executors in each epoch, and (in tests) a counter of the bytes deallocated number.
struct Inner {
    current_epoch: CachePaddedAtomicUsize,
    number_of_executors_in_epoch: NumberOfExecutorsInEpoch,
    #[cfg(test)]
    bytes_deallocated: CachePaddedAtomicUsize,
}

/// A shared manager that coordinates memory reclamation across executors.
///
/// The [`SharedManager`] is the central structure that keeps a number of registered executors,
/// a number of executors that passed the current epoch, and the current epoch number.
///
/// Executors register themselves with the [`SharedManager`] and get
/// a thread-local [`LocalManager`], which they use to schedule memory for deallocation or dropping.
///
/// The [`SharedManager`] ensures that memory is only freed when it is safe — that is,
/// once all executors have advanced past the epoch in which the memory was retired.
///
/// # Usage
///
/// 1. Create a new [`SharedManager`] with [`SharedManager::new`].
/// 2. For each executor (thread or runtime worker), call
///    [`SharedManager::register_new_executor`] to install a thread-local [`LocalManager`].
/// 3. Executors periodically call [`LocalManager::maybe_pass_epoch`] to advance epochs
///    and allow reclamation.
/// 4. When an executor is done, it must deregister with
///    `unsafe { local_manager().deregister() }`.
///
/// # Example
///
/// You can find a very detailed example in the [`LocalManager`]'s docs.
#[derive(Clone)]
pub struct SharedManager {
    inner: LightArc<Inner>,
}

impl SharedManager {
    /// Creates a new [`SharedManager`].
    ///
    /// This function initializes the global epoch and prepares the internal state
    /// for executor registration.
    pub fn new() -> Self {
        Self {
            inner: LightArc::new(Inner {
                current_epoch: CachePaddedAtomicUsize::new(),
                number_of_executors_in_epoch: NumberOfExecutorsInEpoch::new(),
                #[cfg(test)]
                bytes_deallocated: CachePaddedAtomicUsize::new(),
            })
        }
    }

    /// Increments the counter of deallocated bytes (test-only).
    #[cfg(test)]
    pub(crate) fn increment_bytes_deallocated(&self, bytes: usize) {
        self.inner.bytes_deallocated.fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
    }

    /// Returns the total number of bytes deallocated since creation (test-only).
    #[cfg(test)]
    pub(crate) fn bytes_deallocated(&self) -> usize {
        self.inner.bytes_deallocated.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Registers a new executor in the current thread.
    ///
    /// This creates and installs a thread-local [`LocalManager`] associated
    /// with this [`SharedManager`].
    ///
    /// # Panics
    ///
    /// Panics if the current thread already has a registered [`LocalManager`].
    pub fn register_new_executor(&self) {
        self.inner.number_of_executors_in_epoch.register_new_executor();

        LOCAL_MANAGER.with(|local_manager_| {
            let local_manager = unsafe { &mut *local_manager_.get() };

            assert!(
                local_manager
                    .replace(LocalManager::new(self))
                    .is_none(),
                "Attempt to register local manager in a thread that already has a local manager. \
                Each thread can be registered only once"
            );
        });
    }

    /// Deregisters an executor from the current epoch.
    ///
    /// Returns `true` if all executors have left the current epoch, which means
    /// the global epoch can safely be advanced.
    pub(crate) fn deregister_executor(&self) -> bool {
        if unlikely(
            self
                .inner
                .number_of_executors_in_epoch
                .deregister_executor_and_decrement_counter(),
        ) {
            self.inner.current_epoch.fetch_add(1, AcqRel);

            return true;
        }

        false
    }

    /// Marks the calling executor as having passed the current epoch.
    ///
    /// Returns `true` if this was the last executor in the epoch, meaning
    /// the global epoch is incremented and memory can be reclaimed.
    pub(crate) fn executor_passed_epoch(&self) -> bool {
        if unlikely(self.inner.number_of_executors_in_epoch.executor_passed_epoch()) {
            // All executors passed the epoch, we can update the current epoch
            self.inner.current_epoch.fetch_add(1, AcqRel);

            return true;
        }

        false
    }

    /// Returns the current global epoch.
    pub(crate) fn current_epoch(&self) -> usize {
        self.inner.current_epoch.load(Relaxed)
    }
}

impl Default for SharedManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let data = self
            .number_of_executors_in_epoch
            .parsed_data_for_drop();

        assert!(
            data.in_current_epoch == 0 && data.all == 0,
            "Some executors are still registered when the shared manager is dropped. \
            Make sure to call `deregister_executor` for all registered executors \
            (use {code}.",
            code = "unsafe { local_manager().deregister() }"
        );
    }
}