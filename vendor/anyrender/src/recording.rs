use std::sync::Arc;

use crate::{Filter, Glyph, NormalizedCoord, Paint, PaintRef, PaintScene, RenderContext};
use kurbo::{Affine, BezPath, Rect, Shape, Stroke};
use peniko::{BlendMode, Color, Fill, FontData, Style, StyleRef};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

const DEFAULT_TOLERANCE: f64 = 0.1;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum RenderCommand<Font = FontData, Brush = Paint> {
    /// Pushes a new layer clipped by the specified shape and composed with previous layers using the specified blend mode.
    /// Every drawing command after this call will be clipped by the shape until the layer is popped.
    /// However, the transforms are not saved or modified by the layer stack.
    PushLayer(LayerCommand),
    /// Pushes a new clip layer clipped by the specified shape.
    /// Every drawing command after this call will be clipped by the shape until the layer is popped.
    /// However, the transforms are not saved or modified by the layer stack.
    PushClipLayer(ClipCommand),
    /// Pops the current layer.
    PopLayer,
    /// Strokes a shape using the specified style and brush.
    Stroke(StrokeCommand<Brush>),
    /// Fills a shape using the specified style and brush.
    Fill(FillCommand<Brush>),
    /// Draws a run of glyphs
    GlyphRun(GlyphRunCommand<Font, Brush>),
    /// Draw a rounded rectangle blurred with a gaussian filter.
    BoxShadow(BoxShadowCommand),
}

impl RenderCommand {
    /// Apply the specific transform to the command
    fn apply_transform(mut self, transform: Affine) -> Self {
        match &mut self {
            RenderCommand::PushLayer(cmd) => cmd.transform = transform * cmd.transform,
            RenderCommand::PushClipLayer(cmd) => cmd.transform = transform * cmd.transform,
            RenderCommand::PopLayer => {}
            RenderCommand::Stroke(cmd) => cmd.transform = transform * cmd.transform,
            RenderCommand::Fill(cmd) => cmd.transform = transform * cmd.transform,
            RenderCommand::GlyphRun(cmd) => cmd.transform = transform * cmd.transform,
            RenderCommand::BoxShadow(cmd) => cmd.transform = transform * cmd.transform,
        };

        self
    }
}

/// Pushes a new layer clipped by the specified shape and composed with previous layers using the specified blend mode.
/// Every drawing command after this call will be clipped by the shape until the layer is popped.
/// However, the transforms are not saved or modified by the layer stack.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LayerCommand {
    #[cfg_attr(feature = "serde", serde(default))]
    pub fill: Fill,
    pub blend: BlendMode,
    pub alpha: f32,
    pub transform: Affine,
    #[cfg_attr(feature = "serde", serde(with = "svg_path"))]
    pub clip: BezPath, // TODO: more shape options
    pub filter: Option<Arc<Filter>>,
    pub backdrop_filter: Option<Arc<Filter>>,
}

/// Pushes a new clip layer clipped by the specified shape.
/// Every drawing command after this call will be clipped by the shape until the layer is popped.
/// However, the transforms are not saved or modified by the layer stack.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ClipCommand {
    #[cfg_attr(feature = "serde", serde(default))]
    pub fill: Fill,
    pub transform: Affine,
    #[cfg_attr(feature = "serde", serde(with = "svg_path"))]
    pub clip: BezPath, // TODO: more shape options
}

/// Strokes a shape using the specified style and brush.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StrokeCommand<Brush = Paint> {
    pub style: Stroke,
    pub transform: Affine,
    pub brush: Brush, // TODO: review ownership to avoid cloning. Should brushes be a "resource"?
    pub brush_transform: Option<Affine>,
    #[cfg_attr(feature = "serde", serde(with = "svg_path"))]
    pub shape: BezPath, // TODO: more shape options
}

/// Fills a shape using the specified style and brush.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FillCommand<Brush = Paint> {
    pub fill: Fill,
    pub transform: Affine,
    pub brush: Brush, // TODO: review ownership to avoid cloning. Should brushes be a "resource"?
    pub brush_transform: Option<Affine>,
    #[cfg_attr(feature = "serde", serde(with = "svg_path"))]
    pub shape: BezPath, // TODO: more shape options
}

