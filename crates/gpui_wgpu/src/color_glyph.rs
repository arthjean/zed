//! Draws COLRv1 color glyphs on the CPU.
//!
//! swash draws COLRv0 layers, color bitmaps, and outlines, but not COLRv1, the
//! format of the "Noto Color Emoji" that Linux distributions ship: its glyphs
//! came out empty. skrifa walks the COLRv1 paint graph of a glyph and calls a
//! [`ColorPainter`]; this module is that painter. It keeps a stack of
//! transforms, of clip masks, and of layers in premultiplied RGBA, and draws
//! the four brushes (solid, linear, radial, and sweep gradients) with the
//! composite modes of the OpenType specification
//! (<https://learn.microsoft.com/en-us/typography/opentype/spec/colr>).
//!
//! The image it returns is the one swash would return for a color glyph:
//! straight (not premultiplied) RGBA, placed from the glyph origin.

use skrifa::{
    FontRef, GlyphId, MetadataProvider as _,
    color::{Brush, ColorPainter, ColorStop, CompositeMode, Extend, Transform},
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
    raw::TableProvider as _,
    raw::types::BoundingBox,
};
use swash::{
    scale::{Source, image::Content, image::Image},
    zeno::{Command, Mask, Placement, Point},
};

/// The largest side of a color glyph image, in pixels. A glyph that would be
/// larger is clipped to it.
const MAX_SIDE: f32 = 2048.;

/// The palette entry that stands for the text color.
const FOREGROUND: u16 = 0xFFFF;

/// Whether the face holds a COLR table of version 1 or later.
pub(crate) fn has_colr_v1(colr: Option<&[u8]>) -> bool {
    colr.and_then(|table| table.get(..2))
        .is_some_and(|version| u16::from_be_bytes([version[0], version[1]]) >= 1)
}

/// Draws `glyph_id` of the face at `face_index` of `data` at `ppem` pixels
/// per em, moved right and up by `offset` pixels. `None` when the glyph has
/// no COLRv1 paint, or when its paint graph is broken.
pub(crate) fn render(
    data: &[u8],
    face_index: u32,
    glyph_id: u32,
    ppem: f32,
    offset: (f32, f32),
) -> Option<Image> {
    let font = FontRef::from_index(data, face_index).ok()?;
    let glyph = font.color_glyphs().get_with_format(
        GlyphId::new(glyph_id),
        skrifa::color::ColorGlyphFormat::ColrV1,
    )?;
    let units_per_em = f32::from(font.head().ok()?.units_per_em());
    if units_per_em <= 0. || !ppem.is_finite() || ppem <= 0. {
        return None;
    }
    let scale = ppem / units_per_em;
    let location = LocationRef::default();

    // Font units, y up, to pixels from the glyph origin, y down.
    let to_origin = Affine {
        a: scale,
        b: 0.,
        c: 0.,
        d: -scale,
        e: offset.0,
        f: -offset.1,
    };
    let mut bounds = BoundsPainter::new(&font, to_origin);
    glyph.paint(location, &mut bounds).ok()?;
    let area = bounds.area?;
    let limit = MAX_SIDE.min(ppem * 8.);
    let x_min = area.x_min.max(-limit).floor();
    let y_min = area.y_min.max(-limit).floor();
    let x_max = area.x_max.min(limit).ceil();
    let y_max = area.y_max.min(limit).ceil();
    if x_max <= x_min || y_max <= y_min {
        return None;
    }
    let width = (x_max - x_min) as usize;
    let height = (y_max - y_min) as usize;

    let to_image = Affine {
        e: to_origin.e - x_min,
        f: to_origin.f - y_min,
        ..to_origin
    };
    let mut painter = Painter::new(&font, to_image, width, height);
    glyph.paint(location, &mut painter).ok()?;
    let data = painter.finish();

    Some(Image {
        source: Source::ColorOutline(0),
        content: Content::Color,
        placement: Placement {
            left: x_min as i32,
            top: -(y_min as i32),
            width: width as u32,
            height: height as u32,
        },
        data,
    })
}

