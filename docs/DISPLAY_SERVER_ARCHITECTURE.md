# PICKLE OS Display Server Architecture (Phase: GUI foundation)

> Status: **foundation / skeleton implemented.** This document describes the
> target design for moving the GUI off the kernel-resident compositor and onto a
> proper client–server windowing system, and describes the concrete pieces that
> already exist in the tree (`kernel/src/wm.rs`, the `SYS_WIN_*` syscalls, the
> `displayd` compositor task, and the `libpickleos::gui` client library).

---

## 1. Motivation

Today the GUI is *kernel resident*: `gui::compositor_task` runs inside the
kernel, owns the framebuffer directly, and hard-codes the windows it draws
(`Welcome`, `System`, and the live `Terminal`s). This was a fine bootstrap, but
it violates the microkernel principle that everything which is not scheduling,
memory, IPC or capability enforcement should live outside the trusted core.

The audit (`docs/AUDIT_AND_ROADMAP.md`) calls out the need to evolve this into a
real windowing system in which **applications are separate processes** that ask
a **display server** to create windows on their behalf, render into a
**shared pixel buffer**, and receive **input events** through a well-defined
protocol.

This phase lays the *architecture foundation*: the server-side window registry,
the client/server protocol, the shared-buffer presentation model, the
window-management syscalls, and a minimal client library. The full widget
toolkit, font/theming engine, hardware-accelerated compositing, and a fully
ring-3 `displayd` come in later phases per the roadmap.

---

## 2. Display server model

```
        +-------------------+        +-------------------+
        |  client app  (A)  |        |  client app  (B)  |   ring 3 (eventually)
        |  libpickleos::gui |        |  libpickleos::gui |
        +---------+---------+        +---------+---------+
                  |  SYS_WIN_* (int 0x80)       |
                  v                             v
        +-------------------------------------------------+
        |   kernel window-server core  (kernel/src/wm.rs) |  trusted core
        |   - window registry (id -> ServerWindow)        |
        |   - per-window shared pixel buffer (backing)    |
        |   - per-window input event queue                |
        |   - new/destroy notification queues             |
        +-------------------------+-----------------------+
                                  ^   (read windows, push events)
                                  |
        +-------------------------+-----------------------+
        |   displayd / compositor  (gui::compositor_task) |
        |   - owns the linear framebuffer                 |
        |   - composites desktop + windows + cursor       |
        |   - hit-tests pointer, manages focus/stacking   |
        |   - delivers mouse/keyboard events to windows   |
        +-------------------------------------------------+
                                  |
                                  v
                       linear framebuffer (HW)
```

### 2.1 Roles

* **Window-server core (`wm.rs`)** — the part that *must* be trusted because it
  hands out memory and mediates between mutually-distrustful clients. It owns
  the canonical list of windows, each window's backing pixel buffer, and each
  window's event queue. It is deliberately mechanism-only: it has no policy
  about *where* windows go or *which* one is focused.

* **Compositor / `displayd` (`gui::compositor_task`)** — the policy half. It
  owns the screen, decides stacking order, focus, and window placement,
  composites the scene, and translates raw pointer/keyboard input into
  per-window events. Today it runs as a kernel task that calls directly into
  `wm.rs`; the protocol is structured so it can be lifted into its own ring-3
  process later (it would then talk to `wm.rs` purely through IPC + a mapped
  framebuffer capability).

* **Client applications** — ordinary processes that link `libpickleos::gui` and
  use the `SYS_WIN_*` syscalls. A client never touches the framebuffer; it only
  draws into its own window's pixel buffer and presents it with a commit.

### 2.2 Why split *core* from *compositor*

Keeping the registry/allocation (`wm.rs`) separate from the
placement/compositing policy (`displayd`) means:

* the security-critical buffer ownership and event routing live in one small,
  auditable place;
* the compositor can be replaced or moved to ring 3 without changing the client
  ABI;
* multiple compositors / a test harness can drive the same registry.

---

## 3. Client–server protocol