/// Draws a run of glyphs
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GlyphRunCommand<Font = FontData, Brush = Paint> {
    pub font_data: Font,
    pub font_size: f32,
    pub hint: bool,
    pub normalized_coords: Vec<NormalizedCoord>,
    #[cfg_attr(feature = "serde", serde(default = "Default::default"))]
    pub embolden: kurbo::Vec2,
    pub style: Style,
    pub brush: Brush,
    pub brush_alpha: f32,
    pub transform: Affine,
    pub glyph_transform: Option<Affine>,
    pub glyphs: Vec<Glyph>,
}

/// Draw a box shadow around a box
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BoxShadowCommand {
    pub transform: Affine,
    pub rect: Rect,
    pub brush: Color,
    pub radius: f64,
    pub std_dev: f64,
}

/// A recording of a Scene or Scene Fragment stored as plain data types that can be stored
/// and passed around.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub tolerance: f64,
    pub commands: Vec<RenderCommand>,
    /// Recorded commands kept by reference ([`PaintScene::append_shared`]),
    /// each drawn before `commands[at]` (after the last command when `at`
    /// is past them), in order. A painter draws them where they are
    /// ([`PaintScene::append_scene`]); [`Scene::flattened`] lists them
    /// among the commands, copied.
    pub shared: Vec<SharedAt>,
}

/// Recorded commands drawn by reference at a place in a [`Scene`].
#[derive(Clone, Debug, PartialEq)]
pub struct SharedAt {
    /// The index in [`Scene::commands`] it is drawn before.
    pub at: usize,
    pub commands: Arc<Vec<RenderCommand>>,
    pub transform: Affine,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            tolerance: DEFAULT_TOLERANCE,
            commands: Vec::new(),
            shared: Vec::new(),
        }
    }
}

impl Scene {
    /// Create a new empty
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tolerance(tolerance: f64) -> Self {
        Self {
            tolerance,
            commands: Vec::new(),
            shared: Vec::new(),
        }
    }

    /// Every command it draws, in order, its shared parts copied in at
    /// their transforms: for a reader of the commands (a test, an
    /// encoder). A painter needs no copy ([`PaintScene::append_scene`]).
    pub fn flattened(&self) -> Vec<RenderCommand> {
        let mut out = Vec::with_capacity(self.commands.len());
        let mut shared = self.shared.iter().peekable();
        let put = |out: &mut Vec<RenderCommand>, part: &SharedAt| {
            out.extend(
                part.commands
                    .iter()
                    .cloned()
                    .map(|cmd| cmd.apply_transform(part.transform)),
            );
        };
        for (i, cmd) in self.commands.iter().enumerate() {
            while let Some(part) = shared.next_if(|p| p.at <= i) {
                put(&mut out, part);
            }
            out.push(cmd.clone());
        }
        for part in shared {
            put(&mut out, part);
        }
        out
    }

    fn convert_paint(&mut self, paint_ref: PaintRef<'_>) -> Paint {
        match paint_ref {
            Paint::Solid(color) => Paint::Solid(color),
            Paint::Gradient(gradient) => Paint::Gradient(gradient.clone()),
            Paint::Image(image) => Paint::Image(image.to_owned()),
            // TODO: handle this somehow
            Paint::Resource(id) => Paint::Resource(id),
            Paint::Custom(_) => Paint::Solid(Color::TRANSPARENT),
        }
    }
}

impl RenderContext for Scene {}
impl PaintScene for Scene {
    fn reset(&mut self) {
        self.shared.clear();
        self.commands.clear()
    }

    fn push_layer(
        &mut self,
        fill: Fill,
        blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        clip: &impl Shape,
        filter: Option<Arc<Filter>>,
        backdrop_filter: Option<Arc<Filter>>,
    ) {
        let blend = blend.into();
        let clip = clip.into_path(self.tolerance);
        let layer = LayerCommand {
            fill,
            blend,
            alpha,
            transform,
            clip,
            filter,
            backdrop_filter,
        };
        self.commands.push(RenderCommand::PushLayer(layer));
    }