/// An affine map: `x' = a x + c y + e`, `y' = b x + d y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `self` after `inner`: `inner` applies first.
    fn then(&self, inner: &Affine) -> Affine {
        Affine {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    fn inverse(&self) -> Option<Affine> {
        let determinant = self.a * self.d - self.b * self.c;
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            return None;
        }
        let inverse = 1. / determinant;
        let a = self.d * inverse;
        let b = -self.b * inverse;
        let c = -self.c * inverse;
        let d = self.a * inverse;
        Some(Affine {
            a,
            b,
            c,
            d,
            e: -(a * self.e + c * self.f),
            f: -(b * self.e + d * self.f),
        })
    }
}

impl From<Transform> for Affine {
    fn from(transform: Transform) -> Self {
        Affine {
            a: transform.xx,
            b: transform.yx,
            c: transform.xy,
            d: transform.yy,
            e: transform.dx,
            f: transform.dy,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x_min: f32,
    y_min: f32,
    x_max: f32,
    y_max: f32,
}

impl Rect {
    const EMPTY: Rect = Rect {
        x_min: f32::INFINITY,
        y_min: f32::INFINITY,
        x_max: f32::NEG_INFINITY,
        y_max: f32::NEG_INFINITY,
    };

    fn include(&mut self, (x, y): (f32, f32)) {
        self.x_min = self.x_min.min(x);
        self.y_min = self.y_min.min(y);
        self.x_max = self.x_max.max(x);
        self.y_max = self.y_max.max(y);
    }

    fn intersect(&self, other: &Rect) -> Rect {
        Rect {
            x_min: self.x_min.max(other.x_min),
            y_min: self.y_min.max(other.y_min),
            x_max: self.x_max.min(other.x_max),
            y_max: self.y_max.min(other.y_max),
        }
    }

    fn union(&self, other: &Rect) -> Rect {
        Rect {
            x_min: self.x_min.min(other.x_min),
            y_min: self.y_min.min(other.y_min),
            x_max: self.x_max.max(other.x_max),
            y_max: self.y_max.max(other.y_max),
        }
    }

    fn is_empty(&self) -> bool {
        !(self.x_min < self.x_max && self.y_min < self.y_max)
    }
}

/// The outline of a glyph, mapped by a transform as it is drawn.
struct PathPen {
    transform: Affine,
    commands: Vec<Command>,
}

impl PathPen {
    fn point(&self, x: f32, y: f32) -> Point {
        let (x, y) = self.transform.apply(x, y);
        Point::new(x, y)
    }
}

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.commands.push(Command::MoveTo(point));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.commands.push(Command::LineTo(point));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let control = self.point(cx0, cy0);
        let point = self.point(x, y);
        self.commands.push(Command::QuadTo(control, point));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let first = self.point(cx0, cy0);
        let second = self.point(cx1, cy1);
        let point = self.point(x, y);
        self.commands.push(Command::CurveTo(first, second, point));
    }

    fn close(&mut self) {
        self.commands.push(Command::Close);
    }
}

/// The path of `glyph_id` in the coordinates `transform` maps font units to.
fn glyph_path(font: &FontRef<'_>, glyph_id: GlyphId, transform: Affine) -> Vec<Command> {
    let mut pen = PathPen {
        transform,
        commands: Vec::new(),
    };
    if let Some(outline) = font.outline_glyphs().get(glyph_id) {
        let settings = DrawSettings::unhinted(Size::unscaled(), LocationRef::default());
        if outline.draw(settings, &mut pen).is_err() {
            pen.commands.clear();
        }
    }
    pen.commands
}

fn box_path(clip: &BoundingBox<f32>, transform: Affine) -> Vec<Command> {
    let corner = |x, y| {
        let (x, y) = transform.apply(x, y);
        Point::new(x, y)
    };
    vec![
        Command::MoveTo(corner(clip.x_min, clip.y_min)),
        Command::LineTo(corner(clip.x_max, clip.y_min)),
        Command::LineTo(corner(clip.x_max, clip.y_max)),
        Command::LineTo(corner(clip.x_min, clip.y_max)),
        Command::Close,
    ]
}

fn path_bounds(commands: &[Command]) -> Rect {
    let mut bounds = Rect::EMPTY;
    for command in commands {
        let points: &[Point] = match command {
            Command::MoveTo(point) | Command::LineTo(point) => std::slice::from_ref(point),
            Command::QuadTo(a, b) => &[*a, *b],
            Command::CurveTo(a, b, c) => &[*a, *b, *c],
            Command::Close => &[],
        };
        for point in points {
            bounds.include((point.x, point.y));
        }
    }
    bounds
}

