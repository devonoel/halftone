use image::{imageops::FilterType, DynamicImage, GenericImageView};
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

pub fn convert_image(path: &Path, width: u32, mono: bool) -> Result<String, image::ImageError> {
    let img = image::open(path)?;
    Ok(convert(&img, width, mono))
}

pub fn convert_bytes(bytes: &[u8], width: u32, mono: bool) -> Result<String, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    Ok(convert(&img, width, mono))
}

fn convert(img: &DynamicImage, width: u32, mono: bool) -> String {
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
    let mut out = String::with_capacity((width as usize + 1) * height as usize);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let value = levels[i];
            let idx = value.round().clamp(0.0, max_level) as usize;
            let glyph = RAMP[idx] as char;

            // Brightness alone can't tell apart regions that differ in hue
            // but not luminance (green skin vs. tan leather vs. brown wood,
            // say) -- coloring each glyph with its cell's actual sampled
            // color recovers exactly that information instead of throwing
            // it away in the grayscale conversion above.
            if mono {
                out.push(glyph);
            } else {
                let pixel = rgb.get_pixel(x, y);
                out.push_str(&format!(
                    "\x1b[38;2;{};{};{}m{glyph}",
                    pixel[0], pixel[1], pixel[2]
                ));
            }

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
        if !mono {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }

    out
}
