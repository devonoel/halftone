use ab_glyph::{FontRef, PxScale};
use image::{DynamicImage, GenericImageView, Rgb, RgbImage, imageops::FilterType};
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

// JetBrains Mono, bundled under the SIL OFL 1.1 (see assets/JetBrainsMono-OFL.txt),
// so `--out foo.png` doesn't depend on whatever fonts happen to be on the
// machine building or running the binary.
const FONT_BYTES: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");

// Pixel size of one character cell when rasterizing to an image. Kept at the
// same 2:1 height:width ratio as CELL_ASPECT_RATIO above, for the same reason.
const CELL_PIXEL_WIDTH: u32 = 10;
const CELL_PIXEL_HEIGHT: u32 = 20;

pub struct Cell {
    pub glyph: char,
    pub color: [u8; 3],
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
            cells.push(Cell {
                glyph,
                color: [pixel[0], pixel[1], pixel[2]],
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
const AUTO_BG_LUMINANCE: f64 = 28.0;

/// Derives a dark background color tinted toward the image's own average
/// color (its overall "temperature"), instead of defaulting to flat black
/// regardless of source. The average hue/saturation is preserved and just
/// scaled down to a dark target luminance.
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

    let scale = AUTO_BG_LUMINANCE / luminance;
    let channel = |v: f64| (v * scale).round().clamp(0.0, 255.0) as u8;
    [channel(r), channel(g), channel(b)]
}

// Renders the same glyph grid to a raster image, so ASCII/ANSI art can be
// shared as a PNG/JPG rather than just pasted as text. Cell positions are
// placed directly from grid coordinates instead of running the font's own
// text layout, since we want a fixed-size monospace grid regardless of this
// particular font's own advance-width metrics.
pub fn render_image(grid: &Grid, mono: bool, bg: [u8; 3]) -> RgbImage {
    let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled font is valid");
    let scale = PxScale::from(CELL_PIXEL_HEIGHT as f32);

    let img_width = grid.width * CELL_PIXEL_WIDTH;
    let img_height = grid.height * CELL_PIXEL_HEIGHT;
    let mut canvas = RgbImage::from_pixel(img_width, img_height, Rgb(bg));

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

    canvas
}