/// Finds the area a color glyph paints: the union, over its fills, of the
/// clips in effect at each fill.
struct BoundsPainter<'a> {
    font: &'a FontRef<'a>,
    transforms: Vec<Affine>,
    clips: Vec<Rect>,
    area: Option<Rect>,
}

impl<'a> BoundsPainter<'a> {
    fn new(font: &'a FontRef<'a>, root: Affine) -> Self {
        Self {
            font,
            transforms: vec![root],
            clips: Vec::new(),
            area: None,
        }
    }

    fn transform(&self) -> Affine {
        self.transforms.last().copied().unwrap_or(Affine {
            a: 1.,
            b: 0.,
            c: 0.,
            d: 1.,
            e: 0.,
            f: 0.,
        })
    }

    fn push_clip_rect(&mut self, rect: Rect) {
        let clip = match self.clips.last() {
            Some(outer) => outer.intersect(&rect),
            None => rect,
        };
        self.clips.push(clip);
    }
}

impl ColorPainter for BoundsPainter<'_> {
    fn push_transform(&mut self, transform: Transform) {
        let next = self.transform().then(&Affine::from(transform));
        self.transforms.push(next);
    }

    fn pop_transform(&mut self) {
        self.transforms.pop();
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        let rect = path_bounds(&glyph_path(self.font, glyph_id, self.transform()));
        self.push_clip_rect(rect);
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        let rect = path_bounds(&box_path(&clip_box, self.transform()));
        self.push_clip_rect(rect);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill(&mut self, _brush: Brush<'_>) {
        // A fill with no clip would cover the whole plane: the clip box of
        // the glyph bounds it, and a glyph without one paints in clips.
        let Some(clip) = self.clips.last() else {
            return;
        };
        if clip.is_empty() {
            return;
        }
        self.area = Some(match &self.area {
            Some(area) => area.union(clip),
            None => *clip,
        });
    }

    fn push_layer(&mut self, _composite_mode: CompositeMode) {}
}

/// A color in premultiplied RGBA, each channel from 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Rgba {
    const TRANSPARENT: Rgba = Rgba {
        r: 0.,
        g: 0.,
        b: 0.,
        a: 0.,
    };

    fn premultiplied(r: f32, g: f32, b: f32, a: f32) -> Rgba {
        Rgba {
            r: r * a,
            g: g * a,
            b: b * a,
            a,
        }
    }

    fn scale(self, factor: f32) -> Rgba {
        Rgba {
            r: self.r * factor,
            g: self.g * factor,
            b: self.b * factor,
            a: self.a * factor,
        }
    }

    fn lerp(self, other: Rgba, t: f32) -> Rgba {
        Rgba {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }

    /// The color channels divided by alpha.
    fn straight(self) -> [f32; 3] {
        if self.a <= 0. {
            [0.; 3]
        } else {
            [self.r / self.a, self.g / self.a, self.b / self.a]
        }
    }
}

/// The color line of a gradient: stops in premultiplied RGBA, sorted.
struct ColorLine {
    stops: Vec<(f32, Rgba)>,
    extend: Extend,
}

impl ColorLine {
    fn at(&self, t: f32) -> Rgba {
        let t = match self.extend {
            Extend::Repeat => t - t.floor(),
            Extend::Reflect => {
                let period = t.rem_euclid(2.);
                if period > 1. { 2. - period } else { period }
            }
            _ => t,
        };
        let (Some(first), Some(last)) = (self.stops.first(), self.stops.last()) else {
            return Rgba::TRANSPARENT;
        };
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        for pair in self.stops.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            if t <= end.0 {
                let span = end.0 - start.0;
                if span <= 0. {
                    return end.1;
                }
                return start.1.lerp(end.1, (t - start.0) / span);
            }
        }
        last.1
    }
}

/// The position on the color line of a point of a linear gradient.
fn linear_position(p0: (f32, f32), p1: (f32, f32), point: (f32, f32)) -> Option<f32> {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let length = dx * dx + dy * dy;
    if length <= 1e-12 {
        return None;
    }
    Some(((point.0 - p0.0) * dx + (point.1 - p0.1) * dy) / length)
}

