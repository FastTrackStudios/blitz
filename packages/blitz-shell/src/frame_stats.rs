//! FTS: how the window's frames are going, readable from any thread.
//!
//! The frame log (`FTS_FPS_LOG`) prints to stderr, which a shipped phone
//! app never shows. These counters are published every second instead, and
//! one more is live: when the frame under way began. A thread that is not
//! the main one (a watchdog, the app's telemetry) reads them — so a screen
//! frozen inside one long frame still says how long it has been stuck.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

static FPS: AtomicU32 = AtomicU32::new(0);
static MEAN_US: AtomicU32 = AtomicU32::new(0);
static WORST_US: AtomicU32 = AtomicU32::new(0);
static LAYERS: AtomicU32 = AtomicU32::new(0);
/// When the counters were last published (ms since [`epoch`]): older than
/// a second and a half, the window has drawn nothing since — idle.
static PUBLISHED_AT: AtomicU64 = AtomicU64::new(0);
/// The slowest document update (the components rendering: a page built,
/// its faces opened) this second and the last, µs.
static POLL_WORST: AtomicU32 = AtomicU32::new(0);
static POLL_WORST_LAST: AtomicU32 = AtomicU32::new(0);
/// When the frame under way began (ms since [`epoch`], +1 so 0 = no frame).
static IN_FRAME_SINCE: AtomicU64 = AtomicU64::new(0);

/// Why a frame asked for the next one, counted this second and published
/// as the last second's: the widget layer set still changing, a document
/// animation, a widget still moving.
static WHY: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static WHY_LAST: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

/// Why the next frame was asked for.
#[derive(Clone, Copy)]
pub(crate) enum Why {
    /// The last paint changed which widgets are layers.
    Unsettled = 0,
    /// The document animates (a CSS animation or transition, a scroll).
    Animating = 1,
    /// A widget is still moving (a spring, a lamp).
    Widgets = 2,
}

pub(crate) fn asked_again(why: Why) {
    WHY[why as usize].fetch_add(1, Ordering::Relaxed);
}

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// The last second's frames, and the frame under way.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Frames drawn in the last full second.
    pub fps: u32,
    /// Their mean and worst cost, ms.
    pub mean_ms: f32,
    pub worst_ms: f32,
    /// Widget layers the last frame drew.
    pub layers: u32,
    /// How long the main thread has been inside the frame or document
    /// update under way, ms (0: neither under way).
    pub in_frame_ms: u64,
    /// The slowest document update of the last second, ms — a page that
    /// takes seconds to build shows here, not in the frame times.
    pub update_worst_ms: f32,
    /// Of the last second's frames, how many asked for the next because the
    /// layer set was still changing, the document animated, a widget moved.
    /// A screen that never goes still says which of these keeps it awake.
    pub again_unsettled: u32,
    pub again_animating: u32,
    pub again_widgets: u32,
}

/// What the counters say now.
#[must_use]
pub fn snapshot() -> FrameStats {
    let since = IN_FRAME_SINCE.load(Ordering::Relaxed);
    let now = epoch().elapsed().as_millis() as u64 + 1;
    // Nothing drawn for a while: the last second's numbers are history.
    let idle = now.saturating_sub(PUBLISHED_AT.load(Ordering::Relaxed)) > 1500;
    if idle {
        // Still say how slow an update was, if one ran since.
        let poll = POLL_WORST.swap(0, Ordering::Relaxed);
        return FrameStats {
            in_frame_ms: if since == 0 { 0 } else { now.saturating_sub(since) },
            update_worst_ms: poll as f32 / 1000.0,
            ..FrameStats::default()
        };
    }
    FrameStats {
        update_worst_ms: POLL_WORST_LAST.load(Ordering::Relaxed) as f32 / 1000.0,
        fps: FPS.load(Ordering::Relaxed),
        mean_ms: MEAN_US.load(Ordering::Relaxed) as f32 / 1000.0,
        worst_ms: WORST_US.load(Ordering::Relaxed) as f32 / 1000.0,
        layers: LAYERS.load(Ordering::Relaxed),
        in_frame_ms: if since == 0 { 0 } else { now.saturating_sub(since) },
        again_unsettled: WHY_LAST[0].load(Ordering::Relaxed),
        again_animating: WHY_LAST[1].load(Ordering::Relaxed),
        again_widgets: WHY_LAST[2].load(Ordering::Relaxed),
    }
}

/// A document update (components rendering) took `us`.
pub(crate) fn update_took(us: u64) {
    POLL_WORST.fetch_max(u32::try_from(us).unwrap_or(u32::MAX), Ordering::Relaxed);
}

pub(crate) fn frame_began() {
    IN_FRAME_SINCE.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
}

pub(crate) fn frame_ended() {
    IN_FRAME_SINCE.store(0, Ordering::Relaxed);
}

pub(crate) fn publish(fps: u32, mean_us: u32, worst_us: u32, layers: u32) {
    FPS.store(fps, Ordering::Relaxed);
    MEAN_US.store(mean_us, Ordering::Relaxed);
    WORST_US.store(worst_us, Ordering::Relaxed);
    LAYERS.store(layers, Ordering::Relaxed);
    PUBLISHED_AT.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    POLL_WORST_LAST.store(POLL_WORST.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    for (now, last) in WHY.iter().zip(WHY_LAST.iter()) {
        last.store(now.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    }
}
