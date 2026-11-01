Source: crates.io `anyrender_vello` 0.15.0 (dioxuslabs/anyrender), the
published tarball with the registry's own bookkeeping files dropped —
re-vendored 2026-11-01 from 0.12.0 (blitz branch `fts-upstream-2026-11`,
architect#170) by a three-way merge of this copy's FTS changes onto the
0.15.0 source. Originally merged (2026-10-03) with the copy the
FastTrackStudios blitz fork carries (`vendor/anyrender_vello`): its
`compositor.rs` and `rasterizer.rs`, which composite texture widgets over
a kept page and rasterise a scene for one (`anyrender_vello::Rasterizer`,
which the processor's painted visualisers use). The blitz fork's copy and
this one are meant to stay identical.
License: MIT OR Apache-2.0 — theirs, not ours, and it stays that way.

Vendored for three changes to `window_renderer.rs`, all found by
measuring an arrangement window at 2560x1440 and all reversible from
the environment so the next machine can re-measure rather than trust
these numbers:

- **The GPU barrier is gone.** After `present`, upstream calls
  `poll(wait_indefinitely())`, which blocks the thread until the GPU has
  finished everything it was handed. That is a hard fence once a frame:
  the CPU cannot start building frame N+1 until frame N has been
  rasterised, so a frame costs CPU + GPU instead of max(CPU, GPU).
  `PollType::Poll` does the same housekeeping — map callbacks, freeing
  what the queue released — without the fence. `FTS_GPU_BARRIER=1` puts
  the wait back for a driver that needs it.

  **Re-measured 2026-09-20 and it now buys nothing.** A/B at 5120x1440
  on an idle box, 4400 frames an arm: 4.10 ms p50 with the barrier and
  4.10 ms without, and `wait` itself is 0.02–0.03 ms either way. By the
  time the poll happens the GPU has already finished — present is
  1.3 ms and the CPU spends 2.6 ms building the next frame, so there is
  no fence left to remove. The original measurement was taken at a frame
  latency of two, where `get_current_texture` blocked and the barrier
  compounded it; at one it does not. Kept because it costs nothing and a
  different driver may not agree, but do not expect it to show up in a
  number.

- **`desired_maximum_frame_latency` is 1, not 2.** (Upstream took this
  in 0.15.0: `VelloRendererOptions::desired_maximum_frame_latency`,
  default 1. The fork keeps only the `FTS_FRAME_LATENCY` override on top
  of the configured value.) Two lets the CPU run a
  frame ahead, which is right for something that can always fill the
  pipeline; here it meant `get_current_texture` blocked until the queue
  drained, on the critical path. Acquire-and-present went from 6.7 ms at
  two to 2.2 ms at one, with the GPU render itself 1.3 ms either way.
  Three was worse again. `FTS_FRAME_LATENCY` overrides it.

- **`present_mode` is switchable** via `FTS_PRESENT`
  (`immediate` / `mailbox`, default `AutoVsync`), so vsync can be ruled
  in or out by measurement. Under `AutoVsync` a slow frame and a frame
  that is merely WAITING are the same number from outside.

- **`FTS_DIRECT_SURFACE` is gone.** It rendered straight into the
  swapchain, dropping the intermediate texture and its blit; measured at
  5120x1440 it bought nothing (`present` 1.30 ms p50 either way — the
  cost is the swapchain acquire, not the copy). The fork's compositor
  draws the window's frame into that intermediate, so with it the
  direct path has nothing to render into, and it went.

Plus a one-shot surface-configuration line behind the crate's existing
opt-in `log_frame_times` feature, which is `println`-based upstream —
extended with the present path (always the intermediate now). `--features frame-times` on
`session-daw` turns it on.

Drop the vendor if upstream takes the barrier and latency changes. See
the root `[patch.crates-io]` entry and issue #119.

FTS (STA-299): `VelloRendererOptions::default()` reads the surface's clear
colour from `ANYRENDER_BASE_COLOR=#rrggbb` (upstream: white), and so does
`From<anyrender::RendererConfig>` when the config names no colour
(`VelloRendererOptions::new()` is `const` upstream and stays white). It is what
shows wherever the page paints nothing — on iOS, the safe-area strips
outside Blitz's viewport. Signal's iOS app sets its dark ground there.
