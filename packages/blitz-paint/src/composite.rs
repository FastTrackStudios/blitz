//! FTS: custom widgets drawn as composited layers (see
//! `anyrender::composite` and `blitz_dom::CompositeState`).
//!
//! While painting, a widget whose picture is one texture
//! ([`Widget::composite_texture`](blitz_dom::Widget::composite_texture)) is
//! left out of the page and listed as a layer, unless something above it
//! changes how it looks beyond an opacity (which the layer takes): a
//! transform other than a translation, a filter, a clip-path or a mask.
//!
//! Page content painted after a layer and over it (a badge on a
//! visualiser) is *lifted*: recorded into an overlay drawn above the
//! layer, not into the page, so it stays on top. Content painted over an
//! overlay is lifted too, keeping the page's order. A layer the page was
//! painted into (the first paint, before the set was known) becomes one on
//! the next paint: each paint records the widgets that can be layers.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use anyrender::composite::Layer;
use anyrender::{PaintScene, ResourceId};
use blitz_dom::{BaseDocument, Node};
use kurbo::{Affine, Rect};
use style::properties::ComputedValues;

/// This paint's compositing: what may be a layer, and what was found.
#[derive(Default)]
pub(crate) struct Compositing {
    /// Widgets whose picture is a texture: node → texture.
    textures: HashMap<usize, ResourceId>,
    /// Widgets to draw as layers this paint (found able last paint).
    compose: HashSet<usize>,
    /// The layers so far, in paint order.
    layers: RefCell<Vec<Layer>>,
    /// What shows of each texture layer and overlay so far: content that
    /// paints over any of it is lifted.
    above: RefCell<Vec<Rect>>,
    /// The overlay being recorded (content lifted since the last layer),
    /// and its bounds.
    overlay: RefCell<Option<(anyrender::Scene, Rect)>>,
    /// Painting lifted content into the overlay (no layers, no lifting).
    lifting: Cell<bool>,
    /// Texture widgets under a transform or an effect.
    blocked: RefCell<HashSet<usize>>,
    /// Widgets drawn as layers, and their textures.
    placed: RefCell<HashMap<usize, ResourceId>>,
    /// Texture widgets drawn at all (not culled).
    visible: RefCell<HashSet<usize>>,
}

impl Compositing {
    /// For the root document of a renderer that composites; `textures` from
    /// this frame's widget paints.
    pub(crate) fn new(doc: &BaseDocument, textures: HashMap<usize, ResourceId>) -> Self {
        let compose = doc.composite.widgets.borrow().clone();
        Self { textures, compose, ..Self::default() }
    }

    /// A widget being drawn at `transform` (its content origin included),
    /// `size` device pixels, showing through `clip`: whether it is a layer
    /// (and so not drawn into the page).
    pub(crate) fn place(&self, node: &Node, transform: Affine, size: (f64, f64), clip: Rect) -> bool {
        let node_id = node.id;
        if self.lifting.get() {
            self.visible.borrow_mut().insert(node_id);
            return false;
        }
        let Some(&resource) = self.textures.get(&node_id) else { return false };
        self.visible.borrow_mut().insert(node_id);
        let [a, b, c, d, _, _] = transform.as_coeffs();
        let translation_only = (a - 1.0).abs() < 1e-9 && b.abs() < 1e-9 && c.abs() < 1e-9 && (d - 1.0).abs() < 1e-9;
        if !translation_only || under_effect(node) {
            self.blocked.borrow_mut().insert(node_id);
            return false;
        }
        if !self.compose.contains(&node_id) {
            return false;
        }
        let rect = transform.transform_rect_bbox(Rect::new(0.0, 0.0, size.0, size.1));
        self.close_overlay();
        self.above.borrow_mut().push(rect.intersect(clip));
        self.layers.borrow_mut().push(Layer::Texture { resource, rect, clip, opacity: opacity(node) });
        self.placed.borrow_mut().insert(node_id, resource);
        true
    }

    /// Whether an element about to paint over `bbox` (device pixels) must
    /// be lifted: it paints something, over a layer or an overlay.
    pub(crate) fn lifts(&self, node: &Node, styles: &ComputedValues, bbox: Rect) -> bool {
        if self.lifting.get() || self.layers.borrow().is_empty() {
            return false;
        }
        // A pixel's overlap at least: boxes that merely touch (fractional
        // edges) don't cover each other.
        let over = self.above.borrow().iter().any(|r| {
            let i = r.intersect(bbox);
            i.width() >= 1.0 && i.height() >= 1.0
        });
        over && paints_something(node, styles)
    }