The protocol is defined once, in `wm::op`, and is reachable two ways that share
the exact same opcodes and semantics:

1. **Syscall transport (implemented now):** thin `SYS_WIN_*` syscalls that map
   1:1 onto the core operations. This is the fast path while `displayd` is in
   the kernel.
2. **IPC transport (future):** the same opcodes carried as `ipc::Message` tags
   to a named endpoint (`"displayd"`), for when the server is a ring-3 process.
   The opcode numbering is reserved now so the move is source-compatible.

### 3.1 Operations (client → server)

| Op            | Syscall            | Arguments                              | Returns                |
|---------------|--------------------|----------------------------------------|------------------------|
| `CREATE_WINDOW` | `SYS_WIN_CREATE` | `w`, `h`, `title_ptr`, `title_len`     | `window_id` or `-1`    |
| `COMMIT`        | `SYS_WIN_COMMIT` | `window_id`, `buf_ptr`, `byte_len`     | `0` or `-1`            |
| `POLL_EVENT`    | `SYS_WIN_POLL`   | `window_id`, `event_ptr` (16 bytes)    | `1` got / `0` none / `-1` |
| `DESTROY_WINDOW`| `SYS_WIN_DESTROY`| `window_id`                            | `0` or `-1`            |
| `WINDOW_INFO`   | `SYS_WIN_INFO`   | `window_id`, `info_ptr` (16 bytes)     | `0` or `-1`            |

* **CREATE_WINDOW** allocates a `ServerWindow`, a zeroed `w*h` pixel buffer
  (sizes are clamped to `wm::MAX_W` × `wm::MAX_H`), records the caller as the
  *owner*, and enqueues the id on the *new-window* queue the compositor drains.
* **COMMIT** copies `byte_len` bytes of `0x00RRGGBB` pixels from the client's
  buffer into the window's server-side backing store and bumps a global dirty
  counter so the compositor repaints. (This is the "present" / "swap" call.)
* **POLL_EVENT** dequeues one input event into a 16-byte user struct
  (non-blocking; returns 0 when the queue is empty).
* **DESTROY_WINDOW** frees the backing buffer and enqueues the id on the
  *destroyed-window* queue.
* **WINDOW_INFO** reads back current geometry (the compositor may have moved the
  window via a title-bar drag).

All operations are **owner-checked** for ring-3 callers: a task may only operate
on windows it created.

### 3.2 Events (server → client)

Events are delivered through the per-window queue and read with POLL_EVENT. The
wire form is a fixed 16-byte record (little-endian):

```
struct WmEvent {     // offset
    u32 kind;        // 0   one of wm::op::EV_*
    i32 x;           // 4   window-local X (pixels), valid for mouse events
    i32 y;           // 8   window-local Y (pixels)
    u32 arg;         // 12  button index (mouse) or Unicode codepoint (key)
}
```

| Event           | `kind`         | Meaning                                    |
|-----------------|----------------|--------------------------------------------|
| `EV_MOUSE_MOVE` | `0x10`         | pointer moved over the window (x,y local)  |
| `EV_MOUSE_DOWN` | `0x11`         | button pressed (`arg` = button: 0=L,1=R,2=M) |
| `EV_MOUSE_UP`   | `0x12`         | button released                            |
| `EV_KEY`        | `0x13`         | key typed (`arg` = Unicode codepoint)      |
| `EV_FOCUS`      | `0x15`         | window gained keyboard focus               |
| `EV_BLUR`       | `0x16`         | window lost keyboard focus                 |
| `EV_CLOSE`      | `0x14`         | the WM requests the window close           |

The queue is bounded (`wm::EVENT_QUEUE_CAP`); when full, the oldest event is
dropped so a slow client cannot make the compositor block or grow memory without
bound.

---

## 4. Shared-memory framebuffer approach

A real display server avoids copying whole frames by **sharing** the window's
pixel buffer between client and server. PICKLE OS models this in three layers so
the client ABI is stable as the implementation matures:

