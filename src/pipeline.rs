use ab_glyph::{FontRef, PxScale};
use image::{
    DynamicImage, GenericImageView, GrayImage, ImageBuffer, Luma, Pixel, Rgb, RgbImage, Rgba,
    RgbaImage, imageops::FilterType,
};
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
    /// Color this cell's own background blends toward: the color of
    /// the cell's darker sub-pixels, so the background carries detail the
    /// glyph color can't. With `--flat-bg` it's never painted, and is just
    /// a copy of `color`.
    pub shade: [u8; 3],
}

// Sub-pixels sampled per cell for two-tone mode, which per-cell backgrounds
// render with. 4x8 keeps them square (cells
// are 2:1, see CELL_ASPECT_RATIO) and is exactly 32 of them, so a cell's
// light/dark split -- and each candidate glyph's shape -- fits in one `u32`
// bitmask, with bit `sy * TWO_TONE_SUB_W + sx` for sub-pixel (sx, sy).
const TWO_TONE_SUB_W: u32 = 4;
const TWO_TONE_SUB_H: u32 = 8;

// Luminance gap (out of 255) between a cell's light and dark sub-pixel
// groups above which the cell counts as an edge and gets a shape-matched
// glyph. Below it, the split is mostly noise/texture, and the usual
// dithered density glyph reads better -- it's what keeps flat areas looking
// like ASCII art instead of a scatter of random punctuation.
const TWO_TONE_EDGE_CONTRAST: f64 = 60.0;

// How far each per-cell background is blended from the flat background
// toward the cell's own darker tone. Around 0.3-0.5 fills the gaps between
// glyphs with color while they still clearly stand out; much higher and the
// render turns into a solid color mosaic with the glyphs fading into it.
const CELL_BG_STRENGTH: f64 = 0.45;

// On edge cells, how much each sub-pixel of mismatch between a glyph's shape
// and the cell's light sub-pixels counts against it, in the same units as
// the RGB distance between the color a glyph would show and the color
// aimed for. Higher favors shape over color accuracy.
const TWO_TONE_SHAPE_WEIGHT: f64 = 1.0;

// Cap on color error carried into any one cell, per channel. A region whose
// target a cell's fg/bg pair can't reach at all (a bright aim over a cell
// whose light tone is dim) would otherwise pile error up cell after cell
// until it bursts out as a streak of mismatched glyphs downstream.
const COLOR_ERROR_LIMIT: f64 = 48.0;

// Glyphs eligible for shape matching on edge cells: ones with a clear
// directional or positional shape, plus the density ramp for mostly-filled
// cells. Letters and digits are left out on purpose -- they'd match fine on
// shape, but scattered through the art they read as text rather than as
// strokes. So is the blank glyph: a cell with only a sliver of light in it
// otherwise matches "nothing" best and loses its glyph entirely, punching
// holes along exactly the edges this is meant to sharpen.
const TWO_TONE_GLYPHS: &str = ".:-=+*#%@/\\|_()<>[]^~'`,;\"";

// Fraction of a glyph sub-pixel's area that must be inked for that sub-pixel
// to count as "on" in the glyph's shape mask.
const GLYPH_MASK_COVERAGE: f32 = 0.3;

/// A glyph as two-tone mode sees it: its 4x8 shape mask (see TWO_TONE_SUB_W)
/// and the fraction of the whole cell its ink covers, which is how much of
/// the cell shows the glyph color rather than the background.
struct GlyphShape {
    glyph: char,
    mask: u32,
    coverage: f64,
}

