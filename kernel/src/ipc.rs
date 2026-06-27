//! Inter-Process Communication (IPC): synchronous message passing.
//!
//! In a microkernel, IPC is *the* central mechanism — drivers, file systems and
//! services are separate tasks that talk to each other (and to the kernel) by
//! exchanging messages. PICKLE OS implements a small, synchronous, endpoint-based
//! IPC modeled loosely on seL4/Mach ports:
//!
//!   * An **endpoint** is a kernel object with a message queue and a set of
//!     blocked receivers. Endpoints can be looked up by a well-known name.
//!   * [`send`] enqueues a message and wakes a waiting receiver (asynchronous).
//!   * [`receive`] blocks until a message is available (synchronous receive).
//!   * [`call`] = send + block until the peer [`reply`]s — the classic RPC
//!     pattern services use to answer client requests.
//!
//! All blocking integrates with the [`crate::task`] scheduler: a task waiting on
//! IPC is marked `Blocked` and the CPU runs someone else until it's woken.

use crate::task;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

/// Opaque handle to an endpoint.
pub type EndpointId = u64;

/// A fixed-size message. Small messages are copied by value (fast path); larger
/// payloads should be transferred via shared memory referenced by these words.
#[derive(Debug, Clone, Copy)]
pub struct Message {
    /// Application-defined message type / opcode.
    pub tag: u64,
    /// Inline data words (e.g. syscall-style arguments or small results).
    pub words: [u64; 6],
    /// Task id of the sender, filled in by the kernel on send.
    pub sender: u64,
}

impl Message {
    /// Construct a message carrying just a tag (zeroed data words).
    pub fn new(tag: u64) -> Self {
        Message {
            tag,
            words: [0; 6],
            sender: 0,
        }
    }

    /// Construct a message with a tag and explicit data words.
    pub fn with_words(tag: u64, words: [u64; 6]) -> Self {
        Message {
            tag,
            words,
            sender: 0,
        }
    }
}

/// One IPC endpoint: a message queue plus the ids of tasks blocked receiving.
struct Endpoint {
    queue: VecDeque<Message>,
    waiters: VecDeque<u64>,
}

impl Endpoint {
    fn new() -> Self {
        Endpoint {
            queue: VecDeque::new(),
            waiters: VecDeque::new(),
        }
    }
}

/// Global IPC registry.
struct IpcState {
    endpoints: BTreeMap<EndpointId, Endpoint>,
    names: BTreeMap<String, EndpointId>,
    /// Pending RPC replies keyed by the *caller's* task id.
    replies: BTreeMap<u64, Option<Message>>,
}

static NEXT_EP: AtomicU64 = AtomicU64::new(1);
static STATE: Mutex<Option<IpcState>> = Mutex::new(None);

/// Initialize the IPC subsystem (needs the heap).
pub fn init() {
    *STATE.lock() = Some(IpcState {
        endpoints: BTreeMap::new(),
        names: BTreeMap::new(),
        replies: BTreeMap::new(),
    });
}

fn with_state<R>(f: impl FnOnce(&mut IpcState) -> R) -> R {
    // Disable interrupts while holding the IPC lock so a task cannot be
    // preempted mid-critical-section (which would deadlock the spinlock if the
    // next task also touches IPC).
    interrupts::without_interrupts(|| {
        let mut guard = STATE.lock();
        let s = guard.as_mut().expect("ipc not initialized");
        f(s)
    })
}

/// Create a fresh, anonymous endpoint and return its id.
pub fn create_endpoint() -> EndpointId {
    let id = NEXT_EP.fetch_add(1, Ordering::SeqCst);
    with_state(|s| {
        s.endpoints.insert(id, Endpoint::new());
    });
    id
}

/// Create (or fetch) an endpoint registered under a well-known `name`. Services
/// use this so clients can find them with [`lookup`].
pub fn create_named_endpoint(name: &str) -> EndpointId {
    with_state(|s| {
        if let Some(&id) = s.names.get(name) {
            return id;
        }
        let id = NEXT_EP.fetch_add(1, Ordering::SeqCst);
        s.endpoints.insert(id, Endpoint::new());
        s.names.insert(String::from(name), id);
        id
    })
}

/// Look up an endpoint id by its registered name.
pub fn lookup(name: &str) -> Option<EndpointId> {
    with_state(|s| s.names.get(name).copied())
}

