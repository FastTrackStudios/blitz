//! Paint a [`blitz_dom::BaseDocument`] by pushing [`anyrender`] drawing commands into
//! an impl [`anyrender::PaintScene`].

#![allow(clippy::collapsible_if)]

#[cfg(feature = "custom-widget")]
mod composite;
mod color;
mod debug_overlay;
mod filters;
mod gradient;
mod kurbo_css;
mod layers;
mod render;
mod sizing;
mod text;

use std::cell::RefCell;
use std::collections::HashMap;

use anyrender::{PaintScene, Scene};
use blitz_dom::{BaseDocument, util::Color};
use render::BlitzDomPainter;

const FONT_EMBOLDEN_ENABLED: bool = cfg!(any(
    feature = "font-embolden",
    all(feature = "apple-font-embolden", target_os = "macos"),
    all(feature = "apple-font-embolden", target_os = "ios"),
));

/// The default color for text selection highlights
const SELECTION_COLOR: Color = Color::from_rgb8(180, 213, 255);

/// The scene each custom widget painted this frame.
///
/// FTS: behind a `RefCell` so the walk can TAKE a widget's scene rather
/// than clone it. A deep copy of every path a widget drew, every frame,
/// was the single largest cost in painting an arrangement — and it buys
/// nothing, because the map is thrown away at the end of the frame and
/// no node is painted twice.
type CustomWidgetSceneMap = RefCell<HashMap<(usize, usize), Scene>>;

/// Paint a [`blitz_dom::BaseDocument`] by pushing drawing commands into
/// an impl [`anyrender::PaintScene`].
///
/// This function assumes that the styles and layout in the [`BaseDocument`] are already
/// resolved. Please ensure that this is the case before trying to paint.
///
/// The implementation of [`PaintScene`] is responsible for handling the commands that are pushed into it.
/// Generally this will involve executing them to draw a rasterized image/texture. But in some cases it may choose to
/// transform them to a vector format (e.g. SVG/PDF) or serialize them in raw form for later use.
pub fn paint_scene(
    scene: &mut impl PaintScene,
    doc: &mut BaseDocument,
    scale: f64,
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
) {
    // Run `.paint()` on every custom widget in the document (and all subdocuments) ahead of time.
    // This helps us avoid borrow-checker issues as we recurse down the tree (`.paint()` require `&mut self`).
    //
    // TODO: Take widget and sub-document visibility into account
    #[allow(unused_mut)]
    let custom_widget_scenes: CustomWidgetSceneMap = RefCell::new(HashMap::new());
    #[cfg(feature = "custom-widget")]
    let textures = {
        let mut textures = HashMap::new();
        build_custom_widget_scenes(&mut custom_widget_scenes.borrow_mut(), &mut textures, doc, scene, scale);
        textures
    };

    // FTS: texture widgets become layers when the renderer composites.
    #[cfg(feature = "custom-widget")]
    let compositing = anyrender::composite::supported()
        .then(|| composite::Compositing::new(doc, textures));

    #[allow(unused_mut)]
    let mut generator = BlitzDomPainter::new(
        doc,
        scale,
        width,
        height,
        x_offset as f64,
        y_offset as f64,
        &custom_widget_scenes,
    );
    #[cfg(feature = "custom-widget")]
    {
        generator.compositing = compositing.as_ref();
    }
    generator.paint_scene(scene);
    #[cfg(feature = "custom-widget")]
    if let Some(compositing) = compositing {
        compositing.finish(doc);
    }

    // println!(
    //     "Rendered using {} clips (depth: {}) (wanted: {})",
    //     CLIPS_USED.load(atomic::Ordering::SeqCst),
    //     CLIP_DEPTH_USED.load(atomic::Ordering::SeqCst),
    //     CLIPS_WANTED.load(atomic::Ordering::SeqCst)
    // );
}

#[cfg(feature = "custom-widget")]
fn build_custom_widget_scenes(
    custom_widget_scenes: &mut HashMap<(usize, usize), Scene>,
    textures: &mut HashMap<usize, anyrender::ResourceId>,
    doc: &mut BaseDocument,
    render_ctx: &mut impl anyrender::RenderContext,
    scale: f64,
) {
    let doc_id = doc.id();

    // Process scenes for every custom widget in the document
    let custom_widget_node_ids = doc.custom_widget_node_ids();
    for node_id in custom_widget_node_ids.into_iter() {
        if let Some((scene, texture)) = process_custom_widget_node(doc, render_ctx, node_id, scale) {
            custom_widget_scenes.insert((doc_id, node_id), scene);
            // FTS: only the root document's widgets are layers.
            if let Some(texture) = texture {
                textures.insert(node_id, texture);
            }
        }
    }

    // Recurse into sub documents
    let sub_document_node_ids = doc.sub_document_node_ids();
    for node_id in sub_document_node_ids.into_iter() {
        if let Some(sub_doc) = doc.get_node_mut(node_id).and_then(|node| node.subdoc_mut()) {
            let mut inner = sub_doc.inner_mut();
            build_custom_widget_scenes(custom_widget_scenes, &mut HashMap::new(), &mut inner, render_ctx, scale);
        }
    }
}

/// FTS: paint only the widgets drawn as layers (updating their textures),
/// for a frame that reuses the page: nothing else changed. False when a
/// layer can't be drawn as it was (its texture or size changed): paint the
/// page instead.
#[cfg(feature = "custom-widget")]
pub fn paint_composited_widgets(
    doc: &mut BaseDocument,
    render_ctx: &mut impl anyrender::RenderContext,
    scale: f64,
) -> bool {
    let placed: Vec<(usize, anyrender::ResourceId)> =
        doc.composite.placed.borrow().iter().map(|(n, r)| (*n, *r)).collect();
    let mut ok = true;
    for (node_id, texture) in placed {
        // Unchanged: its texture already shows it.
        let wants = doc
            .get_node(node_id)
            .and_then(|n| n.element_data())
            .and_then(|el| el.custom_widget_data())
            .is_some_and(|w| w.widget.needs_redraw());
        if !wants {
            continue;
        }
        match process_custom_widget_node(doc, render_ctx, node_id, scale) {
        }
    }
    ok
}

#[cfg(feature = "custom-widget")]
fn process_custom_widget_node(
    doc: &mut BaseDocument,
    render_ctx: &mut impl anyrender::RenderContext,
    node_id: usize,
    scale: f64,
) -> Option<(Scene, Option<anyrender::ResourceId>)> {
    use blitz_dom::node::{CustomWidgetStatus, ProxyRenderContext};

    let node = doc.get_node_mut(node_id)?;
    let width = (node.final_layout.size.width as f64 * scale) as u32;
    let height = (node.final_layout.size.height as f64 * scale) as u32;

    if width == 0 || height == 0 {
        return None;
    }

    let style = node.stylo_element_data.primary_styles()?;
    let element = node.data.downcast_element_mut()?;
    let widget_data = element.custom_widget_data_mut()?;

    let mut render_ctx = ProxyRenderContext {
        inner: render_ctx,
        resource_ids: &mut widget_data.active_resource_ids,
    };

    if widget_data.status == CustomWidgetStatus::Suspended {
        widget_data.widget.can_create_surfaces(&mut render_ctx);
        widget_data.status = CustomWidgetStatus::Active;
    }

    let widget_scene = widget_data
        .widget
        .paint(&mut render_ctx, &style, width, height, scale);

    Some((widget_scene, widget_data.widget.composite_texture()))
}
