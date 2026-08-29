use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use roxmltree::Document;
use svgtypes::{SimplePathSegment, SimplifyingPathParser};
use tauri::image::Image;
use tiny_skia::{
    BlendMode, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};

#[cfg(test)]
const ICON_SIZE: u32 = 36;
#[cfg(test)]
const ICON_POINTS: f32 = 18.0;
#[cfg(test)]
const ICON_SCALE: f32 = ICON_SIZE as f32 / ICON_POINTS;
pub const MAX_BARS: usize = 4;

const TEXT_HEIGHT: u32 = 36;
const OUTER_PADDING: f32 = 2.0;
const GROUP_GAP: f32 = 22.0;
const ICON_TEXT_GAP: f32 = 8.0;
const PROVIDER_ICON_SIZE: f32 = 32.0;
const PROVIDER_ICON_INSET: f32 = 1.0;
const COMPOSITE_BASE_CENTER: f32 = 34.2;
const COMPOSITE_BASE_SIZE: f32 = 42.0;
const COMPOSITE_UPSTREAM_CENTER: f32 = 72.0;
const COMPOSITE_UPSTREAM_SIZE: f32 = 64.0;
const COMPOSITE_MOAT_RADIUS: f32 = 23.0;
const _: () = {
    assert!(COMPOSITE_UPSTREAM_SIZE > COMPOSITE_BASE_SIZE * 1.35);
    assert!(COMPOSITE_UPSTREAM_SIZE > COMPOSITE_MOAT_RADIUS * 2.0);
};
const SINGLE_VALUE_SIZE: f32 = 23.0;
const STACKED_VALUE_SIZE: f32 = 17.0;
const STACKED_BASELINES: [f32; 2] = [15.0, 32.0];
const FONT_SOURCE: &[u8] = include_bytes!("../assets/fonts/Poppins-SemiBold.ttf");

const CLAUDE_ICON: &str = include_str!("../../src/assets/provider-icons/claude.svg");
const CODEX_ICON: &str = include_str!("../../src/assets/provider-icons/codex.svg");
const COPILOT_ICON: &str = include_str!("../../src/assets/provider-icons/copilot.svg");
const CURSOR_ICON: &str = include_str!("../../src/assets/provider-icons/cursor.svg");
const DEVIN_ICON: &str = include_str!("../../src/assets/provider-icons/devin.svg");
const ANTIGRAVITY_ICON: &str = include_str!("../../src/assets/provider-icons/antigravity.svg");
const GROK_ICON: &str = include_str!("../../src/assets/provider-icons/grok.svg");
const OPENCODE_ICON: &str = include_str!("../../src/assets/provider-icons/opencode.svg");
const OPENROUTER_ICON: &str = include_str!("../../src/assets/provider-icons/openrouter.svg");
const ZAI_ICON: &str = include_str!("../../src/assets/provider-icons/zai.svg");
const KIMI_ICON: &str = include_str!("../../src/assets/provider-icons/kimi.svg");
const MINIMAX_ICON: &str = include_str!("../../src/assets/provider-icons/minimax.svg");
const SUB2API_ICON: &str = include_str!("../../src/assets/provider-icons/sub2api.svg");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextGroup {
    pub provider_id: String,
    pub upstream_provider_id: Option<String>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BarGroup {
    pub provider_id: String,
    pub upstream_provider_id: Option<String>,
    pub fractions: Vec<f64>,
}

pub fn text_icon(groups: &[TextGroup]) -> Option<Image<'static>> {
    let strip = render_text_strip(groups)?;
    Some(Image::new_owned(strip.rgba, strip.width, TEXT_HEIGHT))
}

pub fn bar_strip_icon(groups: &[BarGroup]) -> Option<Image<'static>> {
    let strip = render_bar_strip(groups)?;
    Some(Image::new_owned(strip.rgba, strip.width, TEXT_HEIGHT))
}

#[cfg(test)]
pub fn bar_icon(fractions: &[f64]) -> Image<'static> {
    Image::new_owned(render_bar_rgba(fractions), ICON_SIZE, ICON_SIZE)
}

struct RenderedStrip {
    rgba: Vec<u8>,
    width: u32,
}

#[derive(Debug, Clone)]
struct GroupLayout<'a> {
    group: &'a TextGroup,
    text_width: f32,
    width: f32,
}

#[derive(Debug, Clone)]
struct BarGroupLayout<'a> {
    group: &'a BarGroup,
    width: f32,
}