/// The position on the color line of a point of a two-circle radial
/// gradient: the largest `t` whose circle, of nonnegative radius, passes
/// through the point (the HTML canvas definition, which the COLR
/// specification takes).
fn radial_position(
    c0: (f32, f32),
    r0: f32,
    c1: (f32, f32),
    r1: f32,
    point: (f32, f32),
) -> Option<f32> {
    let (cdx, cdy) = (c1.0 - c0.0, c1.1 - c0.1);
    let (pdx, pdy) = (point.0 - c0.0, point.1 - c0.1);
    let dr = r1 - r0;
    let a = cdx * cdx + cdy * cdy - dr * dr;
    let b = pdx * cdx + pdy * cdy + r0 * dr;
    let c = pdx * pdx + pdy * pdy - r0 * r0;
    let radius_at = |t: f32| r0 + t * dr;
    if a.abs() < 1e-6 {
        if b.abs() < 1e-12 {
            return None;
        }
        let t = c / (2. * b);
        return (radius_at(t) >= 0.).then_some(t);
    }
    let discriminant = b * b - a * c;
    if discriminant < 0. {
        return None;
    }
    let root = discriminant.sqrt();
    let (high, low) = {
        let first = (b + root) / a;
        let second = (b - root) / a;
        if first >= second {
            (first, second)
        } else {
            (second, first)
        }
    };
    if radius_at(high) >= 0. {
        Some(high)
    } else if radius_at(low) >= 0. {
        Some(low)
    } else {
        None
    }
}

/// The position on the color line of a point of a sweep gradient. skrifa
/// gives angles in degrees, clockwise on the y-up design grid.
fn sweep_position(center: (f32, f32), start: f32, end: f32, point: (f32, f32)) -> Option<f32> {
    let span = end - start;
    if span.abs() < 1e-6 {
        return None;
    }
    let angle = (-(point.1 - center.1))
        .atan2(point.0 - center.0)
        .to_degrees()
        .rem_euclid(360.);
    Some((angle - start) / span)
}

#[derive(Clone, Copy)]
enum Shape {
    Solid(Rgba),
    Linear {
        p0: (f32, f32),
        p1: (f32, f32),
    },
    Radial {
        c0: (f32, f32),
        r0: f32,
        c1: (f32, f32),
        r1: f32,
    },
    Sweep {
        center: (f32, f32),
        start: f32,
        end: f32,
    },
}

struct Layer {
    pixels: Vec<Rgba>,
    mode: CompositeMode,
}

/// Paints a color glyph into an image of `width` by `height` pixels.
struct Painter<'a> {
    font: &'a FontRef<'a>,
    palette: Vec<Rgba>,
    width: usize,
    height: usize,
    transforms: Vec<Affine>,
    /// Coverage from 0 to 1 per pixel, each clip already intersected with
    /// the ones below it.
    clips: Vec<Vec<f32>>,
    layers: Vec<Layer>,
}

/// The first palette of the CPAL table, in premultiplied RGBA.
fn first_palette(font: &FontRef<'_>) -> Vec<Rgba> {
    let Ok(cpal) = font.cpal() else {
        return Vec::new();
    };
    let (Some(Ok(records)), Some(first)) = (
        cpal.color_records_array(),
        cpal.color_record_indices().first(),
    ) else {
        return Vec::new();
    };
    let first = usize::from(first.get());
    let count = usize::from(cpal.num_palette_entries());
    records
        .get(first..first + count)
        .unwrap_or_default()
        .iter()
        .map(|color| {
            Rgba::premultiplied(
                f32::from(color.red) / 255.,
                f32::from(color.green) / 255.,
                f32::from(color.blue) / 255.,
                f32::from(color.alpha) / 255.,
            )
        })
        .collect()
}

impl<'a> Painter<'a> {
    fn new(font: &'a FontRef<'a>, root: Affine, width: usize, height: usize) -> Self {
        let palette = first_palette(font);
        Self {
            font,
            palette,
            width,
            height,
            transforms: vec![root],
            clips: Vec::new(),
            layers: vec![Layer {
                pixels: vec![Rgba::TRANSPARENT; width * height],
                mode: CompositeMode::SrcOver,
            }],
        }
    }