/// Send `msg` to an endpoint, waking one blocked receiver if present.
/// Asynchronous: returns as soon as the message is enqueued.
pub fn send(ep: EndpointId, mut msg: Message) {
    msg.sender = task::current_id();
    let woke = with_state(|s| {
        match s.endpoints.get_mut(&ep) {
            Some(endpoint) => {
                endpoint.queue.push_back(msg);
                endpoint.waiters.pop_front()
            }
            None => {
                // Silently drop messages to non-existent endpoints.
                // The alternative (panic with disabled interrupts) would
                // permanently deadlock the kernel on a single-CPU system.
                None
            }
        }
    });
    if let Some(w) = woke {
        task::unblock(w);
    }
}

/// Block until a message is available on `ep`, then return it.
pub fn receive(ep: EndpointId) -> Message {
    loop {
        // Fast path: a message is already queued. with_state disables
        // interrupts internally to prevent preemption during state access.
        let popped = with_state(|s| {
            match s.endpoints.get_mut(&ep) {
                Some(endpoint) => endpoint.queue.pop_front(),
                None => None, // endpoint gone — return None (handled below)
            }
        });
        if let Some(m) = popped {
            return m;
        }
        // Slow path: register as a waiter, block, and retry when woken.
        let me = task::current_id();
        with_state(|s| {
            if let Some(endpoint) = s.endpoints.get_mut(&ep) {
                // Check queue again before registering (avoids lost wake-up
                // race — a message may have arrived between the fast-path and
                // the waiter registration).
                if let Some(m) = endpoint.queue.pop_front() {
                    // Store the message so we can return it after waking.
                    s.replies.insert(me, Some(m));
                    return Some(());
                }
                // Remove any stale entry for this task before pushing.
                // On a spurious wake (e.g. from a signal or timeout) we
                // loop back here and would otherwise accumulate duplicates
                // in the waiters list, causing unbounded growth and
                // potentially waking the same task multiple times for
                // one message.
                endpoint.waiters.retain(|&w| w != me);
                endpoint.waiters.push_back(me);
            }
            None::<()>
        });
        // Mark blocked and switch away. interrupts::without_interrupts is
        // used because with_state already handles the IPC lock, and we
        // need scheduler::with to also run with interrupts disabled.
        let should_loop = task::scheduler::with(|sch| {
            let cur = sch.current;
            // Check if we already got a message queued in replies (from
            // the double-check above).
            let has_reply = with_state(|s| s.replies.remove(&task::current_id()).flatten().is_some());
            if has_reply {
                return false; // don't block, return on next iteration
            }
            sch.tasks[cur].state = task::State::Blocked;
            true
        });
        if should_loop {
            task::scheduler::schedule();
        }
        // Loop and re-check the queue.
    }
}

/// RPC: send `msg` to `ep` and block until the peer [`reply`]s, returning the
/// reply. This is the request/response pattern services expose to clients.
pub fn call(ep: EndpointId, mut msg: Message) -> Message {
    let me = task::current_id();
    msg.sender = me;

    // Register a reply slot and enqueue the request, waking a receiver.
    let woke = with_state(|s| {
        s.replies.insert(me, None);
        match s.endpoints.get_mut(&ep) {
            Some(endpoint) => {
                endpoint.queue.push_back(msg);
                endpoint.waiters.pop_front()
            }
            None => None,
        }
    });
    if let Some(w) = woke {
        task::unblock(w);
    }

    // Wait for the reply.
    loop {
        let reply = with_state(|s| match s.replies.get(&me) {
            Some(Some(_)) => s.replies.remove(&me).flatten(),
            _ => None,
        });
        if let Some(r) = reply {
            return r;
        }
        task::scheduler::with(|sch| {
            let cur = sch.current;
            sch.tasks[cur].state = task::State::Blocked;
        });
        task::scheduler::schedule();
    }
}

/// Remove a task's reply slot from the global replies map.
///
/// Called from [`crate::task::do_exit`] so that every exited task's
/// pending-reply slot is cleaned up, preventing a slow memory leak
/// in the `replies` map.
pub fn cleanup_reply_slot(task_id: u64) {
    with_state(|s| {
        s.replies.remove(&task_id);
    });
}

/// Reply to a message previously received via [`call`]. Delivers `reply_msg` to
/// the original caller and wakes it.
pub fn reply(original: &Message, mut reply_msg: Message) {
    reply_msg.sender = task::current_id();
    let sender = original.sender;
    let should_wake = with_state(|s| {
        if s.replies.contains_key(&sender) {
            s.replies.insert(sender, Some(reply_msg));
            true
        } else {
            // The sender used `send` (fire-and-forget), not `call`: nothing to
            // reply to. Drop silently.
            false
        }
    });
    if should_wake {
        task::unblock(sender);
    }
}
