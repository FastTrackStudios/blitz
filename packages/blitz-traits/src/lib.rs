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

/// FTS: the same frame, split at the point the scene stops being built
/// and starts being handed to the GPU — how long encoding the scene
/// took, in microseconds.
///
/// [`LAST_FRAME_MICROS`] alone cannot tell a window that draws too much
/// from a window that draws little and waits on the compositor to take
/// it; the two are the same number and want opposite fixes. Stored
/// rather than printed so the application can put them on screen beside
/// the frame time.
pub static LAST_ENCODE_MICROS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// FTS: and the other half — acquiring the surface, submitting and
/// presenting, in microseconds. See [`LAST_ENCODE_MICROS`].
pub static LAST_PRESENT_MICROS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