fn render_text_strip(groups: &[TextGroup]) -> Option<RenderedStrip> {
    let groups = groups
        .iter()
        .filter(|group| !group.values.is_empty())
        .map(|group| {
            let text_width = group
                .values
                .iter()
                .take(2)
                .map(|value| {
                    measure_text(
                        value,
                        if group.values.len() == 1 {
                            SINGLE_VALUE_SIZE
                        } else {
                            STACKED_VALUE_SIZE
                        },
                    )
                })
                .fold(0.0_f32, f32::max)
                .ceil();
            GroupLayout {
                group,
                text_width,
                width: PROVIDER_ICON_SIZE + ICON_TEXT_GAP + text_width,
            }
        })
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return None;
    }

    let content_width = groups.iter().map(|group| group.width).sum::<f32>()
        + GROUP_GAP * groups.len().saturating_sub(1) as f32;
    let width = (content_width + OUTER_PADDING * 2.0).ceil().max(1.0) as u32;
    let mut pixmap = Pixmap::new(width, TEXT_HEIGHT).expect("menu bar strip dimensions are valid");
    let mut x = OUTER_PADDING;

    for layout in groups {
        draw_provider_icon(
            &mut pixmap,
            &layout.group.provider_id,
            layout.group.upstream_provider_id.as_deref(),
            x,
        );
        let text_x = x + PROVIDER_ICON_SIZE + ICON_TEXT_GAP;
        if layout.group.values.len() == 1 {
            let value = &layout.group.values[0];
            let baseline = centered_baseline(value, SINGLE_VALUE_SIZE, TEXT_HEIGHT as f32);
            draw_text(&mut pixmap, value, SINGLE_VALUE_SIZE, text_x, baseline);
        } else {
            for (value, baseline) in layout.group.values.iter().take(2).zip(STACKED_BASELINES) {
                let value_width = measure_text(value, STACKED_VALUE_SIZE);
                draw_text(
                    &mut pixmap,
                    value,
                    STACKED_VALUE_SIZE,
                    text_x + layout.text_width - value_width,
                    baseline,
                );
            }
        }
        x += layout.width + GROUP_GAP;
    }

    Some(RenderedStrip {
        rgba: pixmap.take_demultiplied(),
        width,
    })
}

fn render_bar_strip(groups: &[BarGroup]) -> Option<RenderedStrip> {
    const BAR_AREA_WIDTH: f32 = 36.0;

    let groups = groups
        .iter()
        .filter(|group| !group.fractions.is_empty())
        .map(|group| BarGroupLayout {
            group,
            width: PROVIDER_ICON_SIZE + ICON_TEXT_GAP + BAR_AREA_WIDTH,
        })
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return None;
    }

    let content_width = groups.iter().map(|group| group.width).sum::<f32>()
        + GROUP_GAP * groups.len().saturating_sub(1) as f32;
    let width = (content_width + OUTER_PADDING * 2.0).ceil().max(1.0) as u32;
    let mut pixmap = Pixmap::new(width, TEXT_HEIGHT).expect("menu bar strip dimensions are valid");
    let mut x = OUTER_PADDING;

    for layout in groups {
        draw_provider_icon(
            &mut pixmap,
            &layout.group.provider_id,
            layout.group.upstream_provider_id.as_deref(),
            x,
        );
        draw_bar_rows(
            &mut pixmap,
            &layout.group.fractions,
            x + PROVIDER_ICON_SIZE + ICON_TEXT_GAP,
            0.0,
            BAR_AREA_WIDTH,
            TEXT_HEIGHT as f32,
        );
        x += layout.width + GROUP_GAP;
    }

    Some(RenderedStrip {
        rgba: pixmap.take_demultiplied(),
        width,
    })
}

fn bundled_font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_SOURCE, FontSettings::default())
            .expect("bundled menu bar font must be valid")
    })
}

fn centered_baseline(text: &str, size: f32, height: f32) -> f32 {
    let font = bundled_font();
    let mut top = f32::MAX;
    let mut bottom = f32::MIN;
    for character in text.chars() {
        let metrics = font.metrics(character, size);
        top = top.min(-(metrics.height as f32 + metrics.ymin as f32));
        bottom = bottom.max(-(metrics.ymin as f32));
    }
    if top == f32::MAX {
        return height / 2.0;
    }
    (height - (bottom - top)) / 2.0 - top
}

fn measure_text(text: &str, size: f32) -> f32 {
    let font = bundled_font();
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        if let Some(previous) = previous {
            width += font
                .horizontal_kern(previous, character, size)
                .unwrap_or(0.0);
        }
        width += font.metrics(character, size).advance_width;
        previous = Some(character);
    }
    width.max(0.0)
}

