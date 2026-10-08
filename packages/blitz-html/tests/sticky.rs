//! `position: sticky` (top): a header stays at the top of its scrolling
//! list while its section scrolls under it, and leaves with its section.

use blitz_dom::{DocumentConfig, FontContext};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

fn layout_doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            font_ctx: Some(FontContext::new()),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

/// Two 400px sections in a 200px list, each with a 40px sticky header.
const HTML: &str = r#"<html><body style="margin:0">
<div id="list" style="height:200px;overflow-y:auto">
  <div id="a" style="height:400px"><div id="ha" style="position:sticky;top:0;height:40px"></div></div>
  <div id="b" style="height:400px"><div id="hb" style="position:sticky;top:10px;height:40px"></div></div>
</div></body></html>"#;

fn y(doc: &HtmlDocument, sel: &str) -> f32 {
    let id = doc.query_selector(sel).unwrap().unwrap();
    doc.get_node(id).unwrap().final_layout.location.y
}

fn scroll_to(doc: &mut HtmlDocument, top: f64) {
    let list = doc.query_selector("#list").unwrap().unwrap();
    let now = doc.get_node(list).unwrap().scroll_offset.y;
    doc.scroll_node_by(list, 0.0, now - top, |_| {});
    doc.resolve(0.0);
}

#[test]
fn a_header_sticks_while_its_section_scrolls() {
    let mut doc = layout_doc(HTML);
    assert_eq!(y(&doc, "#ha"), 0.0, "at rest it sits where layout put it");
    scroll_to(&mut doc, 150.0);
    assert!((y(&doc, "#ha") - 150.0).abs() < 0.5, "pinned at the top: {}", y(&doc, "#ha"));
}

#[test]
fn a_header_leaves_with_its_section() {
    let mut doc = layout_doc(HTML);
    scroll_to(&mut doc, 380.0);
    // Its section ends at 400: the 40px header can go no lower than 360.
    assert!((y(&doc, "#ha") - 360.0).abs() < 0.5, "held inside its section: {}", y(&doc, "#ha"));
    // The next header is not reached yet (its section starts at 400).
    assert_eq!(y(&doc, "#hb"), 0.0);
    scroll_to(&mut doc, 500.0);
    // 500 + 10 − 400 (its section's top) = 110 into its section.
    assert!((y(&doc, "#hb") - 110.0).abs() < 0.5, "the next one pinned 10px down: {}", y(&doc, "#hb"));
}
