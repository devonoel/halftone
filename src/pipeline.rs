use ab_glyph::{FontRef, PxScale};
use image::{DynamicImage, GenericImageView, Rgb, RgbImage, Rgba, RgbaImage, imageops::FilterType};
use imageproc::drawing::draw_text_mut;
use std::path::Path;

// Light-to-dense glyph ramp. Terminals default to a dark background, so this
// maps bright source pixels to dense glyphs (visible "ink" against the dark
// background) and dark pixels to blank space -- the opposite convention from
// print-based ASCII art (dark ink on white paper), which would run this ramp
// backwards.
const RAMP: &[u8] = b" .:-=+*#%@";

// Terminal cells are roughly twice as tall as they are wide, so a naive
// one-row-per-pixel-row mapping would stretch the image vertically. Same
// correction eyesore's vision code applies to circular field-of-view.
const CELL_ASPECT_RATIO: f64 = 2.0;

// Extra gamma lift applied to each cell's rendered color (see build_grid).
// Plain histogram equalization barely moves an already well-spread
// histogram, so ordinary (non-clustered) but dim/moody photos still render
// muddy without an additional deliberate brightness curve on top. In
// practice this alone is a weak lever for ordinary photos -- most of their
// darker/midtone pixels are also fairly low-saturation to begin with, so
// brightening them just yields brighter gray, not more visible color.
const COLOR_GAMMA: f64 = 1.4;

// Exponent applied to each cell's saturation (`saturation.powf(SATURATION_GAMMA)`,
// so < 1 lifts it). This is the lever that actually reads as "less muddy" at
// a glance: an ordinary photo's midtones are rarely far from gray, so
// pushing their (small) existing hue difference harder is what makes the
// render look like it has color in it, far more than adjusting brightness
// does on its own.
//
// A flat multiplier (the original approach here) does that fine for genuinely
// gray midtones, but clamping at 1.0 means it also flattens every already
// moderately-saturated color (a warm-toned "white" sand dune at ~0.4-0.56,
// say) up to full saturation alongside actually-vivid colors (a fiery sky at
// ~0.8-0.9) -- the two become visually indistinguishable, and the sand loses
// the paler, brighter quality that made it read as sand rather than solid
// orange. A power curve lifts low saturations at least as much (0.05 -> 0.22
// vs. the old 3x's 0.15) while leaving the relative ordering between
// mid/high saturations intact instead of clamping them all to the same
// ceiling.
const SATURATION_GAMMA: f64 = 0.5;

// JetBrains Mono, bundled under the SIL OFL 1.1 (see assets/JetBrainsMono-OFL.txt),
// so `--out foo.png` doesn't depend on whatever fonts happen to be on the
// machine building or running the binary. Bold rather than Regular: glyph
// strokes only ink a fraction of their cell, so most of a rendered PNG's
// pixel area is actually background color no matter how bright/saturated
// the cell color is -- heavier strokes claim more of that area as visible
// "ink" and measurably close the gap between cell color and final average
// pixel brightness. Has no bearing on ANSI/text output, which never touches
// a font at all.
const FONT_BYTES: &[u8] = include_bytes!("../assets/JetBrainsMono-Bold.ttf");

// Pixel size of one character cell when rasterizing to an image. Kept at the
// same 2:1 height:width ratio as CELL_ASPECT_RATIO above, for the same reason.
const CELL_PIXEL_WIDTH: u32 = 10;
const CELL_PIXEL_HEIGHT: u32 = 20;

pub struct Cell {
    pub glyph: char,
    pub color: [u8; 3],
}

/// Hue (degrees) and saturation (0..1) of an RGB color, discarding value --
/// used to re-apply a boosted brightness without touching hue/saturation.
/// Scaling R/G/B by a common factor and clamping (the naive way to brighten
/// a color) crushes the differences between channels once any of them hits
/// 255, which desaturates bright pixels toward gray/white. Adjusting value
/// in HSV space sidesteps that entirely.
fn rgb_to_hue_sat(r: u8, g: u8, b: u8) -> (f64, f64) {
    let r = r as f64 / 255.0;
    let g = g as f64 / 255.0;
    let b = b as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let saturation = if max > 0.0 { delta / max } else { 0.0 };
    let hue = if delta <= 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if max == g {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };

    (hue, saturation)
}