    fn push_clip_layer(&mut self, fill: Fill, transform: Affine, clip: &impl Shape) {
        let clip = clip.into_path(self.tolerance);
        let layer = ClipCommand {
            fill,
            transform,
            clip,
        };
        self.commands.push(RenderCommand::PushClipLayer(layer));
    }

    fn pop_layer(&mut self) {
        self.commands.push(RenderCommand::PopLayer);
    }

    fn stroke<'a>(
        &mut self,
        style: &Stroke,
        transform: Affine,
        paint_ref: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let shape = shape.into_path(self.tolerance);
        let brush = self.convert_paint(paint_ref.into());
        let stroke = StrokeCommand {
            style: style.clone(),
            transform,
            brush,
            brush_transform,
            shape,
        };
        self.commands.push(RenderCommand::Stroke(stroke));
    }

    fn fill<'a>(
        &mut self,
        style: Fill,
        transform: Affine,
        paint: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let shape = shape.into_path(self.tolerance);
        let brush = self.convert_paint(paint.into());
        let fill = FillCommand {
            fill: style,
            transform,
            brush,
            brush_transform,
            shape,
        };
        self.commands.push(RenderCommand::Fill(fill));
    }

    fn draw_glyphs<'a, 's: 'a>(
        &'a mut self,
        font: &'a FontData,
        font_size: f32,
        hint: bool,
        normalized_coords: &'a [NormalizedCoord],
        embolden: kurbo::Vec2,
        style: impl Into<StyleRef<'a>>,
        paint_ref: impl Into<PaintRef<'a>>,
        brush_alpha: f32,
        transform: Affine,
        glyph_transform: Option<Affine>,
        glyphs: impl Iterator<Item = Glyph>,
    ) {
        let brush = self.convert_paint(paint_ref.into());
        let glyph_run = GlyphRunCommand {
            font_data: font.clone(),
            font_size,
            hint,
            normalized_coords: normalized_coords.to_vec(),
            embolden,
            style: style.into().to_owned(),
            brush,
            brush_alpha,
            transform,
            glyph_transform,
            glyphs: glyphs.into_iter().collect(),
        };
        self.commands.push(RenderCommand::GlyphRun(glyph_run));
    }

    fn draw_box_shadow(
        &mut self,
        transform: Affine,
        rect: Rect,
        brush: Color,
        radius: f64,
        std_dev: f64,
    ) {
        let box_shadow = BoxShadowCommand {
            transform,
            rect,
            brush,
            radius,
            std_dev,
        };
        self.commands.push(RenderCommand::BoxShadow(box_shadow));
    }

    fn append_scene(&mut self, scene: Scene, scene_transform: Affine) {
        // Its shared parts stay shared, at their places among the
        // commands appended after this scene's own.
        let base = self.commands.len();
        self.shared.extend(scene.shared.into_iter().map(|part| SharedAt {
            at: base + part.at,
            commands: part.commands,
            transform: scene_transform * part.transform,
        }));
        self.commands.extend(
            scene
                .commands
                .into_iter()
                .map(|cmd| cmd.apply_transform(scene_transform)),
        );
    }

    fn append_shared(&mut self, commands: &Arc<Vec<RenderCommand>>, transform: Affine) {
        self.shared.push(SharedAt {
            at: self.commands.len(),
            commands: Arc::clone(commands),
            transform,
        });
    }
}

/// Serde helper for serializing `BezPath` as an SVG path string.
#[cfg(feature = "serde")]
mod svg_path {
    use kurbo::BezPath;
    use serde::{self, Deserialize, Deserializer, Serializer};

    use crate::svg_path_parser;

    pub fn serialize<S>(path: &BezPath, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&path.to_svg())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BezPath, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        svg_path_parser::parse_svg_path(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;

    fn square(x: f64) -> Scene {
        let mut s = Scene::new();
        s.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::BLACK,
            None,
            &Rect::new(x, 0.0, x + 1.0, 1.0),
        );
        s
    }

