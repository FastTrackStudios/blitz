//! A pressed pointer that leaves the window keeps its target (studio#141).
//!
//! Drag a control past the edge of a plugin window and the moves must keep
//! going to the element under the pointer when it left, at their true
//! coordinates. Hit-testing a point outside the viewport finds nothing, so
//! these moves used to fall back to the root and the drag was lost (or, with
//! the embedder clamping the coordinates to keep it alive, froze at the
//! edge).

use blitz_dom::{Document, DocumentConfig};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::{
    events::{
        BlitzPointerEvent, BlitzPointerId, MouseEventButton, MouseEventButtons, Point,
        PointerCoords, PointerDetails, UiEvent,
    },
    shell::{ColorScheme, Viewport},
};
use std::sync::Arc;

fn doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(200, 200, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

fn pointer(x: f32, y: f32, held: bool) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: PointerCoords {
            page_x: x,
            page_y: y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        },
        button: MouseEventButton::Main,
        buttons: if held {
            MouseEventButtons::from(MouseEventButton::Main)
        } else {
            MouseEventButtons::None
        },
        mods: Default::default(),
        details: PointerDetails::default(),
        element: Point::default(),
        active_pointers: Default::default(),
    }
}

const PAD: &str = r#"<html><body style="margin:0">
    <div id="pad" style="width:200px; height:200px;"></div>
</body></html>"#;

/// The pad, or the element the pointer is over inside it.
fn on_pad(doc: &HtmlDocument) -> bool {
    let pad = doc.query_selector("#pad").unwrap().unwrap();
    std::iter::successors(doc.get_hover_node_id(), |&id| {
        doc.get_node(id).and_then(|n| n.parent)
    })
    .any(|id| id == pad)
}

#[test]
fn a_held_pointer_past_the_window_edge_keeps_its_target() {
    let mut doc = doc(PAD);
    doc.handle_ui_event(UiEvent::PointerDown(pointer(100.0, 100.0, true)));
    assert!(on_pad(&doc), "pressed on the pad");

    for (x, y) in [(-60.0, 100.0), (100.0, -40.0), (380.0, 260.0)] {
        doc.handle_ui_event(UiEvent::PointerMove(pointer(x, y, true)));
        assert!(
            on_pad(&doc),
            "held at ({x}, {y}), outside the window: hover {:?}",
            doc.get_hover_node_id()
        );
    }
    // The release outside ends the drag where it is, on the pad.
    doc.handle_ui_event(UiEvent::PointerUp(pointer(380.0, 260.0, false)));
    assert!(on_pad(&doc), "released outside the window");
}

#[test]
fn a_pointer_that_is_not_held_leaves_with_the_window() {
    let mut doc = doc(PAD);
    doc.handle_ui_event(UiEvent::PointerMove(pointer(100.0, 100.0, false)));
    assert!(on_pad(&doc), "hovering the pad");
    doc.handle_ui_event(UiEvent::PointerMove(pointer(-60.0, 100.0, false)));
    assert!(!on_pad(&doc), "not held: outside the window is over nothing");
}
