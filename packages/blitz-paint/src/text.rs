use anyrender::PaintScene;
use blitz_dom::{BaseDocument, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, Stroke};
use parley::{Affinity, Cursor, FontData, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use skrifa::{
    FontRef,
    charmap::Charmap,
    instance::{LocationRef, Size},
    metrics::GlyphMetrics,
};
use style::values::computed::TextDecorationLine;

use crate::SELECTION_COLOR;

/// Constraints used to truncate a single-line inline layout when
/// `overflow: {hidden,clip,scroll,auto}` is combined with
/// `text-overflow: ellipsis` (CSS UI Level 3 §8.2).
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct TextTruncation {
    /// Maximum line advance, in CSS px (pre-transform). Glyphs are cut off
    /// once this width (minus the ellipsis advance, if any) would be exceeded.
    pub max_advance: Option<f32>,
    /// When true, an ellipsis (U+2026) is drawn at the cutoff. When false
    /// the glyphs are simply clipped (equivalent to `text-overflow: clip`).
    pub ellipsis: bool,
}

/// Horizontal ellipsis character — CSS `text-overflow: ellipsis` renders this
/// glyph if it's present in the run's font. Falls back to three periods
/// otherwise.
const ELLIPSIS_CHAR: char = '\u{2026}';
const ELLIPSIS_FALLBACK_CHAR: char = '.';
const ELLIPSIS_FALLBACK_COUNT: usize = 3;

/// Resolved ellipsis shape for a particular font + size.
struct Ellipsis {
    /// Glyphs to draw at the cut-off point, in order. Each `(glyph_id, advance)`.
    glyphs: smallvec::SmallVec<[(u32, f32); 3]>,
    /// Total advance used by the ellipsis glyphs.
    total_advance: f32,
}

impl Ellipsis {
    fn shape(font: &FontData, font_size: f32) -> Option<Self> {
        let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
        let charmap = Charmap::new(&font_ref);
        let metrics = GlyphMetrics::new(&font_ref, Size::new(font_size), LocationRef::default());

        if let Some(glyph_id) = charmap.map(ELLIPSIS_CHAR) {
            let advance = metrics.advance_width(glyph_id).unwrap_or(0.0);
            let mut glyphs = smallvec::SmallVec::new();
            glyphs.push((glyph_id.to_u32(), advance));
            return Some(Self { glyphs, total_advance: advance });
        }

        // Font lacks U+2026 — fall back to three ASCII periods.
        let glyph_id = charmap.map(ELLIPSIS_FALLBACK_CHAR)?;
        let advance = metrics.advance_width(glyph_id).unwrap_or(0.0);
        let mut glyphs = smallvec::SmallVec::new();
        for _ in 0..ELLIPSIS_FALLBACK_COUNT {
            glyphs.push((glyph_id.to_u32(), advance));
        }
        Some(Self {
            glyphs,
            total_advance: advance * ELLIPSIS_FALLBACK_COUNT as f32,
        })
    }
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    truncation: TextTruncation,
) {
    // Per CSS UI §8.2, `text-overflow` applies to the last line that overflows
    // in the inline direction. Detecting multi-line overflow reliably is
    // follow-up work; for now we apply truncation only to the first line,
    // which covers the common `white-space: nowrap; overflow: hidden;` case.
    let mut lines_iter = lines;
    let Some(first_line) = lines_iter.next() else { return };

    paint_line(scene, &first_line, doc, transform, truncation);
    for line in lines_iter {
        paint_line(scene, &line, doc, transform, TextTruncation::default());
    }
}

fn paint_line<'a>(
    scene: &mut impl PaintScene,
    line: &Line<'a, TextBrush>,
    doc: &BaseDocument,
    transform: Affine,
    truncation: TextTruncation,
) {
    let max_advance = truncation.max_advance;
    let want_ellipsis = truncation.ellipsis;

    // Cached ellipsis shape, keyed on the first run's (font, size). Re-shaped
    // if a subsequent run uses a different font/size — rare within one line.
    let mut ellipsis: Option<(FontData, f32, Ellipsis)> = None;

    // Reserve space for the ellipsis glyphs when planning cut-offs.
    let reserve_advance = |font: &FontData, font_size: f32,
                           ellipsis: &mut Option<(FontData, f32, Ellipsis)>|
     -> f32 {
        if !want_ellipsis || max_advance.is_none() {
            return 0.0;
        }
        if let Some((f, s, e)) = ellipsis.as_ref() {
            if f == font && *s == font_size {
                return e.total_advance;
            }
        }
        if let Some(shaped) = Ellipsis::shape(font, font_size) {
            let advance = shaped.total_advance;
            *ellipsis = Some((font.clone(), font_size, shaped));
            advance
        } else {
            0.0
        }
    };

    let mut truncated = false;

    for item in line.items() {
        if truncated {
            break;
        }

        let PositionedLayoutItem::GlyphRun(glyph_run) = item else { continue };

        let run = glyph_run.run();
        let font = run.font();
        let font_size = run.font_size();
        let metrics = run.metrics();
        let style = glyph_run.style();
        let synthesis = run.synthesis();
        let glyph_xform = synthesis
            .skew()
            .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

        let styles = doc
            .get_node(style.brush.id)
            .unwrap()
            .primary_styles()
            .unwrap();
        let itext_styles = styles.get_inherited_text();
        let text_styles = styles.get_text();
        let text_color = itext_styles.color.as_color_color();
        let text_decoration_color = text_styles
            .text_decoration_color
            .as_absolute()
            .map(ToColorColor::as_color_color)
            .unwrap_or(text_color);
        let text_decoration_brush = anyrender::Paint::from(text_decoration_color);
        let text_decoration_line = text_styles.text_decoration_line;
        let has_underline = text_decoration_line.contains(TextDecorationLine::UNDERLINE);
        let has_strikethrough = text_decoration_line.contains(TextDecorationLine::LINE_THROUGH);

        // Reserve space for the ellipsis in this run's font (if we might need it).
        // Per CSS UI §8.2: if the container is too narrow to fit the ellipsis
        // at all, fall back to clip behaviour (no ellipsis). Detect that by
        // checking whether `max_advance` is greater than the ellipsis width.
        let ellipsis_reserve = reserve_advance(font, font_size, &mut ellipsis);
        let cutoff = max_advance.and_then(|m| {
            if want_ellipsis && m <= ellipsis_reserve {
                None // container too small for the ellipsis; behave like clip
            } else {
                Some(m - ellipsis_reserve)
            }
        });

        // Decide which glyphs actually fit. `glyph.x` is the layout-absolute
        // origin of the glyph along the inline axis.
        let kept_glyphs: smallvec::SmallVec<[parley::Glyph; 16]> = if let Some(cutoff) = cutoff {
            let mut out: smallvec::SmallVec<[parley::Glyph; 16]> = smallvec::SmallVec::new();
            for g in glyph_run.positioned_glyphs() {
                if g.x + g.advance > cutoff {
                    truncated = true;
                    break;
                }
                out.push(g);
            }
            out
        } else {
            glyph_run.positioned_glyphs().collect()
        };

        if !kept_glyphs.is_empty() {
            scene.draw_glyphs(
                font,
                font_size,
                true, // hint
                run.normalized_coords(),
                Fill::NonZero,
                &anyrender::Paint::from(text_color),
                1.0,
                transform,
                glyph_xform,
                kept_glyphs.iter().map(|g| anyrender::Glyph {
                    id: g.id as _,
                    x: g.x,
                    y: g.y,
                }),
            );
        }

        // Decorations cover only the emitted portion.
        let emitted_start = kept_glyphs.first().map(|g| g.x).unwrap_or(glyph_run.offset());
        let emitted_end = kept_glyphs
            .last()
            .map(|g| g.x + g.advance)
            .unwrap_or(glyph_run.offset());

        let mut draw_decoration_line = |offset: f32, size: f32, brush: &anyrender::Paint| {
            let x = emitted_start as f64;
            let w = (emitted_end - emitted_start) as f64;
            let y = (glyph_run.baseline() - offset + size / 2.0) as f64;
            let line = kurbo::Line::new((x, y), (x + w, y));
            scene.stroke(&Stroke::new(size as f64), transform, brush, None, &line)
        };

        if has_underline {
            let offset = metrics.underline_offset;
            let size = metrics.underline_size;
            // TODO: intercept line when crossing a descender like "gqy".
            draw_decoration_line(offset, size, &text_decoration_brush);
        }
        if has_strikethrough {
            let offset = metrics.strikethrough_offset;
            let size = metrics.strikethrough_size;
            draw_decoration_line(offset, size, &text_decoration_brush);
        }

        // If we truncated inside this run, emit the ellipsis using the same
        // font/size/colour as the run we're cutting.
        if truncated && want_ellipsis {
            if let Some((_, _, shaped)) = ellipsis.as_ref() {
                let mut cursor_x = emitted_end;
                let y = kept_glyphs
                    .last()
                    .map(|g| g.y)
                    .unwrap_or(glyph_run.baseline());
                scene.draw_glyphs(
                    font,
                    font_size,
                    true,
                    run.normalized_coords(),
                    Fill::NonZero,
                    &anyrender::Paint::from(text_color),
                    1.0,
                    transform,
                    glyph_xform,
                    shaped.glyphs.iter().map(|(id, advance)| {
                        let glyph = anyrender::Glyph {
                            id: *id as _,
                            x: cursor_x,
                            y,
                        };
                        cursor_x += *advance;
                        glyph
                    }),
                );
            }
        }
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
