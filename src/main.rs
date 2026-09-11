mod openai;
mod pipeline;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "halftone",
    about = "Turn images into ANSI/ASCII splash art",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate an image from a text prompt, then convert it to ASCII art.
    Generate {
        /// The prompt describing the image to generate.
        prompt: String,
        #[command(flatten)]
        options: ConvertOptions,
        /// Aspect ratio of the generated image.
        #[arg(long, value_enum, default_value = "square")]
        size: ImageShape,
        /// Also save the raw generated image (before ASCII conversion) to this path.
        #[arg(long)]
        save_image: Option<PathBuf>,
        /// Generate this many images from the same prompt in one request.
        /// When more than 1, `--out`/`--save-image` paths get a `-1`, `-2`,
        /// ... suffix inserted before the extension so each image lands at
        /// its own path instead of overwriting the last one.
        #[arg(short = 'n', long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10))]
        count: u32,
    },
    /// Convert an existing image file to ASCII art.
    Convert {
        /// Path to the source image.
        image_path: PathBuf,
        #[command(flatten)]
        options: ConvertOptions,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ImageShape {
    Square,
    Landscape,
    Portrait,
}

impl ImageShape {
    /// gpt-image-1 only accepts these three exact size strings (dall-e-3's
    /// 1792x1024/1024x1792 no longer apply now that it's been shut down).
    fn as_openai_size(self) -> &'static str {
        match self {
            ImageShape::Square => "1024x1024",
            ImageShape::Landscape => "1536x1024",
            ImageShape::Portrait => "1024x1536",
        }
    }
}

#[derive(Args)]
struct ConvertOptions {
    /// Output width, in characters.
    #[arg(long, default_value_t = 80)]
    width: u32,

    /// Write output to this file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Emit plain monochrome ASCII instead of ANSI color.
    #[arg(long)]
    mono: bool,

    /// Background color for image (`--out foo.png`) exports: `auto` to derive
    /// a dark background tinted toward the image's own average color, or a
    /// hex color like `1e2b30` / `#1e2b30`. Has no effect on ANSI/text
    /// output, which takes its background from the terminal it's viewed in.
    #[arg(long, default_value = "auto")]
    bg: String,
}

/// Resolves the `--bg` value against a converted grid: `auto` derives a
/// background from the grid's own average color, anything else is parsed as
/// a `RRGGBB` (optionally `#`-prefixed) hex color.
fn resolve_bg(bg: &str, grid: &pipeline::Grid) -> Result<[u8; 3], String> {
    if bg.eq_ignore_ascii_case("auto") {
        return Ok(pipeline::auto_background(grid));
    }

    let hex = bg.strip_prefix('#').unwrap_or(bg);
    if hex.len() != 6 {
        return Err(format!(
            "invalid color '{bg}': expected 'auto' or 6 hex digits, like 1e2b30"
        ));
    }
    let channel = |range| {
        u8::from_str_radix(&hex[range], 16)
            .map_err(|_| format!("invalid color '{bg}': not valid hex"))
    };
    Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?])
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Generate {
            prompt,
            options,
            size,
            save_image,
            count,
        } => {
            let images = match openai::generate_images(&prompt, size.as_openai_size(), count) {
                Ok(images) => images,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
            };
            let total = images.len();

            for (i, bytes) in images.into_iter().enumerate() {
                if total > 1 {
                    eprintln!("--- Image {} of {total} ---", i + 1);
                }

                if let Some(path) = &save_image {
                    let path = indexed_path(path, i, total);
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => eprintln!("Saved raw image to {}", path.display()),
                        Err(err) => eprintln!(
                            "failed to save generated image to {}: {}",
                            path.display(),
                            err
                        ),
                    }
                }

                eprintln!("Converting to ASCII art...");
                match pipeline::convert_bytes(&bytes, options.width) {
                    Ok(grid) => {
                        let out = options.out.as_deref().map(|p| indexed_path(p, i, total));
                        write_output(&grid, options.mono, &options.bg, out.as_deref())
                    }
                    Err(err) => {
                        eprintln!("failed to convert generated image: {err}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Convert {
            image_path,
            options,
        } => match pipeline::convert_image(&image_path, options.width) {
            Ok(grid) => write_output(&grid, options.mono, &options.bg, options.out.as_deref()),
            Err(err) => {
                eprintln!("failed to convert {}: {}", image_path.display(), err);
                std::process::exit(1);
            }
        },
    }
}

/// Inserts a `-{n}` (1-based) suffix before `path`'s extension so a batch of
/// `--count` images each get their own file instead of every image after the
/// first overwriting the last. A single-image run (`total <= 1`) returns
/// `path` unchanged, preserving today's filenames for existing scripts.
fn indexed_path(path: &Path, index: usize, total: usize) -> PathBuf {
    if total <= 1 {
        return path.to_path_buf();
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let mut name = format!("{stem}-{}", index + 1);
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        name.push('.');
        name.push_str(ext);
    }
    path.with_file_name(name)
}

/// Extensions recognized as "render this as a raster image" for `--out`.
/// Anything else is treated as a text destination for the ANSI/ASCII art.
fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("bmp") | Some("tiff") | Some("webp")
    )
}

fn write_output(grid: &pipeline::Grid, mono: bool, bg: &str, out: Option<&Path>) {
    let art = pipeline::render_ansi(grid, mono);
    print!("{art}");

    let Some(path) = out else { return };

    if is_image_path(path) {
        let bg = resolve_bg(bg, grid).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });
        let image = pipeline::render_image(grid, mono, bg);
        if let Err(err) = image.save(path) {
            eprintln!("failed to write {}: {}", path.display(), err);
            std::process::exit(1);
        }
    } else if let Err(err) = std::fs::write(path, &art) {
        eprintln!("failed to write {}: {}", path.display(), err);
        std::process::exit(1);
    }
}