/// Rasterizes each of `glyphs` with the same bundled font used
/// for image export and reduces it to a 4x8 on/off shape mask (see
/// TWO_TONE_SUB_W), for matching against a cell's light/dark split. Rendered
/// at 10 px per sub-pixel and box-averaged down, since a 10x20 cell doesn't
/// divide evenly into 4 columns.
fn glyph_shapes(glyphs: &str) -> Vec<GlyphShape> {
    const SUPERSAMPLE: u32 = 10;
    let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled font is valid");
    let w = TWO_TONE_SUB_W * SUPERSAMPLE;
    let h = TWO_TONE_SUB_H * SUPERSAMPLE;
    let scale = PxScale::from(h as f32);

    glyphs
        .chars()
        .map(|glyph| {
            let mut canvas = GrayImage::new(w, h);
            let mut buf = [0u8; 4];
            draw_text_mut(
                &mut canvas,
                Luma([255]),
                0,
                0,
                scale,
                &font,
                glyph.encode_utf8(&mut buf),
            );

            let mut mask = 0u32;
            let mut total_ink = 0u32;
            for sy in 0..TWO_TONE_SUB_H {
                for sx in 0..TWO_TONE_SUB_W {
                    let mut ink = 0u32;
                    for py in sy * SUPERSAMPLE..(sy + 1) * SUPERSAMPLE {
                        for px in sx * SUPERSAMPLE..(sx + 1) * SUPERSAMPLE {
                            ink += canvas.get_pixel(px, py)[0] as u32;
                        }
                    }
                    total_ink += ink;
                    let coverage = ink as f32 / (255 * SUPERSAMPLE * SUPERSAMPLE) as f32;
                    if coverage > GLYPH_MASK_COVERAGE {
                        mask |= 1 << (sy * TWO_TONE_SUB_W + sx);
                    }
                }
            }
            GlyphShape {
                glyph,
                mask,
                coverage: total_ink as f64 / (255 * w * h) as f64,
            }
        })
        .collect()
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

/// A grid of glyph cells, ready for `render_ansi`/`render_image`. `cells` is
/// row-major and must hold exactly `width * height` cells; the renderers
/// index it by position and panic otherwise.
pub struct Grid {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<Cell>,
}

/// The result of converting an image: its grid, plus the backdrop color
/// derived from the source image for `--bg auto`.
pub struct Conversion {
    pub grid: Grid,
    /// A dark background tinted toward the image's own average color (its
    /// overall "temperature"), instead of defaulting to flat black
    /// regardless of source. Comes from the source image rather than the
    /// finished cells, which is why it lives here and not on `Grid`.
    pub auto_bg: [u8; 3],
}

pub struct ConvertSettings {
    /// Output width, in characters.
    pub width: u32,
    /// Per-cell backgrounds (off with `--flat-bg`), which also switch glyph
    /// and color choice to two-tone mode. It picks glyphs by the color each
    /// cell will actually show -- glyph and background mixed by ink coverage --
    /// so it needs to know the backgrounds up front, not just at render.
    pub cell_bg: bool,
    /// Explicit `--bg` color, or `None` for `auto` (derived from the image).
    pub bg: Option<[u8; 3]>,
}

pub fn convert_image(
    path: &Path,
    settings: &ConvertSettings,
) -> Result<Conversion, image::ImageError> {
    let img = image::open(path)?;
    Ok(build_grid(&img, settings))
}

pub fn convert_bytes(
    bytes: &[u8],
    settings: &ConvertSettings,
) -> Result<Conversion, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    Ok(build_grid(&img, settings))
}

fn luminance(pixel: [u8; 3]) -> f64 {
    0.2126 * pixel[0] as f64 + 0.7152 * pixel[1] as f64 + 0.0722 * pixel[2] as f64
}

/// A cell's sub-pixels split into a light and a dark group, for two-tone mode.
struct TwoToneSplit {
    light: [u8; 3],
    dark: [u8; 3],
    /// Which sub-pixels are in the light group, laid out like a glyph mask.
    mask: u32,
    /// Luminance gap between the two groups' average colors.
    contrast: f64,
    /// Average luminance of the whole cell, which both groups' brightness is
    /// measured relative to.
    mean_luminance: f64,
}

// Iterations of 2-means per cell. 32 points and two clusters settle almost
// immediately; this is a ceiling, not a typical count.
const TWO_TONE_KMEANS_ITERATIONS: usize = 6;