1. **Now (single address space for the core):** the window's backing buffer is a
   kernel-heap `Vec<u32>` owned by `wm.rs`. The client builds its frame in its
   own memory and hands it to the server with COMMIT, which copies it into the
   backing store. The compositor then blits the backing store to the
   framebuffer. One copy in, one blit out — simple and safe, and bounded because
   windows are size-capped (the kernel heap is only ~1 MiB).

2. **Next (true shared pages):** the backing buffer becomes a frame-backed
   region the server maps **into the client's address space** (a `Memory`
   capability, see `capability::Object::Memory`). The client draws directly into
   the shared pages and COMMIT degenerates to a *damage report* (`x,y,w,h`) — no
   pixel copy. This is the standard "wl_shm"/DRM-dumb-buffer pattern.

3. **Later (zero-copy scanout / double buffering):** per-window front/back
   buffers with an atomic flip, and optionally GPU-side composition.

The COMMIT syscall is intentionally specified as "present the current
buffer/damage" rather than "copy these bytes" so that moving from layer 1 to
layer 2 does not change the client-visible contract.

### 4.1 Capability story

Framebuffer and window memory are governed by the existing capability system
(`kernel/src/capability.rs`):

* The compositor holds the framebuffer authority (today implicit as a kernel
  task; later an explicit `Object::Mmio`/`Object::Memory` capability minted to
  the `displayd` process).
* Each shared window buffer (layer 2+) is an `Object::Memory` capability minted
  to the owning client with `READ | WRITE` but **not** `GRANT`, so a client
  cannot leak another client's window memory.
* Window ids are unforgeable handles validated against the caller's ownership on
  every syscall, mirroring the capability model for kernel objects.

---

## 5. Component map (what is in the tree)

| Concern                         | File                                   |
|---------------------------------|----------------------------------------|
| Window-server core / registry   | `kernel/src/wm.rs`                     |
| Protocol opcodes + event format  | `kernel/src/wm.rs` (`wm::op`)          |
| Window-management syscalls       | `kernel/src/syscall.rs` (`SYS_WIN_*`)  |
| Compositor / `displayd`          | `kernel/src/gui.rs`                    |
| Client library (Rust)            | `userspace/libpickleos/src/gui.rs`     |
| Client syscall wrappers          | `userspace/libpickleos/src/syscall.rs` |
| In-kernel demo client            | `gui::client_demo_task`                |
| Headless self-test               | `wm::wm_selftest`                      |

---

## 6. Lifecycle of a window (end to end)

1. Client calls `gui::Window::create(w, h, "Title")` → `SYS_WIN_CREATE` →
   `wm::create_window`. The core allocates the backing buffer, records the
   owner, and pushes the id onto the new-window queue.
2. The compositor, each frame, drains the new-window queue and adds a managed
   frame (title bar, border, close button) around the client area, places it,
   focuses it, and sends `EV_FOCUS`.
3. Client renders into its local buffer and calls `Window::commit(&pixels)` →
   `SYS_WIN_COMMIT` → `wm::commit`, which copies the pixels and bumps the dirty
   counter.
4. The compositor sees the dirty counter change and repaints, blitting the
   window's backing store into its on-screen client area.
5. The user moves/clicks/types over the window; the compositor converts this to
   `WmEvent`s in window-local coordinates and enqueues them via
   `wm::push_event`. The client drains them with `Window::poll_event`.
6. On close (client calls `Window::destroy`, or the user clicks `[x]` and the
   compositor calls `wm::destroy_window`), the backing buffer is freed and the
   id is pushed on the destroyed-window queue; the compositor removes the frame.

---

## 7. Non-goals for this phase

* No widget toolkit (buttons, text boxes, layout) — clients get a raw pixel
  buffer and raw events.
* No font/text API in the server — clients rasterize their own text (the
  in-kernel 8×8 font is reused by the demo for convenience only).
* `displayd` still runs in the kernel; the IPC transport is reserved but the
  syscall transport is the active one.
* No true shared pages yet (layer 1 copy-on-commit); the ABI is chosen so this
  is an internal change later.

These are explicitly deferred so this phase stays a small, reviewable
foundation.