    fn transform(&self) -> Affine {
        self.transforms[self.transforms.len() - 1]
    }

    /// A palette entry times `alpha`. The text color is not known when the
    /// glyph is drawn, and emoji fonts do not use it: it draws black.
    fn color(&self, palette_index: u16, alpha: f32) -> Rgba {
        let color = if palette_index == FOREGROUND {
            Rgba::premultiplied(0., 0., 0., 1.)
        } else {
            self.palette
                .get(usize::from(palette_index))
                .copied()
                .unwrap_or(Rgba::premultiplied(0., 0., 0., 1.))
        };
        color.scale(alpha.clamp(0., 1.))
    }

    fn color_line(&self, stops: &[ColorStop], extend: Extend) -> ColorLine {
        let mut stops = stops
            .iter()
            .map(|stop| (stop.offset, self.color(stop.palette_index, stop.alpha)))
            .collect::<Vec<_>>();
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        ColorLine { stops, extend }
    }

    fn coverage(&self, commands: &[Command]) -> Vec<f32> {
        let mut mask = vec![0u8; self.width * self.height];
        if !commands.is_empty() {
            Mask::new(commands)
                .size(self.width as u32, self.height as u32)
                .render_into(&mut mask, None);
        }
        mask.into_iter()
            .map(|coverage| f32::from(coverage) / 255.)
            .collect()
    }

    fn push_clip_coverage(&mut self, mut coverage: Vec<f32>) {
        if let Some(outer) = self.clips.last() {
            for (inner, outer) in coverage.iter_mut().zip(outer) {
                *inner *= outer;
            }
        }
        self.clips.push(coverage);
    }

    fn fill_shape(&mut self, shape: Shape, line: Option<&ColorLine>) {
        let Some(to_paint) = self.transform().inverse() else {
            return;
        };
        let width = self.width;
        let clip = self.clips.last();
        let Some(layer) = self.layers.last_mut() else {
            return;
        };
        for (index, pixel) in layer.pixels.iter_mut().enumerate() {
            let coverage = clip.map_or(1., |clip| clip[index]);
            if coverage <= 0. {
                continue;
            }
            let x = (index % width) as f32 + 0.5;
            let y = (index / width) as f32 + 0.5;
            let point = to_paint.apply(x, y);
            let color = match (shape, line) {
                (Shape::Solid(color), _) => Some(color),
                (Shape::Linear { p0, p1 }, Some(line)) => {
                    linear_position(p0, p1, point).map(|t| line.at(t))
                }
                (Shape::Radial { c0, r0, c1, r1 }, Some(line)) => {
                    radial_position(c0, r0, c1, r1, point).map(|t| line.at(t))
                }
                (Shape::Sweep { center, start, end }, Some(line)) => {
                    sweep_position(center, start, end, point).map(|t| line.at(t))
                }
                _ => None,
            };
            let Some(color) = color else {
                continue;
            };
            let source = color.scale(coverage);
            *pixel = composite(CompositeMode::SrcOver, source, *pixel);
        }
    }

    /// The image in straight RGBA, 8 bits per channel.
    fn finish(mut self) -> Vec<u8> {
        while self.layers.len() > 1 {
            self.merge_top_layer();
        }
        let mut data = Vec::with_capacity(self.width * self.height * 4);
        for pixel in self.layers.remove(0).pixels {
            let [r, g, b] = pixel.straight();
            for channel in [r, g, b, pixel.a] {
                data.push((channel.clamp(0., 1.) * 255.).round() as u8);
            }
        }
        data
    }