/// Splits one cell's 4x8 block of `sub` into two groups by color with
/// 2-means in RGB, seeded from its darkest and brightest sub-pixels, and
/// averages each group. Clustering on color rather than thresholding on
/// luminance is what lets a cell that's half red, half orange at similar
/// brightness split into those two tones at all -- a luminance threshold
/// only ever finds light-vs-dark. The brighter of the two groups is `light`.
fn two_tone_split(sub: &RgbImage, cell_x: u32, cell_y: u32) -> TwoToneSplit {
    const N: usize = (TWO_TONE_SUB_W * TWO_TONE_SUB_H) as usize;
    let mut pixels = [[0f64; 3]; N];
    for sy in 0..TWO_TONE_SUB_H {
        for sx in 0..TWO_TONE_SUB_W {
            let pixel = sub
                .get_pixel(cell_x * TWO_TONE_SUB_W + sx, cell_y * TWO_TONE_SUB_H + sy)
                .0;
            pixels[(sy * TWO_TONE_SUB_W + sx) as usize] = pixel.map(|c| c as f64);
        }
    }

    let lum = |p: &[f64; 3]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let dist = |a: &[f64; 3], b: &[f64; 3]| (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f64>();
    let by_lum = |a: &&[f64; 3], b: &&[f64; 3]| lum(a).total_cmp(&lum(b));
    let mut centroids = [
        *pixels.iter().max_by(by_lum).expect("cell has pixels"),
        *pixels.iter().min_by(by_lum).expect("cell has pixels"),
    ];

    let mut assignment = [0usize; N];
    for _ in 0..TWO_TONE_KMEANS_ITERATIONS {
        let mut changed = false;
        for (i, p) in pixels.iter().enumerate() {
            let group = usize::from(dist(p, &centroids[1]) < dist(p, &centroids[0]));
            changed |= assignment[i] != group;
            assignment[i] = group;
        }
        let mut sums = [[0f64; 3]; 2];
        let mut counts = [0f64; 2];
        for (p, &group) in pixels.iter().zip(&assignment) {
            for c in 0..3 {
                sums[group][c] += p[c];
            }
            counts[group] += 1.0;
        }
        for group in 0..2 {
            // An empty group (a perfectly flat cell) keeps its seed, which
            // for a flat cell is the same color as the other group anyway.
            if counts[group] > 0.0 {
                centroids[group] = sums[group].map(|v| v / counts[group]);
            }
        }
        if !changed {
            break;
        }
    }

    // Seeded brightest-first, but clustering on color can still end with
    // group 1 the brighter one; keep `light` meaning light.
    let light_group = usize::from(lum(&centroids[1]) > lum(&centroids[0]));
    let mut mask = 0u32;
    for (i, &group) in assignment.iter().enumerate() {
        if group == light_group {
            mask |= 1 << i;
        }
    }

    let to_rgb = |p: [f64; 3]| p.map(|v| v.round().clamp(0.0, 255.0) as u8);
    let light = to_rgb(centroids[light_group]);
    let dark = to_rgb(centroids[1 - light_group]);
    TwoToneSplit {
        light,
        dark,
        mask,
        contrast: luminance(light) - luminance(dark),
        mean_luminance: pixels.iter().map(lum).sum::<f64>() / N as f64,
    }
}

fn build_grid(img: &DynamicImage, settings: &ConvertSettings) -> Conversion {
    let (width, two_tone) = (settings.width, settings.cell_bg);
    let (img_w, img_h) = img.dimensions();

    let height = ((width as f64) * (img_h as f64) / (img_w as f64) / CELL_ASPECT_RATIO)
        .round()
        .max(1.0) as u32;

    // Resizing straight down to the target character grid lets the filter's
    // own downsampling do the per-cell averaging, rather than hand-rolling it.
    let small = img.resize_exact(width, height, FilterType::Lanczos3);
    let rgb = small.to_rgb8();

    // Two-tone mode needs to see inside each cell, not just its average, so it
    // gets its own higher-resolution resample alongside the one-pixel-per-cell
    // one above (which still drives equalization and dithering either way).
    let sub = two_tone.then(|| {
        img.resize_exact(
            width * TWO_TONE_SUB_W,
            height * TWO_TONE_SUB_H,
            FilterType::Lanczos3,
        )
        .to_rgb8()
    });
    let (edge_glyphs, ramp_glyphs) = if two_tone {
        let ramp = std::str::from_utf8(RAMP).expect("ramp is ASCII");
        (glyph_shapes(TWO_TONE_GLYPHS), glyph_shapes(ramp))
    } else {
        (Vec::new(), Vec::new())
    };

    let luminance_at =
        |x: u32, y: u32| -> u8 { luminance(rgb.get_pixel(x, y).0).round().clamp(0.0, 255.0) as u8 };

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

    // Equalization above only steers glyph choice, so a dim source (a dark
    // room, a backlit subject) would still get painted with its literal, dim
    // sampled color -- the densest glyphs read as "brightest in this image"
    // but can still be a dark RGB value, muddying the whole render.
    // Substituting a color's own equalized brightness (same percentile used
    // for glyph choice) as its HSV value carries that correction into the
    // color, not just the glyph, while leaving hue/saturation alone --
    // scaling R/G/B directly by a brightness ratio and clamping would wash
    // saturated colors out toward white as soon as any channel hits 255.
    // Pure linear equalization barely moves images whose histogram is
    // already spread out (most ordinary photos, as opposed to the
    // narrow-clustered case it's designed for), so the percentile is also
    // pushed through a gamma curve -- a deliberate extra lift so midtones
    // read brighter on screen even when equalization alone has little to
    // correct.
    let target_value = |lum: u8| -> f64 {
        let percentile = cdf[lum as usize] as f64 / total_pixels;
        percentile.powf(1.0 / COLOR_GAMMA)
    };
    let tone = |pixel: [u8; 3], value: f64| -> [u8; 3] {
        let (hue, saturation) = rgb_to_hue_sat(pixel[0], pixel[1], pixel[2]);
        let saturation = saturation.powf(SATURATION_GAMMA);
        hsv_to_rgb(hue, saturation, value.clamp(0.0, 1.0))
    };

    // Each cell's single corrected color, as plain mode paints it. Computed
    // up front because the auto background is derived from all of them, and
    // two-tone mode needs that background before it can pick any glyph.
    let mut brightnesses = Vec::with_capacity((width * height) as usize);
    let mut colors = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let brightness = target_value(luminances[(y * width + x) as usize]);
            brightnesses.push(brightness);
            colors.push(tone(rgb.get_pixel(x, y).0, brightness));
        }
    }
    let auto_bg = average_background(&colors);
    let base = settings.bg.unwrap_or(auto_bg).map(|c| c as f64);
    let blend_bg = |shade: [u8; 3]| -> [f64; 3] {
        std::array::from_fn(|c| base[c] + (shade[c] as f64 - base[c]) * CELL_BG_STRENGTH)
    };
    // What a cell actually shows on average: glyph color over `coverage` of
    // its area, background over the rest.
    let shown = |coverage: f64, fg: [u8; 3], bg: [f64; 3]| -> [f64; 3] {
        std::array::from_fn(|c| coverage * fg[c] as f64 + (1.0 - coverage) * bg[c])
    };
    // Plain-mode ramp positions before dithering, for two-tone mode, which
    // runs its own color-space dithering instead (see below).
    let undithered_levels = levels.clone();
    let mut color_error = vec![
        [0f64; 3];
        if two_tone {
            (width * height) as usize
        } else {
            0
        }
    ];

    let mut cells = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let value = levels[i];
            let idx = value.round().clamp(0.0, max_level) as usize;
            let glyph = RAMP[idx] as char;
            let brightness = brightnesses[i];
            let color = colors[i];

            // Two-tone mode (per-cell backgrounds): split the cell into two
            // tones and paint the glyph with the lighter one and the
            // background with the darker, so
            // the two colors a cell shows can actually differ -- in hue as
            // well as brightness -- instead of the background being a dimmed
            // copy of the glyph.
            //
            // Each tone keeps its own hue/saturation, but its brightness is
            // this cell's equalized value (the same one `color` gets) scaled
            // by how much brighter or darker that tone is than the cell
            // average, rather than being equalized independently.
            // Equalizing the halves on their own places a textured cell's
            // darker half low in the image's brightness distribution and
            // drives it close to black -- a hazy sky with a little grain
            // turns dark. Scaling relative to the cell keeps texture near
            // the cell's own brightness, while a real edge (a much darker
            // half) still gets a real gap.
            //
            // The glyph is then picked by color: aim for this cell's own two
            // tones mixed at its undithered ramp position's ink coverage --
            // the color it would show if glyphs came in continuous densities
            // -- plus the color error carried over from already-placed
            // neighbors, and take whichever glyph's actual fg/bg mix lands
            // closest. (Aiming at plain mode's mix instead doesn't work: a
            // two-tone background is darker than plain mode's, so every
            // cell falls short, every cell reaches for denser glyphs, and
            // the error saturates into a carpet of `@`.) The miss is carried on
            // to later neighbors, Floyd-Steinberg style but in RGB. Because
            // neighboring cells' two tones differ in hue, not only
            // brightness, a cell that comes out slightly too red can be
            // balanced by a neighbor choosing a sparser or denser glyph --
            // tones get blended across cells, not just within one.
            //
            // Where the gap between the tones is big enough to be a real
            // edge, the glyph's shape counts too -- how well its ink covers
            // the light sub-pixels -- so a diagonal edge through a cell
            // becomes `/` rather than whatever `+` or `*` lands on the right
            // color.
            let cell = match &sub {
                None => Cell {
                    glyph,
                    color,
                    shade: color,
                },
                Some(sub) => {
                    let split = two_tone_split(sub, x, y);
                    let mean = split.mean_luminance.max(1.0);
                    let relative = |c: [u8; 3]| brightness * luminance(c) / mean;
                    let fg = tone(split.light, relative(split.light));
                    let shade = tone(split.dark, relative(split.dark));
                    let bg = blend_bg(shade);

                    let level = undithered_levels[i];
                    let lower = level.floor().clamp(0.0, max_level) as usize;
                    let upper = level.ceil().clamp(0.0, max_level) as usize;
                    let frac = level - lower as f64;
                    let coverage = ramp_glyphs[lower].coverage * (1.0 - frac)
                        + ramp_glyphs[upper].coverage * frac;
                    let ideal = shown(coverage, fg, bg);
                    let aim: [f64; 3] = std::array::from_fn(|c| ideal[c] + color_error[i][c]);

                    let miss = |g: &GlyphShape| -> [f64; 3] {
                        let got = shown(g.coverage, fg, bg);
                        std::array::from_fn(|c| aim[c] - got[c])
                    };
                    let distance = |m: [f64; 3]| m.iter().map(|v| v * v).sum::<f64>().sqrt();
                    let (candidates, shape_weight) = if split.contrast >= TWO_TONE_EDGE_CONTRAST {
                        (&edge_glyphs, TWO_TONE_SHAPE_WEIGHT)
                    } else {
                        (&ramp_glyphs, 0.0)
                    };
                    let best = candidates
                        .iter()
                        .min_by(|a, b| {
                            let score = |g: &GlyphShape| {
                                distance(miss(g))
                                    + shape_weight * (g.mask ^ split.mask).count_ones() as f64
                            };
                            score(a).total_cmp(&score(b))
                        })
                        .expect("glyph set is non-empty");

                    let error = miss(best);
                    let mut spread = |j: usize, weight: f64| {
                        for c in 0..3 {
                            color_error[j][c] = (color_error[j][c] + error[c] * weight)
                                .clamp(-COLOR_ERROR_LIMIT, COLOR_ERROR_LIMIT);
                        }
                    };
                    if x + 1 < width {
                        spread(i + 1, 7.0 / 16.0);
                    }
                    if y + 1 < height {
                        let below = i + width as usize;
                        if x > 0 {
                            spread(below - 1, 3.0 / 16.0);
                        }
                        spread(below, 5.0 / 16.0);
                        if x + 1 < width {
                            spread(below + 1, 1.0 / 16.0);
                        }
                    }

                    Cell {
                        glyph: best.glyph,
                        color: fg,
                        shade,
                    }
                }
            };
            cells.push(cell);

            // Floyd-Steinberg dithering. Rounding each cell to its nearest
            // ramp glyph independently is what produced the blobby,
            // low-detail look -- neighboring cells with similar brightness
            // all round to the same glyph and any finer structure vanishes.
            // Diffusing each cell's rounding error into its not-yet-visited
            // neighbors is the classic halftone-printing trick for faking
            // more tones than you actually have ink levels for. (Only
            // plain mode's glyphs come from this; two-tone mode dithers in
            // color above.)
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

    Conversion {
        grid: Grid {
            width,
            height,
            cells,
        },
        auto_bg,
    }
}

