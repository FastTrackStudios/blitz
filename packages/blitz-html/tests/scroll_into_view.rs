//! Scrolling a node into view in its scrolling ancestor — what a list does
//! to show the row in use when it opens with that row further down.

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

/// A 200px list of twenty 40px rows, row 15 the one to show.
fn list() -> String {
    let rows: String = (0..20)
        .map(|i| format!(r#"<button id="r{i}" style="display:block;width:100%;height:40px;border:none;padding:0">{i}</button>"#))
        .collect();
    format!(r#"<html><body style="margin:0"><div id="list" style="height:200px;overflow-y:auto">{rows}</div></body></html>"#)
}

fn scroll_y(doc: &HtmlDocument, sel: &str) -> f64 {
    let id = doc.query_selector(sel).unwrap().unwrap();
    doc.get_node(id).unwrap().scroll_offset.y
}

#[test]
fn a_row_below_the_fold_is_scrolled_to_the_middle() {
    let mut doc = layout_doc(&list());
    let row = doc.query_selector("#r15").unwrap().unwrap();
    assert!(doc.scroll_node_into_view(row));
    // Row 15 starts at 600; centred in 200px: 600 − (200 − 40) / 2 = 520.
    assert!((scroll_y(&doc, "#list") - 520.0).abs() < 1.0, "scrolled to {}", scroll_y(&doc, "#list"));
}

#[test]
fn a_row_in_view_stays_put() {
    let mut doc = layout_doc(&list());
    let row = doc.query_selector("#r2").unwrap().unwrap();
    assert!(doc.scroll_node_into_view(row));
    assert_eq!(scroll_y(&doc, "#list"), 0.0);
}