fn draw_text(pixmap: &mut Pixmap, text: &str, size: f32, x: f32, baseline: f32) {
    let font = bundled_font();
    let mut pen_x = x;
    let mut previous = None;
    for character in text.chars() {
        if let Some(previous) = previous {
            pen_x += font
                .horizontal_kern(previous, character, size)
                .unwrap_or(0.0);
        }
        let (metrics, bitmap) = font.rasterize(character, size);
        let glyph_x = (pen_x + metrics.xmin as f32).round() as i32;
        let glyph_y = (baseline - metrics.height as f32 - metrics.ymin as f32).round() as i32;
        blend_alpha_mask(
            pixmap,
            &bitmap,
            metrics.width,
            metrics.height,
            glyph_x,
            glyph_y,
        );
        pen_x += metrics.advance_width;
        previous = Some(character);
    }
}

fn blend_alpha_mask(
    pixmap: &mut Pixmap,
    mask: &[u8],
    mask_width: usize,
    mask_height: usize,
    target_x: i32,
    target_y: i32,
) {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    let pixels = pixmap.data_mut();
    for source_y in 0..mask_height {
        let y = target_y + source_y as i32;
        if !(0..height).contains(&y) {
            continue;
        }
        for source_x in 0..mask_width {
            let x = target_x + source_x as i32;
            if !(0..width).contains(&x) {
                continue;
            }
            let alpha = mask[source_y * mask_width + source_x];
            let target = ((y * width + x) * 4) as usize;
            pixels[target + 3] = pixels[target + 3].max(alpha);
        }
    }
}

