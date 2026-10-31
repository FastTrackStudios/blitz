//! A press lands on what is painted on top, however deep its stacking
//! contexts nest and however it is transformed (studio#178).
//!
//! The case that failed: a dropdown panel centred under its cell with
//! `translateX(-50%)`, z-indexed (50) inside a z-indexed bar (40) inside a
//! z-indexed header (3), hanging over a large button. A press on the panel's
//! left half went to the button underneath.

use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

fn doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

const DROPDOWN: &str = r#"<html><body style="margin: 0">
<div style="position: relative; width: 800px; height: 600px;">
  <div style="position: relative; z-index: 3; height: 60px; display: flex;">
    <div style="position: relative; z-index: 40; width: 100%; height: 60px;">
      <div style="position: relative; width: 100px; height: 60px; margin-left: 300px;">
        <div id="panel" style="position: absolute; top: 100%; left: 50%; transform: translateX(-50%); z-index: 50; width: 300px; height: 100px; display: flex;">
          <button id="left" style="width: 100px; height: 100px;">left</button>
        </div>
      </div>
    </div>
  </div>
  <button id="under" style="display: block; width: 800px; height: 540px;">under</button>
</div>
</body></html>"#;

/// The hit's ancestry, innermost first, as `tag#id` words.
fn hit_chain(doc: &HtmlDocument, x: f32, y: f32) -> String {
    let id = doc.hit(x, y).map(|h| h.node_id);
    let mut out = String::new();
    let mut cur = id;
    while let Some(n) = cur.and_then(|i| doc.get_node(i)) {
        if let Some(el) = n.element_data() {
            let tag = el.name.local.to_string();
            let idattr = el.id.as_ref().map(|a| a.to_string()).unwrap_or_default();
            out.push_str(&format!("{tag}#{idattr} "));
        }
        cur = n.parent;
    }
    out
}

#[test]
fn a_press_on_a_centred_nested_dropdown_hits_the_dropdown() {
    let doc = doc(DROPDOWN);
    // The panel spans x 200-500 (its cell at 300-400, centred), y 60-160.
    // Its left half is the left button's, which may render an inner box.
    let hit = hit_chain(&doc, 250.0, 110.0);
    assert!(
        hit.contains("button#left") && !hit.contains("button#under"),
        "a press on the dropdown's left half: {hit}",
    );
    // Beside the panel, the button underneath still takes the press.
    let beside = hit_chain(&doc, 600.0, 110.0);
    assert!(
        beside.contains("button#under"),
        "beside the dropdown: {beside}"
    );
}
