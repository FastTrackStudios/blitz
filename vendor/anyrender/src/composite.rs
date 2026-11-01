//! FTS: layered compositing between a document painter and its window
//! renderer.
//!
//! A document whose widgets render into their own textures (a frame face,
//! a video) doesn't need the whole page painted and rasterised again when
//! only a widget changed. The painter leaves such a widget out of the page
//! scene and lists it as a [`Layer`] instead; a renderer that supports this
//! keeps the page it rasterised (the *base*) and, each frame, puts the
//! base on screen and draws the layers over it.
//!
//! A frame, from the painter's side:
//! 1. [`begin`] — whether the base from last time may be reused, and the
//!    layers to draw over it if it is;
//! 2. `WindowRenderer::render(draw)` — the renderer runs `draw` only when
//!    it can't reuse its base; a `draw` that paints lists that frame's
//!    layers with [`set_layers`];
//! 3. [`reused`] — whether it did.
//!
//! A renderer that doesn't composite never calls [`set_supported`], and a
//! painter seeing [`supported`] false paints widgets into the page as
//! before. One UI thread, one window renderer at a time: the state is
//! thread-local.

use std::cell::{Cell, RefCell};

use kurbo::Rect;

use crate::ResourceId;

/// Something drawn over the base, in device pixels, in paint order.
#[derive(Clone)]
pub enum Layer {
    /// A registered texture (a widget's picture).
    Texture {
        /// A resource the renderer registered (a texture).
        resource: ResourceId,
        /// Where the texture goes: its whole extent maps onto this
        /// rectangle.
        rect: Rect,
        /// What of it shows (the ancestors' clips, and the viewport).
        clip: Rect,
        /// Its ancestors' opacities, multiplied: the layer is drawn this
        /// transparent. (A group's opacity applied to the layer alone:
        /// exact while nothing else in the group shows through it.)
        opacity: f32,
    },
    /// Page content that paints over a texture layer, lifted out of the
    /// base so it stays on top: rasterised when the page is painted
    /// (`scene` is `Some`), reused as it was when the page is reused.
    Overlay {
        /// Which overlay: the renderer keeps each one's texture by it.
        id: u32,
        /// Where it goes (its content's bounds).
        rect: Rect,
        /// The content, in window device pixels, when the page was painted.
        scene: Option<std::rc::Rc<crate::Scene>>,
    },
}

impl Layer {
    /// The same layer for a frame that reuses the page: overlays as they
    /// were rasterised.
    #[must_use]
    pub fn reused(&self) -> Self {
        match self {
            Self::Overlay { id, rect, .. } => Self::Overlay { id: *id, rect: *rect, scene: None },
            other => other.clone(),
        }
    }
}

thread_local! {
    static SUPPORTED: Cell<bool> = const { Cell::new(false) };
    static REUSE: Cell<bool> = const { Cell::new(false) };
    static REUSED: Cell<bool> = const { Cell::new(false) };
    static LAYERS: RefCell<Vec<Layer>> = const { RefCell::new(Vec::new()) };
}

/// Renderer: this thread's window renderer composites layers.
pub fn set_supported(supported: bool) {
    SUPPORTED.with(|s| s.set(supported));
}

/// Painter: whether the renderer composites, so widgets may be layers.
#[must_use]
pub fn supported() -> bool {
    SUPPORTED.with(Cell::get)
}

/// Painter, before `render`: may the renderer reuse its base, and the
/// layers to draw over it (last painted frame's, for a reuse).
pub fn begin(reuse_base: bool, layers: Vec<Layer>) {
    REUSE.with(|r| r.set(reuse_base));
    REUSED.with(|r| r.set(false));
    LAYERS.with(|l| *l.borrow_mut() = layers);
}

/// Painter, while painting: this frame's layers, in paint order.
pub fn set_layers(layers: Vec<Layer>) {
    LAYERS.with(|l| *l.borrow_mut() = layers);
}

/// Renderer: whether the painter allows reusing the base.
#[must_use]
pub fn reuse_requested() -> bool {
    REUSE.with(Cell::get)
}

/// Renderer: the layers to draw this frame.
#[must_use]
pub fn layers() -> Vec<Layer> {
    LAYERS.with(|l| l.borrow().clone())
}

/// Renderer: whether this frame reused the base (the painter's `draw` was
/// not run).
pub fn set_reused(reused: bool) {
    REUSED.with(|r| r.set(reused));
}

/// Painter, after `render`: whether the base was reused.
#[must_use]
pub fn reused() -> bool {
    REUSED.with(Cell::get)
}