/// Background color painted behind a single cell with per-cell backgrounds:
/// a blend from the flat `base` background toward the cell's `shade` (its
/// darker half), CELL_BG_STRENGTH of the way there. Staying below the glyph
/// color keeps the brightness gap that makes the glyph readable, while
/// filling the gaps between strokes with that cell's own hue.
///
/// Blending from `base` rather than simply dimming the glyph color toward
/// black matters for the darkest cells: equalization pins their brightness
/// near zero, so a pure scale leaves them as pitch-black tiles that punch
/// holes through otherwise continuous color. Starting from `base` lets them
/// settle back into the same tinted backdrop the rest of the image sits on.
fn cell_background(cell: &Cell, base: [u8; 3]) -> [u8; 3] {
    let blend = |c: u8, b: u8| {
        (b as f64 + (c as f64 - b as f64) * CELL_BG_STRENGTH)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let shade = cell.shade;
    [
        blend(shade[0], base[0]),
        blend(shade[1], base[1]),
        blend(shade[2], base[2]),
    ]
}

/// `cell_bg` paints each cell's own background (see `cell_background`),
/// blended from `base` -- the resolved `--bg` color, the same one the grid's
/// glyphs were picked against. Without it the terminal's own background
/// shows through; terminals have no notion of opacity, so a flat or
/// transparent backdrop is left to the terminal entirely.
pub fn render_ansi(grid: &Grid, mono: bool, cell_bg: bool, base: [u8; 3]) -> String {
    let mut out = String::with_capacity((grid.width as usize + 1) * grid.height as usize);
    for y in 0..grid.height {
        // Neighboring cells frequently land on the same color, and every
        // escape is ~19 bytes against a 1-byte glyph, so only emit one when
        // the color actually changes. Reset per row since each row ends
        // with `\x1b[0m`.
        let mut last_fg = None;
        let mut last_bg = None;
        for x in 0..grid.width {
            let cell = &grid.cells[(y * grid.width + x) as usize];
            if mono {
                out.push(cell.glyph);
                continue;
            }
            if cell_bg {
                let bg = cell_background(cell, base);
                if last_bg != Some(bg) {
                    let [r, g, b] = bg;
                    out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                    last_bg = Some(bg);
                }
            }
            if last_fg != Some(cell.color) {
                let [r, g, b] = cell.color;
                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                last_fg = Some(cell.color);
            }
            out.push(cell.glyph);
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

/// Computes `Conversion::auto_bg` from each cell's plain-mode color. Always from
/// those colors (rather than two-tone mode's lighter glyph tones) so the
/// backdrop doesn't shift between modes.
fn average_background(colors: &[[u8; 3]]) -> [u8; 3] {
    let n = colors.len() as f64;
    let (mut r, mut g, mut b) = (0f64, 0f64, 0f64);
    for color in colors {
        r += color[0] as f64;
        g += color[1] as f64;
        b += color[2] as f64;
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

/// Fills one character cell's pixel rectangle with a solid color, for
/// per-cell backgrounds. Generic over pixel type so the opaque (`Rgb`) and translucent
/// (`Rgba`) canvases in `render_image` can share it.
fn fill_cell<P: Pixel>(canvas: &mut ImageBuffer<P, Vec<P::Subpixel>>, x: u32, y: u32, color: P) {
    for py in y * CELL_PIXEL_HEIGHT..(y + 1) * CELL_PIXEL_HEIGHT {
        for px in x * CELL_PIXEL_WIDTH..(x + 1) * CELL_PIXEL_WIDTH {
            canvas.put_pixel(px, py, color);
        }
    }
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
//
// With `cell_bg`, every cell is first painted with its own backdrop color
// (see `cell_background`) before its glyph is drawn -- including blank cells,
// which become solid tiles instead of letting `bg` show through. The tiles
// cover `bg`'s color entirely, but a translucent `bg`'s alpha carries over
// to them so a semi-transparent export stays semi-transparent.
pub fn render_image(grid: &Grid, mono: bool, bg: Background, cell_bg: bool) -> DynamicImage {
    let cell_bg = cell_bg && !mono;
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
                    if cell_bg {
                        fill_cell(&mut canvas, x, y, Rgb(cell_background(cell, color)));
                    }
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
                    if cell_bg {
                        let [r, g, b] = cell_background(cell, color);
                        fill_cell(&mut canvas, x, y, Rgba([r, g, b, alpha]));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A horizontal gradient, black on the left to white on the right.
    fn gradient(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, _| {
            let v = (x * 255 / (width - 1)) as u8;
            Rgb([v, v, v])
        }))
    }

    /// Something with hue and structure to it, for tests that need more than
    /// a flat gradient: red rising left to right, blue rising top to bottom.
    fn colorful(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            Rgb([
                (x * 255 / (width - 1)) as u8,
                60,
                (y * 255 / (height - 1)) as u8,
            ])
        }))
    }

    fn settings(width: u32, cell_bg: bool) -> ConvertSettings {
        ConvertSettings {
            width,
            cell_bg,
            bg: None,
        }
    }

    fn uniform_grid(width: u32, height: u32, cell: fn() -> Cell) -> Grid {
        Grid {
            width,
            height,
            cells: (0..width * height).map(|_| cell()).collect(),
        }
    }

    fn ramp_index(glyph: char) -> usize {
        RAMP.iter()
            .position(|&c| c as char == glyph)
            .unwrap_or_else(|| panic!("{glyph:?} is not a ramp glyph"))
    }

    fn mask_rows(mask: u32) -> Vec<u32> {
        (0..TWO_TONE_SUB_H)
            .filter(|sy| (mask >> (sy * TWO_TONE_SUB_W)) & 0b1111 != 0)
            .collect()
    }

    fn mask_columns(mask: u32, row: u32) -> Vec<u32> {
        (0..TWO_TONE_SUB_W)
            .filter(|sx| (mask >> (row * TWO_TONE_SUB_W + sx)) & 1 == 1)
            .collect()
    }

    // --- color helpers ---

    #[test]
    fn hue_and_saturation_of_primaries_and_grays() {
        assert_eq!(rgb_to_hue_sat(255, 0, 0), (0.0, 1.0));
        assert_eq!(rgb_to_hue_sat(0, 255, 0), (120.0, 1.0));
        assert_eq!(rgb_to_hue_sat(0, 0, 255), (240.0, 1.0));
        assert_eq!(rgb_to_hue_sat(128, 128, 128), (0.0, 0.0));
        assert_eq!(rgb_to_hue_sat(0, 0, 0), (0.0, 0.0));
    }

    #[test]
    fn hsv_round_trips_through_hue_and_saturation() {
        for rgb in [
            [255, 0, 0],
            [12, 200, 90],
            [30, 60, 240],
            [250, 250, 5],
            [140, 20, 160],
            [77, 77, 77],
            [0, 0, 0],
            [255, 255, 255],
        ] {
            let (hue, sat) = rgb_to_hue_sat(rgb[0], rgb[1], rgb[2]);
            let value = *rgb.iter().max().unwrap() as f64 / 255.0;
            let back = hsv_to_rgb(hue, sat, value);
            for c in 0..3 {
                assert!(
                    (back[c] as i32 - rgb[c] as i32).abs() <= 1,
                    "{rgb:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn luminance_weights_green_most_and_blue_least() {
        assert_eq!(luminance([0, 0, 0]), 0.0);
        assert!((luminance([255, 255, 255]) - 255.0).abs() < 1e-9);
        assert!(luminance([0, 255, 0]) > luminance([255, 0, 0]));
        assert!(luminance([255, 0, 0]) > luminance([0, 0, 255]));
    }

    // --- auto background ---

    #[test]
    fn auto_background_lands_on_target_luminance() {
        for colors in [
            vec![[200, 30, 30]; 4],
            vec![[10, 10, 10], [250, 250, 250]],
            vec![[30, 180, 90], [90, 30, 200], [255, 255, 0]],
        ] {
            let bg = average_background(&colors);
            let lum = luminance(bg);
            assert!(
                (lum - AUTO_BG_LUMINANCE).abs() < 1.5,
                "{colors:?} gave {bg:?} at luminance {lum}"
            );
        }
    }

    #[test]
    fn auto_background_of_black_is_black() {
        assert_eq!(average_background(&[[0, 0, 0]; 3]), [0, 0, 0]);
    }

    #[test]
    fn auto_background_of_gray_is_neutral() {
        let [r, g, b] = average_background(&[[128, 128, 128]]);
        assert!(r == g && g == b, "got {:?}", [r, g, b]);
    }

    #[test]
    fn auto_background_keeps_a_muted_tint() {
        let source = [220, 40, 40];
        let bg = average_background(&[source]);
        let (source_hue, source_sat) = rgb_to_hue_sat(source[0], source[1], source[2]);
        let (hue, sat) = rgb_to_hue_sat(bg[0], bg[1], bg[2]);
        assert!(bg[0] > bg[1] && bg[0] > bg[2], "lost the red tint: {bg:?}");
        assert!((hue - source_hue).abs() < 5.0);
        assert!(sat < source_sat, "tint wasn't muted: {bg:?}");
    }

    #[test]
    fn cell_background_blends_from_base_toward_shade() {
        let cell = |shade| Cell {
            glyph: '#',
            color: [255, 255, 255],
            shade,
        };
        assert_eq!(
            cell_background(&cell([40, 50, 60]), [40, 50, 60]),
            [40, 50, 60]
        );
        let expected = (100.0 * CELL_BG_STRENGTH).round() as u8;
        assert_eq!(
            cell_background(&cell([100, 100, 100]), [0, 0, 0]),
            [expected; 3]
        );
    }

    // --- glyph shapes ---

    #[test]
    fn blank_glyph_has_no_ink() {
        let space = &glyph_shapes(" ")[0];
        assert_eq!(space.mask, 0);
        assert_eq!(space.coverage, 0.0);
    }

    #[test]
    fn glyph_masks_follow_glyph_shapes() {
        let shapes = glyph_shapes("-_|/\\");
        let [dash, underscore, bar, slash, backslash] = &shapes[..] else {
            unreachable!()
        };

        // `_` sits lower in the cell than `-`.
        assert!(mask_rows(underscore.mask)[0] > *mask_rows(dash.mask).last().unwrap());

        // `|` is one tall column.
        let rows = mask_rows(bar.mask);
        assert!(rows.len() >= TWO_TONE_SUB_H as usize / 2, "{rows:?}");
        for &row in &rows {
            assert_eq!(mask_columns(bar.mask, row), mask_columns(bar.mask, rows[0]));
        }

        // `/` leans right going up, `\` leans left.
        let lean = |mask: u32| {
            let rows = mask_rows(mask);
            let top = mask_columns(mask, rows[0])[0] as i32;
            let bottom = mask_columns(mask, *rows.last().unwrap())[0] as i32;
            top - bottom
        };
        assert!(lean(slash.mask) > 0);
        assert!(lean(backslash.mask) < 0);
    }

    #[test]
    fn denser_glyphs_cover_more_of_the_cell() {
        let shapes = glyph_shapes(".:#@");
        for pair in shapes.windows(2) {
            assert!(
                pair[0].coverage < pair[1].coverage,
                "{:?} covers more than {:?}",
                pair[0].glyph,
                pair[1].glyph
            );
        }
    }

    // --- conversion ---

    #[test]
    fn grid_height_corrects_for_cell_aspect_ratio() {
        let conversion = build_grid(&gradient(200, 100), &settings(40, false));
        assert_eq!(conversion.grid.width, 40);
        assert_eq!(conversion.grid.height, 10);
        assert_eq!(conversion.grid.cells.len(), 400);
    }

    #[test]
    fn very_wide_images_still_get_one_row() {
        let conversion = build_grid(&gradient(1000, 2), &settings(20, true));
        assert_eq!(conversion.grid.height, 1);
        assert_eq!(conversion.grid.cells.len(), 20);
    }

    #[test]
    fn flat_mode_uses_only_ramp_glyphs_and_copies_color_to_shade() {
        let conversion = build_grid(&colorful(64, 64), &settings(32, false));
        for cell in &conversion.grid.cells {
            ramp_index(cell.glyph);
            assert_eq!(cell.shade, cell.color);
        }
    }

    #[test]
    fn two_tone_mode_uses_only_known_glyphs() {
        let conversion = build_grid(&colorful(64, 64), &settings(32, true));
        let allowed: Vec<char> = std::str::from_utf8(RAMP)
            .unwrap()
            .chars()
            .chain(TWO_TONE_GLYPHS.chars())
            .collect();
        for cell in &conversion.grid.cells {
            assert!(allowed.contains(&cell.glyph), "unexpected {:?}", cell.glyph);
        }
    }

    #[test]
    fn brighter_source_regions_get_denser_glyphs() {
        let conversion = build_grid(&gradient(256, 128), &settings(40, false));
        let grid = &conversion.grid;
        let (mut left, mut right) = (0, 0);
        for y in 0..grid.height {
            for x in 0..grid.width {
                let index = ramp_index(grid.cells[(y * grid.width + x) as usize].glyph);
                if x < grid.width / 2 {
                    left += index;
                } else {
                    right += index;
                }
            }
        }
        assert!(right > left * 2, "left {left}, right {right}");
    }

    #[test]
    fn conversion_is_deterministic() {
        let a = build_grid(&colorful(80, 60), &settings(30, true));
        let b = build_grid(&colorful(80, 60), &settings(30, true));
        assert_eq!(a.auto_bg, b.auto_bg);
        for (a, b) in a.grid.cells.iter().zip(&b.grid.cells) {
            assert_eq!((a.glyph, a.color, a.shade), (b.glyph, b.color, b.shade));
        }
    }

    #[test]
    fn auto_background_is_the_same_in_both_modes_and_ignores_explicit_bg() {
        let image = colorful(80, 60);
        let flat = build_grid(&image, &settings(30, false)).auto_bg;
        let two_tone = build_grid(&image, &settings(30, true)).auto_bg;
        let explicit = build_grid(
            &image,
            &ConvertSettings {
                bg: Some([200, 0, 0]),
                ..settings(30, true)
            },
        )
        .auto_bg;
        assert_eq!(flat, two_tone);
        assert_eq!(flat, explicit);
    }

    #[test]
    fn explicit_background_changes_two_tone_glyph_choice() {
        let image = colorful(80, 60);
        let glyphs = |bg| -> String {
            build_grid(
                &image,
                &ConvertSettings {
                    bg,
                    ..settings(30, true)
                },
            )
            .grid
            .cells
            .iter()
            .map(|cell| cell.glyph)
            .collect()
        };
        assert_ne!(glyphs(Some([0, 0, 0])), glyphs(Some([230, 230, 230])));
    }

    #[test]
    fn convert_bytes_rejects_non_images() {
        assert!(convert_bytes(b"definitely not an image", &settings(10, true)).is_err());
    }

    #[test]
    fn convert_bytes_decodes_encoded_images() {
        let mut png = Vec::new();
        gradient(40, 20)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let conversion = convert_bytes(&png, &settings(20, false)).unwrap();
        assert_eq!((conversion.grid.width, conversion.grid.height), (20, 5));
    }

    // --- ANSI rendering ---

    fn red_hash() -> Cell {
        Cell {
            glyph: '#',
            color: [255, 0, 0],
            shade: [80, 0, 0],
        }
    }

    #[test]
    fn mono_ansi_is_plain_text() {
        let out = render_ansi(&uniform_grid(3, 2, red_hash), true, true, [0, 0, 0]);
        assert_eq!(out, "###\n###\n");
    }

    #[test]
    fn ansi_emits_color_only_when_it_changes() {
        let out = render_ansi(&uniform_grid(4, 2, red_hash), false, false, [0, 0, 0]);
        assert_eq!(out, "\x1b[38;2;255;0;0m####\x1b[0m\n".repeat(2));
    }

    #[test]
    fn ansi_cell_backgrounds_use_blended_shade() {
        let grid = uniform_grid(2, 1, red_hash);
        let base = [0, 0, 0];
        let [r, g, b] = cell_background(&grid.cells[0], base);
        let out = render_ansi(&grid, false, true, base);
        assert_eq!(
            out,
            format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;255;0;0m##\x1b[0m\n")
        );
    }

    #[test]
    fn ansi_rows_break_and_reset_per_row() {
        let grid = Grid {
            width: 2,
            height: 2,
            cells: vec![
                red_hash(),
                Cell {
                    glyph: '.',
                    color: [0, 0, 255],
                    shade: [0, 0, 255],
                },
                red_hash(),
                red_hash(),
            ],
        };
        let out = render_ansi(&grid, false, false, [0, 0, 0]);
        assert_eq!(
            out,
            "\x1b[38;2;255;0;0m#\x1b[38;2;0;0;255m.\x1b[0m\n\x1b[38;2;255;0;0m##\x1b[0m\n"
        );
    }

    // --- image rendering ---

    fn blank() -> Cell {
        Cell {
            glyph: ' ',
            color: [255, 255, 255],
            shade: [200, 0, 0],
        }
    }

    #[test]
    fn image_size_is_cells_times_cell_pixels() {
        let image = render_image(
            &uniform_grid(7, 3, red_hash),
            false,
            Background::Opaque([0, 0, 0]),
            true,
        );
        assert_eq!(
            image.dimensions(),
            (7 * CELL_PIXEL_WIDTH, 3 * CELL_PIXEL_HEIGHT)
        );
    }

    #[test]
    fn opaque_background_renders_rgb_and_translucent_renders_rgba() {
        let grid = uniform_grid(2, 2, red_hash);
        assert!(matches!(
            render_image(&grid, false, Background::Opaque([0, 0, 0]), true),
            DynamicImage::ImageRgb8(_)
        ));
        assert!(matches!(
            render_image(
                &grid,
                false,
                Background::Translucent {
                    color: [0, 0, 0],
                    alpha: 0
                },
                false
            ),
            DynamicImage::ImageRgba8(_)
        ));
    }

    #[test]
    fn blank_cells_show_the_flat_background() {
        let image = render_image(
            &uniform_grid(2, 2, blank),
            false,
            Background::Opaque([10, 20, 30]),
            false,
        )
        .to_rgb8();
        assert!(image.pixels().all(|p| p.0 == [10, 20, 30]));
    }

    #[test]
    fn blank_cells_become_solid_tiles_with_cell_backgrounds() {
        let base = [10, 20, 30];
        let expected = cell_background(&blank(), base);
        let image = render_image(
            &uniform_grid(2, 2, blank),
            false,
            Background::Opaque(base),
            true,
        )
        .to_rgb8();
        assert!(image.pixels().all(|p| p.0 == expected));
    }

    #[test]
    fn translucent_background_keeps_its_alpha_behind_glyphs() {
        let image = render_image(
            &uniform_grid(1, 1, red_hash),
            false,
            Background::Translucent {
                color: [0, 0, 0],
                alpha: 80,
            },
            true,
        )
        .to_rgba8();
        // A corner pixel is outside the glyph's ink.
        assert_eq!(image.get_pixel(0, 0)[3], 80);
        // Glyph ink is drawn at the glyph's own opacity, not the
        // backdrop's (anti-aliasing tops out just shy of 255).
        assert!(image.pixels().any(|p| p[3] >= 250 && p[0] > 200));
    }

    #[test]
    fn fully_transparent_background_leaves_only_glyphs() {
        let image = render_image(
            &uniform_grid(2, 1, blank),
            false,
            Background::Translucent {
                color: [0, 0, 0],
                alpha: 0,
            },
            false,
        )
        .to_rgba8();
        assert!(image.pixels().all(|p| p[3] == 0));
    }

    #[test]
    fn mono_images_draw_white_glyphs_and_skip_cell_backgrounds() {
        let image = render_image(
            &uniform_grid(1, 1, red_hash),
            true,
            Background::Opaque([0, 0, 0]),
            true,
        )
        .to_rgb8();
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0]);
        assert!(image.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));
        assert!(image.pixels().any(|p| p[0] > 200));
    }

    #[test]
    #[should_panic]
    fn renderers_reject_grids_with_too_few_cells() {
        let grid = Grid {
            width: 3,
            height: 3,
            cells: vec![red_hash()],
        };
        render_ansi(&grid, false, false, [0, 0, 0]);
    }
}
