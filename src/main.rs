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
        /// The prompt describing the image to generate. Omit this and use
        /// `--prompt-file` instead to read the prompt from a file.
        #[arg(
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        prompt: Option<String>,
        /// Read the prompt from this file instead of passing it as an
        /// argument. Any plain text file works (`.txt`, `.md`, etc.);
        /// leading/trailing whitespace is trimmed.
        #[arg(long)]
        prompt_file: Option<PathBuf>,
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
    /// a dark background tinted toward the image's own average color, a hex
    /// color like `1e2b30` / `#1e2b30`, or `transparent` to leave everything
    /// but the glyphs transparent. Append `@<0-255>` to `auto` or a hex color
    /// (e.g. `auto@128`, `1e2b30@80`) for a semi-transparent tinted
    /// background instead of fully opaque or fully invisible -- a backdrop
    /// still shows through for contrast, without hiding whatever the art
    /// gets composited onto. Anything other than full opacity is PNG output
    /// only. Has no effect on ANSI/text output, which takes its background
    /// from the terminal it's viewed in.
    #[arg(long, default_value = "auto")]
    bg: String,

    /// Give each cell its own background color, blended from the flat
    /// background toward that cell's glyph color by this fraction (0-1),
    /// instead of one flat background behind everything. `0` (the default)
    /// keeps the flat background; around `0.3`-`0.4` fills the gaps between
    /// glyphs with color while they still stand out; higher values push
    /// toward a solid color mosaic. Applies to both ANSI and image output,
    /// and covers `--bg`'s color in images (a translucent `--bg`'s alpha is
    /// kept). Ignored with `--mono`.
    #[arg(long, default_value_t = 0.0, value_parser = parse_unit_interval)]
    cell_bg: f64,
}

fn parse_unit_interval(s: &str) -> Result<f64, String> {
    let value: f64 = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "{value} is out of range: expected a number from 0 to 1"
        ))
    }
}

/// Resolves the `--bg` value against a converted grid. The color part is
/// `auto` (derived from the grid's own average color) or a `RRGGBB`
/// (optionally `#`-prefixed) hex color; `transparent` is shorthand for that
/// color at zero opacity. An optional `@<0-255>` suffix on `auto`/hex picks
/// the opacity directly, so a background can sit anywhere between fully
/// opaque and fully invisible instead of only those two extremes.
fn resolve_bg(bg: &str, grid: &pipeline::Grid) -> Result<pipeline::Background, String> {
    if bg.eq_ignore_ascii_case("transparent") {
        return Ok(pipeline::Background::Translucent {
            color: [0, 0, 0],
            alpha: 0,
        });
    }

    let (spec, alpha) = match bg.split_once('@') {
        Some((spec, alpha_str)) => {
            let alpha: u8 = alpha_str.parse().map_err(|_| {
                format!("invalid alpha '{alpha_str}' in '{bg}': expected a number from 0-255")
            })?;
            (spec, alpha)
        }
        None => (bg, 255),
    };

    let color = if spec.eq_ignore_ascii_case("auto") {
        pipeline::auto_background(grid)
    } else {
        let hex = spec.strip_prefix('#').unwrap_or(spec);
        if hex.len() != 6 {
            return Err(format!(
                "invalid color '{spec}': expected 'auto', 'transparent', or 6 hex digits, like 1e2b30"
            ));
        }
        let channel = |range| {
            u8::from_str_radix(&hex[range], 16)
                .map_err(|_| format!("invalid color '{spec}': not valid hex"))
        };
        [channel(0..2)?, channel(2..4)?, channel(4..6)?]
    };

    if alpha == 255 {
        Ok(pipeline::Background::Opaque(color))
    } else {
        Ok(pipeline::Background::Translucent { color, alpha })
    }
}

/// Whether `path`'s extension supports an alpha channel, for gating any
/// non-fully-opaque `--bg`. Only PNG among today's `is_image_path` formats
/// does in the way this tool writes files -- JPEG has no alpha at all, and
/// BMP/TIFF support from the `image` crate's encoders isn't guaranteed here,
/// so transparency sticks to the one format guaranteed to round-trip it
/// correctly rather than silently flattening to black/white on the others.
fn supports_alpha(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png")
    )
}

/// Resolves the effective prompt text for `generate`: either the positional
/// `prompt` argument or the contents of `prompt_file`, whichever was given
/// (clap's `conflicts_with`/`required_unless_present` guarantee exactly one
/// is `Some`). File contents are trimmed since editors routinely add a
/// trailing newline that shouldn't become part of the prompt.
fn resolve_prompt(prompt: Option<String>, prompt_file: Option<&Path>) -> Result<String, String> {
    if let Some(path) = prompt_file {
        let contents = std::fs::read_to_string(path)
            .map_err(|err| format!("failed to read prompt file {}: {}", path.display(), err))?;
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return Err(format!("prompt file {} is empty", path.display()));
        }
        Ok(trimmed.to_string())
    } else {
        Ok(prompt.expect("clap guarantees prompt or prompt_file is set"))
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Generate {
            prompt,
            prompt_file,
            options,
            size,
            save_image,
            count,
        } => {
            let prompt = match resolve_prompt(prompt, prompt_file.as_deref()) {
                Ok(prompt) => prompt,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
            };
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
                        write_output(&grid, &options, out.as_deref())
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
            Ok(grid) => write_output(&grid, &options, options.out.as_deref()),
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

fn write_output(grid: &pipeline::Grid, options: &ConvertOptions, out: Option<&Path>) {
    let mono = options.mono;
    let art = pipeline::render_ansi(grid, mono, options.cell_bg);
    print!("{art}");

    let Some(path) = out else { return };

    if is_image_path(path) {
        let bg = resolve_bg(&options.bg, grid).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });
        if matches!(bg, pipeline::Background::Translucent { .. }) && !supports_alpha(path) {
            eprintln!(
                "a non-opaque `--bg` needs an alpha channel, which {} doesn't support here -- use a `.png` path instead.",
                path.display()
            );
            std::process::exit(1);
        }
        let image = pipeline::render_image(grid, mono, bg, options.cell_bg);
        if let Err(err) = image.save(path) {
            eprintln!("failed to write {}: {}", path.display(), err);
            std::process::exit(1);
        }
    } else if let Err(err) = std::fs::write(path, &art) {
        eprintln!("failed to write {}: {}", path.display(), err);
        std::process::exit(1);
    }
}
