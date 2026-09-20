//! Types and traits to enable interoperability between the other Blitz crates without
//! circular or unnecessary dependencies.

pub mod devtools;
pub mod events;
pub mod navigation;
pub mod net;
pub mod shell;

pub use smol_str::SmolStr;

/// FTS: how long the last frame took to resolve, paint and present, in
/// microseconds.
///
/// Instrumentation rather than API. An application can time the gaps
/// BETWEEN frames on its own, but that measures how often it was asked
/// to draw — which during a burst of discrete input is the input's
/// cadence and not the frame's cost. The two look identical from
/// outside the shell and mean opposite things: one says the renderer is
/// slow, the other says nothing was asked of it.
pub static LAST_FRAME_MICROS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
