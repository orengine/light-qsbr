//! # light-qsbr
//!
//! `light-qsbr` provides a **lightweight Quiescent-State-Based Reclamation (QSBR)**
//! mechanism for asynchronous runtimes and lock-free data structures.
//!
//! It is designed as a minimal alternative to heavy garbage collectors and allows
//! safe memory reclamation in concurrent environments, where objects can only be freed
//! once all participating threads have advanced past a known **epoch**.
//!
//! ## Core Concepts
//!
//! - [`SharedManager`] is the **global manager**.
//!   It tracks the current global epoch and the number of executors participating in each epoch.
//!
//! - [`LocalManager`] is the **thread-local manager**.
//!   Each executor (thread or runtime worker) has its own local manager,
//!   which schedules memory to be deallocated or dropped when it is safe.
//!
//! - **Epochs** are global counters.
//!   Memory is only reclaimed once all executors have passed the epoch
//!   in which the memory was retired.
//!
//! ## Workflow
//!
//! 1. Create a [`SharedManager`].
//! 2. For each executor (thread/worker), call [`SharedManager::register_new_executor`].
//!    This installs a thread-local [`LocalManager`].
//! 3. Executors schedule memory for deallocation or dropping via
//!    [`LocalManager::schedule_deallocate`], [`LocalManager::schedule_deallocate_slice`],
//!    or [`LocalManager::schedule_drop`].
//! 4. Periodically, executors call [`LocalManager::maybe_pass_epoch`] to attempt to advance
//!    the epoch and trigger safe reclamation.
//! 5. When shutting down, executors must call
//!    `unsafe { LocalManager::deregister() }` to deregister themselves.
//!
//! ## Example
//!
//! ```rust
//! use std::cell::Cell;
//! use light_qsbr::{SharedManager, local_manager, LocalManager};
//! use light_qsbr::orengine_utils::OrengineInstant;
//!
//! # fn main() {
//! let shared = SharedManager::new();
//!
//! // Register an executor for this thread
//! shared.register_new_executor();
//!
//! // Schedule deallocation or dropping through the local manager
//! let value = Box::new(42);
//! let ptr = Box::into_raw(value);
//! unsafe {
//!     local_manager().schedule_deallocate(ptr);
//! }
//!
//! // Periodically try to advance the epoch (e.g., in an event loop)
//! local_manager().maybe_pass_epoch(OrengineInstant::now());
//!
//! // Deregister before thread exit
//! unsafe { LocalManager::deregister() };
//! # }
//! ```
//!
//! ## Safety Guarantees
//!
//! - Objects scheduled for reclamation will only be freed once **all executors**
//!   have passed the epoch in which they were retired.
//! - The API is intentionally minimal: the runtime must periodically call
//!   [`LocalManager::maybe_pass_epoch`] to make progress.
//!
//! ## When to Use
//!
//! - Implementing lock-free collections that need safe memory reclamation.
//! - Async runtimes that want QSBR without the overhead of hazard pointers or full garbage collection.
//! - Situations where *you control executor lifecycle* and can ensure correct registration/deregistration.
//!
//! ## When *Not* to Use
//!
//! - If you need automatic reclamation without explicit epoch-passing.
//! - If your workload is highly dynamic with frequent thread churn—QSBR works best with stable executor sets.
//!
//! ## See Also
//!
//! - [`SharedManager`] for the global epoch tracker.
//! - [`LocalManager`] for per-thread scheduling of reclamation.
//! - [`local_manager`] to access the thread-local [`LocalManager`].
//!
//! ---

#![deny(clippy::all)]
#![deny(clippy::assertions_on_result_states)]
#![deny(clippy::match_wild_err_arm)]
#![deny(clippy::allow_attributes_without_reason)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![warn(clippy::cargo)]
#![allow(
    clippy::multiple_crate_versions,
    reason = "They were set by dev-dependencies"
)]
#![allow(async_fn_in_trait, reason = "It improves readability.")]
#![allow(
    clippy::missing_const_for_fn,
    reason = "Since we cannot make a constant function non-constant after its release,
    we need to look for a reason to make it constant, and not vice versa."
)]
#![allow(clippy::inline_always, reason = "We write highly optimized code.")]
#![allow(
    clippy::must_use_candidate,
    reason = "It is better to developer think about it."
)]
#![allow(
    clippy::module_name_repetitions,
    reason = "This is acceptable most of the time."
)]
#![allow(
    clippy::missing_errors_doc,
    reason = "Unless the error is something special,
    the developer should document it."
)]
#![allow(clippy::redundant_pub_crate, reason = "It improves readability.")]
#![allow(clippy::struct_field_names, reason = "It improves readability.")]
#![allow(
    clippy::module_inception,
    reason = "It is fine if a file in has the same mane as a module."
)]
#![allow(clippy::if_not_else, reason = "It improves readability.")]
#![allow(
    rustdoc::private_intra_doc_links,
    reason = "It allows to create more readable docs."
)]
#![allow(
    clippy::result_unit_err,
    reason = "The function's doc should explain what it returns."
)]

mod local_manager;
mod number_of_executors;
mod deffered;
mod shared_manager;
#[cfg(test)]
mod test;

pub use orengine_utils;
pub use local_manager::*;
pub use shared_manager::*;