    fn merge_top_layer(&mut self) {
        let Some(source) = self.layers.pop() else {
            return;
        };
        let Some(backdrop) = self.layers.last_mut() else {
            self.layers.push(source);
            return;
        };
        for (destination, source_pixel) in backdrop.pixels.iter_mut().zip(source.pixels) {
            *destination = composite(source.mode, source_pixel, *destination);
        }
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, transform: Transform) {
        let next = self.transform().then(&Affine::from(transform));
        self.transforms.push(next);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        let commands = glyph_path(self.font, glyph_id, self.transform());
        let coverage = self.coverage(&commands);
        self.push_clip_coverage(coverage);
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        let commands = box_path(&clip_box, self.transform());
        let coverage = self.coverage(&commands);
        self.push_clip_coverage(coverage);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill(&mut self, brush: Brush<'_>) {
        let point = |p: skrifa::raw::types::Point<f32>| (p.x, p.y);
        match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => {
                let color = self.color(palette_index, alpha);
                self.fill_shape(Shape::Solid(color), None);
            }
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => {
                let line = self.color_line(color_stops, extend);
                if line.stops.len() == 1 {
                    self.fill_shape(Shape::Solid(line.stops[0].1), None);
                } else {
                    let shape = Shape::Linear {
                        p0: point(p0),
                        p1: point(p1),
                    };
                    self.fill_shape(shape, Some(&line));
                }
            }
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => {
                let line = self.color_line(color_stops, extend);
                if line.stops.len() == 1 {
                    self.fill_shape(Shape::Solid(line.stops[0].1), None);
                } else {
                    let shape = Shape::Radial {
                        c0: point(c0),
                        r0,
                        c1: point(c1),
                        r1,
                    };
                    self.fill_shape(shape, Some(&line));
                }
            }
            Brush::SweepGradient {
                c0,
                start_angle,
                end_angle,
                color_stops,
                extend,
            } => {
                let line = self.color_line(color_stops, extend);
                if line.stops.len() == 1 {
                    self.fill_shape(Shape::Solid(line.stops[0].1), None);
                } else {
                    // A pad sweep whose angles meet still draws its two ends.
                    let end = if start_angle == end_angle {
                        end_angle + 0.01
                    } else {
                        end_angle
                    };
                    let shape = Shape::Sweep {
                        center: point(c0),
                        start: start_angle,
                        end,
                    };
                    self.fill_shape(shape, Some(&line));
                }
            }
        }
    }

    fn push_layer(&mut self, composite_mode: CompositeMode) {
        self.layers.push(Layer {
            pixels: vec![Rgba::TRANSPARENT; self.width * self.height],
            mode: composite_mode,
        });
    }

    fn pop_layer(&mut self) {
        if self.layers.len() > 1 {
            self.merge_top_layer();
        }
    }
}

/// Composites `source` over `backdrop`, both premultiplied, with a mode of
/// the COLR table, as the W3C "Compositing and Blending" specification
/// defines it.
fn composite(mode: CompositeMode, source: Rgba, backdrop: Rgba) -> Rgba {
    let porter_duff = |source_factor: f32, backdrop_factor: f32| Rgba {
        r: source.r * source_factor + backdrop.r * backdrop_factor,
        g: source.g * source_factor + backdrop.g * backdrop_factor,
        b: source.b * source_factor + backdrop.b * backdrop_factor,
        a: source.a * source_factor + backdrop.a * backdrop_factor,
    };
    let (sa, ba) = (source.a, backdrop.a);
    match mode {
        CompositeMode::Clear => Rgba::TRANSPARENT,
        CompositeMode::Src => source,
        CompositeMode::Dest => backdrop,
        CompositeMode::SrcOver => porter_duff(1., 1. - sa),
        CompositeMode::DestOver => porter_duff(1. - ba, 1.),
        CompositeMode::SrcIn => porter_duff(ba, 0.),
        CompositeMode::DestIn => porter_duff(0., sa),
        CompositeMode::SrcOut => porter_duff(1. - ba, 0.),
        CompositeMode::DestOut => porter_duff(0., 1. - sa),
        CompositeMode::SrcAtop => porter_duff(ba, 1. - sa),
        CompositeMode::DestAtop => porter_duff(1. - ba, sa),
        CompositeMode::Xor => porter_duff(1. - ba, 1. - sa),
        CompositeMode::Plus => {
            let sum = porter_duff(1., 1.);
            Rgba {
                r: sum.r.min(1.),
                g: sum.g.min(1.),
                b: sum.b.min(1.),
                a: sum.a.min(1.),
            }
        }
        CompositeMode::Unknown => porter_duff(1., 1. - sa),
        blend => {
            let cs = source.straight();
            let cb = backdrop.straight();
            let mixed = blend_colors(blend, cb, cs);
            let channel = |index: usize, s: f32, b: f32| {
                s * (1. - ba) + b * (1. - sa) + sa * ba * mixed[index]
            };
            Rgba {
                r: channel(0, source.r, backdrop.r),
                g: channel(1, source.g, backdrop.g),
                b: channel(2, source.b, backdrop.b),
                a: sa + ba - sa * ba,
            }
        }
    }
}

