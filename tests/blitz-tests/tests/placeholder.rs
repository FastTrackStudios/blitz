//! An input's `placeholder` is laid out, to be drawn while it is empty.

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

fn placeholder_width(doc: &HtmlDocument, sel: &str) -> Option<f32> {
    let id = doc.query_selector(sel).unwrap().unwrap();
    let el = doc.get_node(id).unwrap().data.downcast_element().unwrap();
    el.text_input_data().unwrap().placeholder.as_ref().map(|l| l.width())
}

fn value_width(doc: &HtmlDocument, sel: &str) -> f32 {
    let id = doc.query_selector(sel).unwrap().unwrap();
    let el = doc.get_node(id).unwrap().data.downcast_element().unwrap();
    el.text_input_data().unwrap().editor.try_layout().map_or(0.0, |l| l.width())
}

#[test]
fn a_placeholder_is_laid_out() {
    let doc = layout_doc(r#"<html><body><input id="a" placeholder="Search everything"><input id="b"><input id="c" value="Search everything"></body></html>"#);
    let w = placeholder_width(&doc, "#a").expect("the placeholder is laid out");
    // As wide as the same words typed in.
    assert_eq!(w, value_width(&doc, "#c"), "laid out as its text would be");
    assert!(placeholder_width(&doc, "#b").is_none(), "no attribute, no placeholder");
}