    /// Paint lifted content (`paint`, which paints the element) into the
    /// overlay: clipped to `clip`, faded by its ancestors' opacity, its
    /// bounds `bbox`.
    pub(crate) fn lift(&self, node: &Node, clip: Rect, bbox: Rect, paint: impl FnOnce(&mut anyrender::Scene)) {
        let mut overlay = self.overlay.borrow_mut().take().unwrap_or_else(|| (anyrender::Scene::new(), Rect::ZERO));
        let alpha = node.parent.and_then(|p| node.tree().get(p)).map_or(1.0, opacity);
        overlay.0.push_layer(peniko::Mix::Normal, alpha, Affine::IDENTITY, &clip, None, None);
        self.lifting.set(true);
        paint(&mut overlay.0);
        self.lifting.set(false);
        overlay.0.pop_layer();
        let bbox = bbox.intersect(clip);
        overlay.1 = if overlay.1.area() > 0.0 { overlay.1.union(bbox) } else { bbox };
        self.above.borrow_mut().push(bbox);
        *self.overlay.borrow_mut() = Some(overlay);
    }

    /// End the overlay being recorded: it goes in the layers here.
    fn close_overlay(&self) {
        if let Some((scene, rect)) = self.overlay.borrow_mut().take()
            && rect.area() > 0.0
        {
            let mut layers = self.layers.borrow_mut();
            let id = layers.iter().filter(|l| matches!(l, Layer::Overlay { .. })).count() as u32;
            layers.push(Layer::Overlay { id, rect, scene: Some(Rc::new(scene)) });
        }
    }

    /// After the paint: record which widgets can be layers next time, and
    /// hand the layers to the renderer.
    pub(crate) fn finish(self, doc: &BaseDocument) {
        self.close_overlay();
        let blocked = self.blocked.into_inner();
        let free: HashSet<usize> = self.textures.keys().copied().filter(|id| !blocked.contains(id)).collect();
        let layers = self.layers.into_inner();
        doc.composite.unsettled.set(free != self.compose);
        *doc.composite.widgets.borrow_mut() = free;
        *doc.composite.layers.borrow_mut() = layers.iter().map(Layer::reused).collect();
        *doc.composite.placed.borrow_mut() = self.placed.into_inner();
        *doc.composite.visible.borrow_mut() = self.visible.into_inner();
        anyrender::composite::set_layers(layers);
    }
}

/// The opacities from `node` up, multiplied.
fn opacity(node: &Node) -> f32 {
    let mut alpha = 1.0;
    let mut at = Some(node);
    while let Some(n) = at {
        if let Some(styles) = n.primary_styles() {
            alpha *= styles.get_effects().opacity;
        }
        at = n.parent.and_then(|p| n.tree().get(p));
    }
    alpha
}

/// Whether anything from `node` up changes how its content looks beyond
/// placing it and fading it: a filter, a clip-path, a mask, or a
/// transform.
fn under_effect(node: &Node) -> bool {
    let mut at = Some(node);
    while let Some(n) = at {
        if let Some(styles) = n.primary_styles() {
            let effects = styles.get_effects();
            if !effects.filter.0.is_empty() || !effects.backdrop_filter.0.is_empty() {
                return true;
            }
            if !matches!(styles.get_svg().clip_path, style::values::generics::basic_shape::ClipPath::None) {
                return true;
            }
            if styles.get_svg().mask_image.0.iter().any(|i| !matches!(i, style::values::computed::Image::None)) {
                return true;
            }
        }
        if n.transform.is_some_and(|t| {
            let [a, b, c, d, _, _] = t.as_coeffs();
            (a - 1.0).abs() > 1e-9 || b.abs() > 1e-9 || c.abs() > 1e-9 || (d - 1.0).abs() > 1e-9
        }) {
            return true;
        }
        at = n.parent.and_then(|p| n.tree().get(p));
    }
    false
}

/// Whether painting `node` puts anything on screen: a background, a border,
/// a shadow, an outline, text, an image, a widget.
fn paints_something(node: &Node, styles: &ComputedValues) -> bool {
    if let Some(el) = node.element_data()
        && (el.inline_layout_data.is_some()
            || el.raster_image_data().is_some()
            || el.text_input_data().is_some()
            || el.sub_doc_data().is_some()
            || el.custom_widget_data().is_some())
    {
        return true;
    }
    #[cfg(feature = "svg")]
    if node.element_data().is_some_and(|el| el.svg_data().is_some()) {
        return true;
    }
    let current = styles.clone_color();
    let background = styles.get_background();
    if crate::color::ToColorColor::as_srgb_color(&background.background_color.resolve_to_absolute(&current))
        != crate::color::Color::TRANSPARENT
    {
        return true;
    }
    if background.background_image.0.iter().any(|i| !matches!(i, style::values::computed::Image::None)) {
        return true;
    }
    let border = styles.get_border();
    if [&border.border_top_width, &border.border_right_width, &border.border_bottom_width, &border.border_left_width]
        .iter()
        .any(|w| w.0 > style::values::computed::Au(0))
    {
        return true;
    }
    let effects = styles.get_effects();
    if !effects.box_shadow.0.is_empty() {
        return true;
    }
    let outline = styles.get_outline();
    !matches!(outline.outline_style, style::values::computed::OutlineStyle::BorderStyle(style::values::specified::BorderStyle::None))
        && outline.outline_width.0 > style::values::computed::Au(0)
}