/// `B(Cb, Cs)` of a blend mode, on straight colors.
fn blend_colors(mode: CompositeMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    let separable = |f: fn(f32, f32) -> f32| [f(cb[0], cs[0]), f(cb[1], cs[1]), f(cb[2], cs[2])];
    match mode {
        CompositeMode::Multiply => separable(|b, s| b * s),
        CompositeMode::Screen => separable(screen),
        CompositeMode::Overlay => separable(|b, s| hard_light(s, b)),
        CompositeMode::Darken => separable(f32::min),
        CompositeMode::Lighten => separable(f32::max),
        CompositeMode::ColorDodge => separable(|b, s| {
            if b <= 0. {
                0.
            } else if s >= 1. {
                1.
            } else {
                (b / (1. - s)).min(1.)
            }
        }),
        CompositeMode::ColorBurn => separable(|b, s| {
            if b >= 1. {
                1.
            } else if s <= 0. {
                0.
            } else {
                1. - ((1. - b) / s).min(1.)
            }
        }),
        CompositeMode::HardLight => separable(hard_light),
        CompositeMode::SoftLight => separable(|b, s| {
            if s <= 0.5 {
                b - (1. - 2. * s) * b * (1. - b)
            } else {
                let d = if b <= 0.25 {
                    ((16. * b - 12.) * b + 4.) * b
                } else {
                    b.sqrt()
                };
                b + (2. * s - 1.) * (d - b)
            }
        }),
        CompositeMode::Difference => separable(|b, s| (b - s).abs()),
        CompositeMode::Exclusion => separable(|b, s| b + s - 2. * b * s),
        CompositeMode::HslHue => set_luminosity(set_saturation(cs, saturation(cb)), luminosity(cb)),
        CompositeMode::HslSaturation => {
            set_luminosity(set_saturation(cb, saturation(cs)), luminosity(cb))
        }
        CompositeMode::HslColor => set_luminosity(cs, luminosity(cb)),
        CompositeMode::HslLuminosity => set_luminosity(cb, luminosity(cs)),
        _ => cs,
    }
}

fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b * 2. * s
    } else {
        screen(b, 2. * s - 1.)
    }
}