fn draw_provider_icon(
    pixmap: &mut Pixmap,
    provider_id: &str,
    upstream_provider_id: Option<&str>,
    x: f32,
) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    paint.anti_alias = true;
    let icon_top = (TEXT_HEIGHT as f32 - PROVIDER_ICON_SIZE) / 2.0;

    if let Some(path) = provider_path(provider_id) {
        let style = provider_mark_style(provider_id);
        if let Some((upstream_path, upstream_style)) =
            upstream_provider_id.and_then(|provider_id| {
                provider_path(provider_id).map(|path| (path, provider_mark_style(provider_id)))
            })
        {
            draw_composite_provider_icon(
                pixmap,
                path,
                style,
                upstream_path,
                upstream_style,
                x,
                icon_top,
                &paint,
            );
        } else {
            draw_standalone_provider_icon(pixmap, path, style, x, icon_top, &paint);
        }
    } else {
        let mut fallback = PathBuilder::new();
        fallback.push_circle(
            x + PROVIDER_ICON_SIZE / 2.0,
            icon_top + PROVIDER_ICON_SIZE / 2.0,
            (PROVIDER_ICON_SIZE - 2.0) / 2.0,
        );
        if let Some(path) = fallback.finish() {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_bar_rows(
    pixmap: &mut Pixmap,
    fractions: &[f64],
    x: f32,
    y: f32,
    area_width: f32,
    area_height: f32,
) {
    let count = fractions.len().min(2);
    if count == 0 {
        return;
    }

    let padding = (area_height * 0.08).round().max(1.0);
    let gap = (area_height * 0.03).round().max(1.0);
    let track_x = x + padding;
    let track_width = area_width - 2.0 * padding;
    let layout_count = count.max(2) as f32;
    let track_height = ((area_height - 2.0 * padding - (layout_count - 1.0) * gap) / layout_count)
        .floor()
        .max(1.0);
    let radius = (track_height / 3.0).floor().max(1.0);
    let total_height = count as f32 * track_height + count.saturating_sub(1) as f32 * gap;
    let y_offset = y + padding + ((area_height - 2.0 * padding - total_height) / 2.0).floor();

    for (index, fraction) in fractions.iter().take(count).enumerate() {
        let bar_y = y_offset + index as f32 * (track_height + gap) + 1.0;
        fill_rounded_bar(
            pixmap,
            track_x,
            bar_y,
            track_width,
            track_height,
            radius,
            radius,
            41,
        );

        let fill = bar_fill(track_width, *fraction);
        if fill.fill_width > 0.0 {
            let trailing = if fill.fill_width >= track_width {
                radius
            } else {
                (radius * 0.35).floor().max(0.0)
            };
            fill_rounded_bar(
                pixmap,
                track_x,
                bar_y,
                fill.fill_width,
                track_height,
                radius,
                trailing,
                255,
            );
        }
        if let Some(divider_x) = fill.divider_x {
            fill_rounded_bar(
                pixmap,
                track_x + divider_x,
                bar_y,
                fill.remainder_width,
                track_height,
                (radius * 0.2).floor().max(0.0),
                radius,
                61,
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderMarkStyle {
    Fill,
    Outline,
}

fn provider_mark_style(provider_id: &str) -> ProviderMarkStyle {
    match crate::providers::provider_family(provider_id) {
        "sub2api" => ProviderMarkStyle::Outline,
        _ => ProviderMarkStyle::Fill,
    }
}

fn provider_outline_stroke() -> Stroke {
    Stroke {
        width: 2.7,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

fn draw_provider_mark(
    pixmap: &mut Pixmap,
    path: &Path,
    style: ProviderMarkStyle,
    paint: &Paint,
    transform: Transform,
) {
    match style {
        ProviderMarkStyle::Fill => {
            pixmap.fill_path(path, paint, FillRule::Winding, transform, None)
        }
        ProviderMarkStyle::Outline => {
            pixmap.stroke_path(path, paint, &provider_outline_stroke(), transform, None)
        }
    }
}

fn centered_path_transform(
    path: &Path,
    center_x: f32,
    center_y: f32,
    target_size: f32,
    unit: f32,
    origin_x: f32,
    origin_y: f32,
) -> Transform {
    let bounds = path.bounds();
    let scale = (target_size / bounds.width()).min(target_size / bounds.height()) * unit;
    let tx = origin_x + center_x * unit - (bounds.left() + bounds.width() / 2.0) * scale;
    let ty = origin_y + center_y * unit - (bounds.top() + bounds.height() / 2.0) * scale;
    Transform::from_row(scale, 0.0, 0.0, scale, tx, ty)
}

fn draw_standalone_provider_icon(
    pixmap: &mut Pixmap,
    path: &Path,
    style: ProviderMarkStyle,
    x: f32,
    icon_top: f32,
    paint: &Paint,
) {
    let target_size = match style {
        ProviderMarkStyle::Fill => PROVIDER_ICON_SIZE - PROVIDER_ICON_INSET * 2.0,
        ProviderMarkStyle::Outline => PROVIDER_ICON_SIZE * 0.68,
    };
    let transform = centered_path_transform(
        path,
        PROVIDER_ICON_SIZE / 2.0,
        PROVIDER_ICON_SIZE / 2.0,
        target_size,
        1.0,
        x,
        icon_top,
    );
    draw_provider_mark(pixmap, path, style, paint, transform);
}

#[allow(clippy::too_many_arguments)]
fn draw_composite_provider_icon(
    pixmap: &mut Pixmap,
    base_path: &Path,
    base_style: ProviderMarkStyle,
    upstream_path: &Path,
    upstream_style: ProviderMarkStyle,
    x: f32,
    icon_top: f32,
    paint: &Paint,
) {
    // Keep both visual centers stable while letting the larger upstream mark overlap the smaller
    // unofficial-provider base. The narrow moat prevents monochrome interiors from merging.
    let unit = PROVIDER_ICON_SIZE / 100.0;
    let base_transform = centered_path_transform(
        base_path,
        COMPOSITE_BASE_CENTER,
        COMPOSITE_BASE_CENTER,
        COMPOSITE_BASE_SIZE,
        unit,
        x,
        icon_top,
    );
    draw_provider_mark(pixmap, base_path, base_style, paint, base_transform);

    let mut moat = PathBuilder::new();
    moat.push_circle(
        x + COMPOSITE_UPSTREAM_CENTER * unit,
        icon_top + COMPOSITE_UPSTREAM_CENTER * unit,
        COMPOSITE_MOAT_RADIUS * unit,
    );
    if let Some(moat) = moat.finish() {
        let clear = Paint {
            anti_alias: true,
            blend_mode: BlendMode::Clear,
            ..Paint::default()
        };
        pixmap.fill_path(
            &moat,
            &clear,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    let upstream_transform = centered_path_transform(
        upstream_path,
        COMPOSITE_UPSTREAM_CENTER,
        COMPOSITE_UPSTREAM_CENTER,
        COMPOSITE_UPSTREAM_SIZE,
        unit,
        x,
        icon_top,
    );
    draw_provider_mark(
        pixmap,
        upstream_path,
        upstream_style,
        paint,
        upstream_transform,
    );
}

fn provider_path(provider_id: &str) -> Option<&'static Path> {
    fn parsed(source: &'static str, slot: &'static OnceLock<Path>) -> &'static Path {
        slot.get_or_init(|| {
            parse_svg_path(source)
                .unwrap_or_else(|error| panic!("invalid bundled provider SVG: {error}"))
        })
    }

    static CLAUDE: OnceLock<Path> = OnceLock::new();
    static CODEX: OnceLock<Path> = OnceLock::new();
    static COPILOT: OnceLock<Path> = OnceLock::new();
    static CURSOR: OnceLock<Path> = OnceLock::new();
    static DEVIN: OnceLock<Path> = OnceLock::new();
    static ANTIGRAVITY: OnceLock<Path> = OnceLock::new();
    static GROK: OnceLock<Path> = OnceLock::new();
    static OPENCODE: OnceLock<Path> = OnceLock::new();
    static OPENROUTER: OnceLock<Path> = OnceLock::new();
    static ZAI: OnceLock<Path> = OnceLock::new();
    static KIMI: OnceLock<Path> = OnceLock::new();
    static MINIMAX: OnceLock<Path> = OnceLock::new();
    static SUB2API: OnceLock<Path> = OnceLock::new();
    match crate::providers::provider_family(provider_id) {
        "claude" => Some(parsed(CLAUDE_ICON, &CLAUDE)),
        "codex" => Some(parsed(CODEX_ICON, &CODEX)),
        "copilot" => Some(parsed(COPILOT_ICON, &COPILOT)),
        "cursor" => Some(parsed(CURSOR_ICON, &CURSOR)),
        "devin" => Some(parsed(DEVIN_ICON, &DEVIN)),
        "antigravity" => Some(parsed(ANTIGRAVITY_ICON, &ANTIGRAVITY)),
        "grok" => Some(parsed(GROK_ICON, &GROK)),
        "opencode" => Some(parsed(OPENCODE_ICON, &OPENCODE)),
        "openrouter" => Some(parsed(OPENROUTER_ICON, &OPENROUTER)),
        "zai" => Some(parsed(ZAI_ICON, &ZAI)),
        "kimi" => Some(parsed(KIMI_ICON, &KIMI)),
        "minimax" => Some(parsed(MINIMAX_ICON, &MINIMAX)),
        "sub2api" => Some(parsed(SUB2API_ICON, &SUB2API)),
        _ => None,
    }
}

fn parse_svg_path(source: &str) -> Result<Path, String> {
    let document = Document::parse(source).map_err(|error| error.to_string())?;
    let path_data = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "path")
        .filter_map(|node| node.attribute("d"))
        .collect::<Vec<_>>();
    if path_data.is_empty() {
        return Err("missing path data".to_owned());
    }

    let mut builder = PathBuilder::new();
    for data in path_data {
        for segment in SimplifyingPathParser::from(data) {
            match segment.map_err(|error| error.to_string())? {
                SimplePathSegment::MoveTo { x, y } => {
                    builder.move_to(x as f32, y as f32);
                }
                SimplePathSegment::LineTo { x, y } => {
                    builder.line_to(x as f32, y as f32);
                }
                SimplePathSegment::CurveTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x,
                    y,
                } => {
                    builder.cubic_to(
                        x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
                    );
                }
                SimplePathSegment::Quadratic { x1, y1, x, y } => {
                    builder.quad_to(x1 as f32, y1 as f32, x as f32, y as f32);
                }
                SimplePathSegment::ClosePath => {
                    builder.close();
                }
            }
        }
    }
    builder.finish().ok_or_else(|| "path is empty".into())
}

#[cfg(test)]
fn render_bar_rgba(fractions: &[f64]) -> Vec<u8> {
    let size = ICON_POINTS;
    let mut pixmap = Pixmap::new(ICON_SIZE, ICON_SIZE).expect("menu bar icon dimensions are valid");
    let count = fractions.len().min(MAX_BARS);
    if count == 0 {
        return pixmap.take_demultiplied();
    }

    let padding = (size * 0.08).round().max(1.0);
    let gap = (size * 0.03).round().max(1.0);
    let track_x = padding;
    let track_width = size - 2.0 * padding;
    let layout_count = count.max(2) as f32;
    let track_height = ((size - 2.0 * padding - (layout_count - 1.0) * gap) / layout_count)
        .floor()
        .max(1.0);
    let radius = (track_height / 3.0).floor().max(1.0);
    let total_height = count as f32 * track_height + count.saturating_sub(1) as f32 * gap;
    let y_offset = padding + ((size - 2.0 * padding - total_height) / 2.0).floor();

    for (index, fraction) in fractions.iter().take(MAX_BARS).enumerate() {
        let y = y_offset + index as f32 * (track_height + gap) + 1.0;
        fill_rounded_bar(
            &mut pixmap,
            track_x * ICON_SCALE,
            y * ICON_SCALE,
            track_width * ICON_SCALE,
            track_height * ICON_SCALE,
            radius * ICON_SCALE,
            radius * ICON_SCALE,
            41,
        );

        let fill = bar_fill(track_width, *fraction);
        if fill.fill_width > 0.0 {
            let trailing = if fill.fill_width >= track_width {
                radius
            } else {
                (radius * 0.35).floor().max(0.0)
            };
            fill_rounded_bar(
                &mut pixmap,
                track_x * ICON_SCALE,
                y * ICON_SCALE,
                fill.fill_width * ICON_SCALE,
                track_height * ICON_SCALE,
                radius * ICON_SCALE,
                trailing * ICON_SCALE,
                255,
            );
        }
        if let Some(divider_x) = fill.divider_x {
            fill_rounded_bar(
                &mut pixmap,
                (track_x + divider_x) * ICON_SCALE,
                y * ICON_SCALE,
                fill.remainder_width * ICON_SCALE,
                track_height * ICON_SCALE,
                (radius * 0.2).floor().max(0.0) * ICON_SCALE,
                radius * ICON_SCALE,
                61,
            );
        }
    }
    pixmap.take_demultiplied()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BarFill {
    fill_width: f32,
    remainder_width: f32,
    divider_x: Option<f32>,
}

fn visual_bar_fraction(fraction: f64) -> f64 {
    if !fraction.is_finite() {
        return 0.0;
    }
    let clamped = fraction.clamp(0.0, 1.0);
    if clamped > 0.7 && clamped < 1.0 {
        let remainder = 1.0 - clamped;
        let quantized = ((remainder / 0.15).ceil() * 0.15).min(1.0);
        1.0 - quantized
    } else {
        clamped
    }
}

fn bar_fill(track_width: f32, fraction: f64) -> BarFill {
    if !fraction.is_finite() || fraction <= 0.0 {
        return BarFill {
            fill_width: 0.0,
            remainder_width: 0.0,
            divider_x: None,
        };
    }
    let visual = visual_bar_fraction(fraction);
    if visual >= 1.0 {
        return BarFill {
            fill_width: track_width,
            remainder_width: 0.0,
            divider_x: None,
        };
    }
    let min_visible = 4.0_f32.max((track_width * 0.2).round());
    let max_fill_width = 1.0_f32.max(track_width - min_visible);
    let fill_width = 1.0_f32.max(max_fill_width.min((track_width * visual as f32).round()));
    let true_remainder = track_width - fill_width;
    let remainder_width = (track_width - 1.0).min(true_remainder.max(min_visible));
    BarFill {
        fill_width,
        remainder_width,
        divider_x: Some(track_width - remainder_width),
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rounded_bar(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    leading_radius: f32,
    trailing_radius: f32,
    alpha: u8,
) {
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let leading = leading_radius.min(height / 2.0).min(width / 2.0);
    let trailing = trailing_radius.min(height / 2.0).min(width / 2.0);
    let mut builder = PathBuilder::new();
    builder.move_to(x + leading, y);
    builder.line_to(x + width - trailing, y);
    builder.quad_to(x + width, y, x + width, y + trailing);
    builder.line_to(x + width, y + height - trailing);
    builder.quad_to(x + width, y + height, x + width - trailing, y + height);
    builder.line_to(x + leading, y + height);
    builder.quad_to(x, y + height, x, y + height - leading);
    builder.line_to(x, y + leading);
    builder.quad_to(x, y, x + leading, y);
    builder.close();
    let Some(path): Option<Path> = builder.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, alpha);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::{
        bar_fill, bar_icon, bar_strip_icon, draw_provider_icon, parse_svg_path, provider_path,
        render_bar_rgba, render_text_strip, text_icon, visual_bar_fraction, BarGroup, TextGroup,
        COMPOSITE_UPSTREAM_CENTER, COMPOSITE_UPSTREAM_SIZE, ICON_SIZE, MAX_BARS,
        PROVIDER_ICON_SIZE, TEXT_HEIGHT,
    };

    fn text_group(provider_id: &str, values: &[&str]) -> TextGroup {
        TextGroup {
            provider_id: provider_id.into(),
            upstream_provider_id: None,
            values: values.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn bar_group(provider_id: &str, fractions: &[f64]) -> BarGroup {
        BarGroup {
            provider_id: provider_id.into(),
            upstream_provider_id: None,
            fractions: fractions.to_vec(),
        }
    }

    fn provider_icon_rgba(provider_id: &str, upstream_provider_id: Option<&str>) -> Vec<u8> {
        let mut pixmap = tiny_skia::Pixmap::new(PROVIDER_ICON_SIZE as u32, TEXT_HEIGHT)
            .expect("provider icon dimensions should be valid");
        let mut group = text_group(provider_id, &["50%"]);
        group.upstream_provider_id = upstream_provider_id.map(str::to_owned);
        draw_provider_icon(
            &mut pixmap,
            &group.provider_id,
            group.upstream_provider_id.as_deref(),
            0.0,
        );
        pixmap.take_demultiplied()
    }

    #[test]
    fn bundled_provider_marks_and_font_render_into_a_retina_text_strip() {
        for provider in [
            "claude",
            "codex",
            "copilot",
            "cursor",
            "devin",
            "antigravity",
            "grok",
            "opencode",
            "openrouter",
            "zai",
            "kimi",
            "minimax",
            "sub2api",
        ] {
            let path = provider_path(provider).expect("known provider mark should exist");
            assert!(path.bounds().width() > 0.0);
            assert!(path.bounds().height() > 0.0);
        }

        let strip = render_text_strip(&[
            text_group("claude", &["100%", "36%"]),
            text_group("codex", &["100%", "89%"]),
            text_group("cursor", &["93%", "0%"]),
        ])
        .expect("provider values should produce a strip");
        assert_eq!(strip.rgba.len(), (strip.width * TEXT_HEIGHT * 4) as usize);
        assert!(strip.width > TEXT_HEIGHT * 3);
        assert!(strip
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255));
        let icon = text_icon(&[text_group("codex", &["75%", "40%"])])
            .expect("public text renderer should return an image");
        assert_eq!(icon.height(), TEXT_HEIGHT);
        assert!(icon.width() > TEXT_HEIGHT);
    }

    #[test]
    fn sub2api_text_marks_are_outlined_and_keep_upstream_silhouettes_distinct() {
        let outline = provider_icon_rgba("sub2api", None);
        let codex = provider_icon_rgba("sub2api", Some("codex"));
        let claude = provider_icon_rgba("sub2api@2", Some("claude"));
        let opaque_pixels = |rgba: &[u8]| {
            rgba.as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] >= 192)
                .count()
        };

        for mark in [&outline, &codex, &claude] {
            let opaque = opaque_pixels(mark);
            assert!(opaque > 80, "the provider mark should remain visible");
            assert!(
                opaque < 500,
                "the Sub2API outline must not collapse into a solid disk"
            );
        }
        assert_ne!(
            codex, claude,
            "each upstream should keep its own silhouette"
        );

        let fully_opaque_center = (10..26).all(|y| {
            (8..24).all(|x| {
                let alpha = outline[((y * PROVIDER_ICON_SIZE as usize + x) * 4) + 3];
                alpha == 255
            })
        });
        assert!(!fully_opaque_center);
    }

    #[test]
    fn composite_makes_the_upstream_mark_dominant_without_vertical_clipping() {
        let unit = PROVIDER_ICON_SIZE / 100.0;
        let icon_top = (TEXT_HEIGHT as f32 - PROVIDER_ICON_SIZE) / 2.0;
        let bottom_limit = (TEXT_HEIGHT as f32 - icon_top) / unit;
        assert!(COMPOSITE_UPSTREAM_CENTER + COMPOSITE_UPSTREAM_SIZE / 2.0 <= bottom_limit);
    }

    #[test]
    fn upstream_metadata_applies_composite_geometry_to_any_provider_mark() {
        let standalone = provider_icon_rgba("cursor", None);
        let composite = provider_icon_rgba("cursor", Some("claude"));
        assert_ne!(standalone, composite);
        assert!(composite
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn provider_mark_parser_combines_paths_and_resolves_relative_commands() {
        let path = parse_svg_path(
            r#"<svg><path d="M1 1l2 0v2h-2z"/><path d="M10 10c1 0 2 1 3 2s2 1 3 2"/></svg>"#,
        )
        .expect("valid provider paths should be combined");

        assert!(path.bounds().width() >= 15.0);
        assert!(path.bounds().height() >= 13.0);
    }

    #[test]
    fn text_strip_uses_natural_width_and_ignores_empty_groups() {
        assert!(render_text_strip(&[]).is_none());
        assert!(render_text_strip(&[text_group("codex", &[])]).is_none());

        let one =
            render_text_strip(&[text_group("codex", &["75%"])]).expect("one group should render");
        let two = render_text_strip(&[
            text_group("codex", &["75%"]),
            text_group("claude", &["80%", "40%"]),
        ])
        .expect("two groups should render");
        assert_eq!(one.rgba.len(), (one.width * TEXT_HEIGHT * 4) as usize);
        assert!(two.width > one.width);
    }

    #[test]
    fn bar_strip_uses_provider_groups_and_renders_each_pinned_quota() {
        let one = bar_strip_icon(&[bar_group("codex", &[0.75, 0.4])])
            .expect("one provider should render");
        let two = bar_strip_icon(&[
            bar_group("codex", &[0.75, 0.4]),
            bar_group("claude", &[0.8]),
        ])
        .expect("multiple providers should render");

        assert_eq!(one.height(), TEXT_HEIGHT);
        assert!(two.width() > one.width());
        assert!(two
            .rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255));
        assert!(bar_strip_icon(&[bar_group("codex", &[])]).is_none());
    }

    #[test]
    fn unknown_providers_receive_a_visible_neutral_fallback_mark() {
        assert!(provider_path("future-provider").is_none());
        let strip = render_text_strip(&[text_group("future-provider", &["42%"])])
            .expect("unknown provider should still render");
        assert!(strip
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn account_cards_reuse_their_provider_family_mark() {
        assert!(std::ptr::eq(
            provider_path("claude").unwrap(),
            provider_path("claude@1234abcd").unwrap()
        ));
    }

    #[test]
    fn renderer_preserves_empty_zero_and_full_states() {
        let alpha_pixels = |rgba: &[u8]| {
            rgba.as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] > 0)
                .count()
        };
        let empty = render_bar_rgba(&[]);
        let zero = render_bar_rgba(&[0.0]);
        let half = render_bar_rgba(&[0.5]);
        let full = render_bar_rgba(&[1.0]);

        assert_eq!(empty.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(alpha_pixels(&empty), 0);
        assert!(alpha_pixels(&zero) > 0);
        assert!(zero.as_chunks::<4>().0.iter().all(|pixel| pixel[3] < 255));

        let visible = zero
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, pixel)| pixel[3] > 0)
            .map(|(index, _)| (index % ICON_SIZE as usize, index / ICON_SIZE as usize))
            .collect::<Vec<_>>();
        let min_x = visible.iter().map(|point| point.0).min().unwrap();
        let max_x = visible.iter().map(|point| point.0).max().unwrap();
        let min_y = visible.iter().map(|point| point.1).min().unwrap();
        let max_y = visible.iter().map(|point| point.1).max().unwrap();
        assert!(max_x - min_x > max_y - min_y);
        assert!(half.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255));
        assert!(full.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn renderer_sanitizes_values_and_caps_the_visible_metric_count() {
        assert_eq!(
            render_bar_rgba(&[f64::NAN, -1.0, 2.0]),
            render_bar_rgba(&[0.0, 0.0, 1.0])
        );
        let fractions = [0.1, 0.3, 0.6, 1.0, 0.8];
        assert_eq!(
            render_bar_rgba(&fractions),
            render_bar_rgba(&fractions[..MAX_BARS])
        );
    }

    #[test]
    fn near_full_bars_keep_a_visible_remainder() {
        assert_eq!(visual_bar_fraction(0.0), 0.0);
        assert_eq!(visual_bar_fraction(0.5), 0.5);
        assert!((visual_bar_fraction(0.97) - 0.85).abs() < 0.0001);
        assert_eq!(visual_bar_fraction(1.0), 1.0);

        let near_full = bar_fill(16.0, 0.97);
        assert_eq!(near_full.fill_width, 12.0);
        assert_eq!(near_full.remainder_width, 4.0);
        assert_eq!(near_full.divider_x, Some(12.0));

        let full = bar_fill(16.0, 1.0);
        assert_eq!(full.fill_width, 16.0);
        assert_eq!(full.remainder_width, 0.0);
        assert_eq!(full.divider_x, None);
    }

    #[test]
    fn icon_uses_a_retina_density_square() {
        let icon = bar_icon(&[0.5]);
        assert_eq!((icon.width(), icon.height()), (36, 36));
    }
}
