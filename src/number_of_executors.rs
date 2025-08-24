/// This module provides [`NumberOfExecutorsInEpoch`].
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
use orengine_utils::backoff::Backoff;
use orengine_utils::cache_padded::CachePaddedAtomicUsize;
use orengine_utils::hints::{likely, unlikely};

/// Number of bits required to store the number of executors in the epoch.
const ALL_SHIFT: usize = (usize::BITS / 2) as usize;

/// A type that represents half of the size of the pointer.
#[cfg(target_pointer_width = "64")]
type HalfUsize = u32;
/// A type that represents half of the size of the pointer.
#[cfg(target_pointer_width = "32")]
type HalfUsize = u16;
/// A type that represents half of the size of the pointer.
#[cfg(target_pointer_width = "16")]
type HalfUsize = u8;

/// `NumberOfExecutorsInEpochData` contains the number of executors registered
/// and the number of executors in the current epoch.
///
/// It is stored in a single atomic word.
#[derive(Copy, Clone)]
#[repr(C)]
pub(crate) struct NumberOfExecutorsInEpochData {
    pub(crate) all: HalfUsize,
    pub(crate) in_current_epoch: HalfUsize,
}

/// `NumberOfExecutorsInEpoch` is a thread-safe atomic [`NumberOfExecutorsInEpochData`].
pub(crate) struct NumberOfExecutorsInEpoch {
    data: CachePaddedAtomicUsize,
}

impl NumberOfExecutorsInEpoch {
    /// Creates a new `NumberOfExecutorsInEpoch`.
    pub(crate) const fn new() -> Self {
        Self {
            data: CachePaddedAtomicUsize::new(),
        }
    }
    
    /// Parses one atomic word into [`NumberOfExecutorsInEpochData`].
    const fn parse_data(data: usize) -> NumberOfExecutorsInEpochData {
        NumberOfExecutorsInEpochData {
            all: (data >> ALL_SHIFT) as HalfUsize,
            in_current_epoch: data as HalfUsize,
        }
    }

    /// Encodes [`NumberOfExecutorsInEpochData`] into one atomic word.
    const fn encode_data(data: NumberOfExecutorsInEpochData) -> usize {
        ((data.all as usize) << ALL_SHIFT) | data.in_current_epoch as usize
    }

    /// Returns the number of executors registered and 
    /// the number of executors in the current epoch.
    pub(crate) fn parsed_data_for_drop(&mut self) -> NumberOfExecutorsInEpochData {
        let encoded_data = self.data.get_mut();

        Self::parse_data(*encoded_data)
    } 

    /// Increments the number of executors registered and the number of executors
    /// in the current epoch.
    ///
    /// It locks the caller until the current epoch is updated.
    pub(crate) fn register_new_executor(&self) {
        let backoff = Backoff::new();

        loop {
            let encoded_data = self.data.load(Relaxed);
            let data = Self::parse_data(encoded_data);

            if data.in_current_epoch == 0 && data.all > 0 {
                // Another executor now updates the current epoch, we should wait

                backoff.snooze();

                continue;
            }

            let new_data = NumberOfExecutorsInEpochData {
                all: data.all + 1,
                in_current_epoch: data.in_current_epoch + 1,
            };
            let new_encoded_data = Self::encode_data(new_data);

            if self
                .data
                .compare_exchange_weak(encoded_data, new_encoded_data, AcqRel, Relaxed)
                .is_ok()
            {
                // Now all executors in this epoch know about us, and it is synchronized

                return;
            }

            backoff.snooze();
        }
    }

    /// Returns whether the caller should update the epoch.
    ///
    /// It works only if the caller hasn't decremented the counter in the current epoch.
    /// Before decrementing executor should wait until all executors in the current epoch passed the epoch.
    #[must_use]
    pub(crate) fn deregister_executor_and_decrement_counter(&self) -> bool {
        let backoff = Backoff::new();

        loop {
            let encoded_data = self.data.load(Relaxed);
            let data = Self::parse_data(encoded_data);
            let mut new_in_current_epoch = data.in_current_epoch - 1;
            let new_all = data.all - 1;
            let should_update_epoch = new_in_current_epoch == 0;

            if should_update_epoch {
                new_in_current_epoch = new_all;
            }

            let new_data = NumberOfExecutorsInEpochData {
                all: new_all,
                in_current_epoch: new_in_current_epoch,
            };
            let new_encoded_data = Self::encode_data(new_data);

            if self
                .data
                .compare_exchange_weak(encoded_data, new_encoded_data, AcqRel, Relaxed)
                .is_ok()
            {
                // Now all executors in this epoch know about us, and it is synchronized
                return should_update_epoch;
            }

            backoff.snooze();
        }
    }

    pub(crate) fn prepare_to_update_epoch(&self, all: HalfUsize) {
        debug_assert_eq!(
            Self::parse_data(self.data.load(Acquire)).in_current_epoch,
            0
        );

        let new_data = NumberOfExecutorsInEpochData {
            all,
            in_current_epoch: all,
        };
        let new_encoded_data = Self::encode_data(new_data);

        let prev = self.data.swap(new_encoded_data, AcqRel);

        assert!(!unlikely(Self::parse_data(prev).in_current_epoch != 0),
            "executor_passed_epoch was called in the current epoch more times \
            than executors are registered"
        );
    }

    /// Returns whether the caller should update the epoch.
    #[must_use]
    pub(crate) fn executor_passed_epoch(&self) -> bool {
        let encoded_data = self.data.fetch_sub(1, AcqRel) - 1;
        let data = Self::parse_data(encoded_data);

        debug_assert!(data.in_current_epoch < data.all);

        if likely(data.in_current_epoch > 0) {
            return false;
        }

        self.prepare_to_update_epoch(data.all);

        true
    }
}