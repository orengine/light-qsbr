# light-qsbr

A **lightweight Quiescent-State-Based Reclamation (QSBR)** library for Rust.

It provides a minimal, efficient mechanism for safe memory reclamation in concurrent
and asynchronous runtimes, without the complexity of a garbage collector or hazard pointers.

---

## ✨ Features

- 🚀 Extremely lightweight — only the essential QSBR pieces.
- 🧵 Thread-local memory managers with a global epoch coordinator.
- 🗑️ Safe reclamation of memory scheduled by executors.
- 🛠️ Designed for async runtimes and lock-free data structures.

---

## 📚 Core Concepts

- **[`SharedManager`](https://docs.rs/light-qsbr/latest/light_qsbr/struct.SharedManager.html)**  
  The global manager. Tracks the current epoch and number of executors.

- **[`LocalManager`](https://docs.rs/light-qsbr/latest/light_qsbr/struct.LocalManager.html)**  
  The thread-local manager.
  Each executor registers one and uses it to schedule
  memory for deallocation or dropping.

- **Epochs**  
  Executors periodically advance epochs.
  Memory is only freed once **all executors**
  have passed the epoch in which the memory was retired.

---

## ⚡ Quick Example

```rust
use light_qsbr::{SharedManager, local_manager};
use light_qsbr::orengine_utils::OrengineInstant;

fn main() {
    // Create the global manager
    let shared = SharedManager::new();

    // Register an executor for this thread
    shared.register_new_executor();

    // Schedule deallocation
    let value = Box::new(42);
    let ptr = Box::into_raw(value);
    unsafe {
        local_manager().schedule_deallocate(ptr);
    }

    // Periodically (better when runtime is polling) try to pass the epoch
    local_manager().maybe_pass_epoch(OrengineInstant::now());

    // Deregister before thread exit
    unsafe { LocalManager::deregister() };
}
```

✅ When to Use

- Implementing lock-free collections that need safe reclamation.

- Async runtimes that want QSBR without the overhead of hazard pointers.

- Situations where you control the executor lifecycle and can enforce correct registration/deregistration.

🚫 When Not to Use

- If executors come and go frequently (executors are short-lived).

- If you want fully automatic memory management (this is not a GC).