/// Rebuilds an RGB color from hue (degrees), saturation, and value (all
/// 0..1 except hue, which is 0..360).
fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> [u8; 3] {
    let c = value * saturation;
    let x = c * (1.0 - ((hue / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = value - c;

    let (r, g, b) = match hue as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    let channel = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [channel(r), channel(g), channel(b)]
}

pub struct Grid {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<Cell>,
}

pub fn convert_image(path: &Path, width: u32) -> Result<Grid, image::ImageError> {
    let img = image::open(path)?;
    Ok(build_grid(&img, width))
}

pub fn convert_bytes(bytes: &[u8], width: u32) -> Result<Grid, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    Ok(build_grid(&img, width))
}

fn build_grid(img: &DynamicImage, width: u32) -> Grid {
    let (img_w, img_h) = img.dimensions();

    let height = ((width as f64) * (img_h as f64) / (img_w as f64) / CELL_ASPECT_RATIO)
        .round()
        .max(1.0) as u32;

    // Resizing straight down to the target character grid lets the filter's
    // own downsampling do the per-cell averaging, rather than hand-rolling it.
    let small = img.resize_exact(width, height, FilterType::Lanczos3);
    let rgb = small.to_rgb8();

    let luminance_at = |x: u32, y: u32| -> u8 {
        let pixel = rgb.get_pixel(x, y);
        (0.2126 * pixel[0] as f64 + 0.7152 * pixel[1] as f64 + 0.0722 * pixel[2] as f64)
            .round()
            .clamp(0.0, 255.0) as u8
    };

    // Histogram equalization: place each pixel by where its luminance falls
    // in this image's own cumulative distribution, rather than its raw
    // linear position between darkest and brightest. A min/max stretch does
    // nothing for a source that already has a few truly black and truly
    // white pixels (this ends up being most photos/paintings) but whose
    // actual subject sits in a narrow, clustered midtone band -- it spreads
    // that cluster out across the ramp instead of collapsing it into one or
    // two glyphs.
    let mut luminances = vec![0u8; (width * height) as usize];
    let mut histogram = [0u32; 256];
    for y in 0..height {
        for x in 0..width {
            let l = luminance_at(x, y);
            luminances[(y * width + x) as usize] = l;
            histogram[l as usize] += 1;
        }
    }

    let total_pixels = (width * height) as f64;
    let mut cdf = [0u32; 256];
    let mut running = 0u32;
    for (value, &count) in histogram.iter().enumerate() {
        running += count;
        cdf[value] = running;
    }

    // Convert every pixel's equalized brightness into a continuous ramp
    // position (not yet rounded to a glyph) so dithering below has error to
    // work with.
    let max_level = (RAMP.len() - 1) as f64;
    let mut levels = vec![0f64; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let l = luminances[(y * width + x) as usize];
            let percentile = cdf[l as usize] as f64 / total_pixels;
            levels[(y * width + x) as usize] = percentile * max_level;
        }
    }

    // Floyd-Steinberg dithering. Rounding each cell to its nearest ramp
    // glyph independently is what produced the blobby, low-detail look --
    // neighboring cells with similar brightness all round to the same
    // glyph and any finer structure vanishes. Diffusing each cell's
    // rounding error into its not-yet-visited neighbors is the classic
    // halftone-printing trick for faking more tones than you actually have
    // ink levels for.
    let mut cells = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let value = levels[i];
            let idx = value.round().clamp(0.0, max_level) as usize;
            let glyph = RAMP[idx] as char;
            let pixel = rgb.get_pixel(x, y);

            // Equalization above only steers glyph choice, so a dim source
            // (a dark room, a backlit subject) still gets painted with its
            // literal, dim sampled color -- the densest glyphs read as
            // "brightest in this image" but can still be a dark RGB value,
            // muddying the whole render. Substituting the pixel's own
            // equalized brightness (same percentile used for glyph choice)
            // as its HSV value carries that correction into the color, not
            // just the glyph, while leaving hue/saturation alone -- scaling
            // R/G/B directly by a brightness ratio and clamping would wash
            // saturated colors out toward white as soon as any channel hits
            // 255. Pure linear equalization barely moves images whose
            // histogram is already spread out (most ordinary photos, as
            // opposed to the narrow-clustered case it's designed for), so
            // the percentile is also pushed through a gamma curve -- a
            // deliberate extra lift so midtones read brighter on screen
            // even when equalization alone has little to correct.
            let percentile = cdf[luminances[i] as usize] as f64 / total_pixels;
            let target_value = percentile.powf(1.0 / COLOR_GAMMA);
            let (hue, saturation) = rgb_to_hue_sat(pixel[0], pixel[1], pixel[2]);
            let saturation = saturation.powf(SATURATION_GAMMA);
            cells.push(Cell {
                glyph,
                color: hsv_to_rgb(hue, saturation, target_value),
            });

            let error = value - idx as f64;
            if x + 1 < width {
                levels[i + 1] += error * 7.0 / 16.0;
            }
            if y + 1 < height {
                let below = i + width as usize;
                if x > 0 {
                    levels[below - 1] += error * 3.0 / 16.0;
                }
                levels[below] += error * 5.0 / 16.0;
                if x + 1 < width {
                    levels[below + 1] += error * 1.0 / 16.0;
                }
            }
        }
    }

    Grid {
        width,
        height,
        cells,
    }
}