fn luminosity(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = luminosity(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut out = c;
    if n < 0. {
        for channel in &mut out {
            *channel = l + (*channel - l) * l / (l - n);
        }
    }
    if x > 1. {
        for channel in &mut out {
            *channel = l + (*channel - l) * (1. - l) / (x - l);
        }
    }
    out
}

fn set_luminosity(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - luminosity(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

fn saturation(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_saturation(c: [f32; 3], s: f32) -> [f32; 3] {
    let max = c[0].max(c[1]).max(c[2]);
    let min = c[0].min(c[1]).min(c[2]);
    if max <= min {
        return [0.; 3];
    }
    let scale = s / (max - min);
    [
        (c[0] - min) * scale,
        (c[1] - min) * scale,
        (c[2] - min) * scale,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn an_affine_inverse_undoes_it() {
        let transform = Affine {
            a: 2.,
            b: 0.5,
            c: -1.,
            d: 3.,
            e: 4.,
            f: -2.,
        };
        let inverse = transform.inverse().unwrap();
        let (x, y) = transform.apply(1.5, -7.);
        let (back_x, back_y) = inverse.apply(x, y);
        assert!(close(back_x, 1.5) && close(back_y, -7.));
    }

    #[test]
    fn then_applies_the_inner_transform_first() {
        let scale = Affine {
            a: 2.,
            b: 0.,
            c: 0.,
            d: 2.,
            e: 0.,
            f: 0.,
        };
        let translate = Affine {
            a: 1.,
            b: 0.,
            c: 0.,
            d: 1.,
            e: 3.,
            f: 0.,
        };
        assert_eq!(scale.then(&translate).apply(1., 0.), (8., 0.));
        assert_eq!(translate.then(&scale).apply(1., 0.), (5., 0.));
    }

    #[test]
    fn a_color_line_extends_as_asked() {
        let red = Rgba::premultiplied(1., 0., 0., 1.);
        let blue = Rgba::premultiplied(0., 0., 1., 1.);
        let line = |extend| ColorLine {
            stops: vec![(0., red), (1., blue)],
            extend,
        };
        assert_eq!(line(Extend::Pad).at(1.5), blue);
        assert!(close(line(Extend::Repeat).at(1.25).b, 0.25));
        assert!(close(line(Extend::Reflect).at(1.25).b, 0.75));
        assert!(close(line(Extend::Pad).at(0.5).r, 0.5));
    }

    #[test]
    fn a_linear_gradient_projects_on_its_axis() {
        let t = linear_position((0., 0.), (10., 0.), (2.5, 40.)).unwrap();
        assert!(close(t, 0.25));
        assert!(linear_position((1., 1.), (1., 1.), (0., 0.)).is_none());
    }

    #[test]
    fn a_radial_gradient_of_concentric_circles_grows_with_distance() {
        let t = radial_position((0., 0.), 0., (0., 0.), 10., (5., 0.)).unwrap();
        assert!(close(t, 0.5));
        let t = radial_position((0., 0.), 10., (0., 0.), 20., (15., 0.)).unwrap();
        assert!(close(t, 0.5));
    }

    #[test]
    fn a_radial_gradient_outside_its_cone_draws_nothing() {
        // Two equal circles side by side sweep a band: a point far above it
        // is on no circle.
        assert!(radial_position((0., 0.), 1., (10., 0.), 1., (5., 50.)).is_none());
    }

    #[test]
    fn a_sweep_gradient_turns_clockwise_on_the_design_grid() {
        // On the y-up grid, clockwise from +x reaches -y at 90 degrees.
        let t = sweep_position((0., 0.), 0., 360., (0., -1.)).unwrap();
        assert!(close(t, 0.25));
        let t = sweep_position((0., 0.), 0., 360., (0., 1.)).unwrap();
        assert!(close(t, 0.75));
    }

    #[test]
    fn source_over_keeps_the_backdrop_under_a_transparent_source() {
        let backdrop = Rgba::premultiplied(0., 1., 0., 1.);
        let half_red = Rgba::premultiplied(1., 0., 0., 0.5);
        let out = composite(CompositeMode::SrcOver, half_red, backdrop);
        assert!(close(out.r, 0.5) && close(out.g, 0.5) && close(out.a, 1.));
        assert_eq!(
            composite(CompositeMode::SrcOver, Rgba::TRANSPARENT, backdrop),
            backdrop
        );
    }

    #[test]
    fn porter_duff_modes_follow_their_factors() {
        let source = Rgba::premultiplied(1., 0., 0., 1.);
        let backdrop = Rgba::premultiplied(0., 0., 1., 0.5);
        assert_eq!(
            composite(CompositeMode::Clear, source, backdrop),
            Rgba::TRANSPARENT
        );
        assert_eq!(composite(CompositeMode::Dest, source, backdrop), backdrop);
        let source_in = composite(CompositeMode::SrcIn, source, backdrop);
        assert!(close(source_in.a, 0.5) && close(source_in.r, 0.5));
        let destination_out = composite(CompositeMode::DestOut, source, backdrop);
        assert!(close(destination_out.a, 0.));
    }

    #[test]
    fn multiply_of_white_keeps_the_backdrop() {
        let white = Rgba::premultiplied(1., 1., 1., 1.);
        let backdrop = Rgba::premultiplied(0.2, 0.4, 0.6, 1.);
        let out = composite(CompositeMode::Multiply, white, backdrop);
        assert!(close(out.r, 0.2) && close(out.g, 0.4) && close(out.b, 0.6));
    }

    #[test]
    fn luminosity_mode_takes_the_source_lightness() {
        let gray = [0.5, 0.5, 0.5];
        let out = blend_colors(CompositeMode::HslLuminosity, [1., 0., 0.], gray);
        assert!(close(luminosity(out), 0.5));
    }

    #[test]
    fn only_a_colr_table_of_version_one_counts() {
        assert!(has_colr_v1(Some(&[0, 1, 0, 0])));
        assert!(!has_colr_v1(Some(&[0, 0, 0, 0])));
        assert!(!has_colr_v1(None));
    }
}