    fn xs(commands: &[RenderCommand]) -> Vec<f64> {
        commands
            .iter()
            .filter_map(|c| match c {
                RenderCommand::Fill(f) => Some(f.transform.translation().x + f.shape.bounding_box().x0),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_recording_keeps_shared_commands_by_reference_at_their_place() {
        let layer = Arc::new(square(10.0).commands);
        let mut widget = square(0.0);
        widget.append_shared(&layer, Affine::translate((5.0, 0.0)));
        widget.append_scene(square(1.0), Affine::IDENTITY);
        assert!(Arc::ptr_eq(&widget.shared[0].commands, &layer), "copied, not shared");
        assert_eq!(widget.commands.len(), 2);
        assert_eq!(xs(&widget.flattened()), vec![0.0, 15.0, 1.0]);

        // Appended into another recording, still shared, still in place.
        let mut page = square(-1.0);
        page.append_scene(widget, Affine::translate((100.0, 0.0)));
        assert!(Arc::ptr_eq(&page.shared[0].commands, &layer));
        assert_eq!(xs(&page.flattened()), vec![-1.0, 100.0, 115.0, 101.0]);

        // Drawn into a painter, in the same order.
        assert!(page.shared.iter().all(|part| part.at == 2));
        let mut painter = Flat(Scene::new());
        painter.append_scene(page, Affine::IDENTITY);
        assert_eq!(xs(&painter.0.commands), vec![-1.0, 100.0, 115.0, 101.0]);
    }

    /// A painter with no recording of its own: what Vello's does, drawing
    /// every command it is given as it comes.
    struct Flat(Scene);

    impl RenderContext for Flat {}

    impl PaintScene for Flat {
        fn reset(&mut self) {
            self.0.reset();
        }
        fn push_layer(
            &mut self,
            fill: Fill,
            blend: impl Into<BlendMode>,
            alpha: f32,
            transform: Affine,
            clip: &impl Shape,
            filter: Option<Arc<Filter>>,
            backdrop_filter: Option<Arc<Filter>>,
        ) {
            self.0.push_layer(fill, blend, alpha, transform, clip, filter, backdrop_filter);
        }
        fn push_clip_layer(&mut self, fill: Fill, transform: Affine, clip: &impl Shape) {
            self.0.push_clip_layer(fill, transform, clip);
        }
        fn pop_layer(&mut self) {
            self.0.pop_layer();
        }
        fn stroke<'a>(
            &mut self,
            style: &Stroke,
            transform: Affine,
            brush: impl Into<PaintRef<'a>>,
            brush_transform: Option<Affine>,
            shape: &impl Shape,
        ) {
            self.0.stroke(style, transform, brush, brush_transform, shape);
        }
        fn fill<'a>(
            &mut self,
            style: Fill,
            transform: Affine,
            brush: impl Into<PaintRef<'a>>,
            brush_transform: Option<Affine>,
            shape: &impl Shape,
        ) {
            self.0.fill(style, transform, brush, brush_transform, shape);
        }
        fn draw_glyphs<'a, 's: 'a>(
            &'s mut self,
            font: &'a FontData,
            font_size: f32,
            hint: bool,
            normalized_coords: &'a [NormalizedCoord],
            embolden: kurbo::Vec2,
            style: impl Into<StyleRef<'a>>,
            brush: impl Into<PaintRef<'a>>,
            brush_alpha: f32,
            transform: Affine,
            glyph_transform: Option<Affine>,
            glyphs: impl Iterator<Item = Glyph> + Clone,
        ) {
            self.0.draw_glyphs(
                font,
                font_size,
                hint,
                normalized_coords,
                embolden,
                style,
                brush,
                brush_alpha,
                transform,
                glyph_transform,
                glyphs,
            );
        }
        fn draw_box_shadow(
            &mut self,
            transform: Affine,
            rect: Rect,
            brush: Color,
            radius: f64,
            std_dev: f64,
        ) {
            self.0.draw_box_shadow(transform, rect, brush, radius, std_dev);
        }
    }
}