pub fn render_ansi(grid: &Grid, mono: bool) -> String {
    let mut out = String::with_capacity((grid.width as usize + 1) * grid.height as usize);
    for y in 0..grid.height {
        for x in 0..grid.width {
            let cell = &grid.cells[(y * grid.width + x) as usize];
            if mono {
                out.push(cell.glyph);
            } else {
                let [r, g, b] = cell.color;
                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{}", cell.glyph));
            }
        }
        if !mono {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

// Target luminance (out of 255) for an auto-computed background. Dark enough
// that bright glyphs still read clearly against it, but far off pure black so
// a warm/cool source image still reads as warm/cool rather than neutral.
const AUTO_BG_LUMINANCE: f64 = 34.0;

// How much of the image's average color to keep when tinting the background,
// from 0 (flat neutral gray) to 1 (the average color at full strength, the
// old behavior). A strongly single-hued source (a red sunset, a green
// jungle) averages to a fully-saturated color in that same hue -- rendered
// at full strength behind glyphs that are largely that same hue, the
// background stops reading as a backdrop and starts competing with the
// artwork for the same color, muddying the whole image even though its
// luminance is correctly dark. Muting the tint keeps a hint of "warm" or
// "cool" without giving the background enough saturation to compete.
const AUTO_BG_TINT: f64 = 0.25;

/// Derives a dark background color tinted toward the image's own average
/// color (its overall "temperature"), instead of defaulting to flat black
/// regardless of source.
pub fn auto_background(grid: &Grid) -> [u8; 3] {
    let n = grid.cells.len() as f64;
    let (mut r, mut g, mut b) = (0f64, 0f64, 0f64);
    for cell in &grid.cells {
        r += cell.color[0] as f64;
        g += cell.color[1] as f64;
        b += cell.color[2] as f64;
    }
    r /= n;
    g /= n;
    b /= n;

    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luminance <= 0.0 {
        return [0, 0, 0];
    }

    // Blend each channel toward this same luminance's neutral gray. The
    // gray's weighted sum is `luminance` by construction, so this blend
    // leaves the overall luminance unchanged -- only saturation drops --
    // and the scale step below still lands exactly on AUTO_BG_LUMINANCE.
    let r = luminance + (r - luminance) * AUTO_BG_TINT;
    let g = luminance + (g - luminance) * AUTO_BG_TINT;
    let b = luminance + (b - luminance) * AUTO_BG_TINT;

    let scale = AUTO_BG_LUMINANCE / luminance;
    let channel = |v: f64| (v * scale).round().clamp(0.0, 255.0) as u8;
    [channel(r), channel(g), channel(b)]
}

/// Background for `render_image`: either a fully solid backdrop, or a
/// (possibly zero-opacity) tinted one, so the art can be composited over
/// something else (a slide, a web page, another image) with the glyphs
/// still popping against a hint of backdrop color rather than nothing at
/// all. `Opaque` is kept as its own variant rather than folded into
/// `Translucent { alpha: 255, .. }` so the common case still renders onto a
/// plain `RgbImage` instead of carrying a needless all-255 alpha channel.
pub enum Background {
    Opaque([u8; 3]),
    Translucent { color: [u8; 3], alpha: u8 },
}

// Renders the same glyph grid to a raster image, so ASCII/ANSI art can be
// shared as a PNG/JPG rather than just pasted as text. Cell positions are
// placed directly from grid coordinates instead of running the font's own
// text layout, since we want a fixed-size monospace grid regardless of this
// particular font's own advance-width metrics.
//
// Opaque and translucent backgrounds need different pixel types (`Rgb<u8>`
// has nowhere to put an alpha channel), so this renders onto whichever of
// `RgbImage`/`RgbaImage` fits and hands back a `DynamicImage` -- callers
// don't need to care which one they got, since both save to any format that
// supports their color type.
pub fn render_image(grid: &Grid, mono: bool, bg: Background) -> DynamicImage {
    let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled font is valid");
    let scale = PxScale::from(CELL_PIXEL_HEIGHT as f32);
    let img_width = grid.width * CELL_PIXEL_WIDTH;
    let img_height = grid.height * CELL_PIXEL_HEIGHT;

    match bg {
        Background::Opaque(color) => {
            let mut canvas = RgbImage::from_pixel(img_width, img_height, Rgb(color));
            for y in 0..grid.height {
                for x in 0..grid.width {
                    let cell = &grid.cells[(y * grid.width + x) as usize];
                    if cell.glyph == ' ' {
                        continue;
                    }
                    let color = if mono {
                        Rgb([255, 255, 255])
                    } else {
                        Rgb(cell.color)
                    };
                    let mut buf = [0u8; 4];
                    let glyph_str = cell.glyph.encode_utf8(&mut buf);
                    draw_text_mut(
                        &mut canvas,
                        color,
                        (x * CELL_PIXEL_WIDTH) as i32,
                        (y * CELL_PIXEL_HEIGHT) as i32,
                        scale,
                        &font,
                        glyph_str,
                    );
                }
            }
            DynamicImage::ImageRgb8(canvas)
        }
        Background::Translucent { color, alpha } => {
            let mut canvas = RgbaImage::from_pixel(
                img_width,
                img_height,
                Rgba([color[0], color[1], color[2], alpha]),
            );
            for y in 0..grid.height {
                for x in 0..grid.width {
                    let cell = &grid.cells[(y * grid.width + x) as usize];
                    if cell.glyph == ' ' {
                        continue;
                    }
                    let [r, g, b] = if mono { [255, 255, 255] } else { cell.color };
                    let color = Rgba([r, g, b, 255]);
                    let mut buf = [0u8; 4];
                    let glyph_str = cell.glyph.encode_utf8(&mut buf);
                    draw_text_mut(
                        &mut canvas,
                        color,
                        (x * CELL_PIXEL_WIDTH) as i32,
                        (y * CELL_PIXEL_HEIGHT) as i32,
                        scale,
                        &font,
                        glyph_str,
                    );
                }
            }
            DynamicImage::ImageRgba8(canvas)
        }
    }
}
