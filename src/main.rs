use clap::{Args, Parser, Subcommand};
#[cfg(feature = "generate")]
use halftone::openai;
use halftone::pipeline;
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
    #[cfg(feature = "generate")]
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
        /// An image to work from -- a sketch, a layout, a photo -- which the
        /// model repaints following the prompt, keeping its composition.
        #[arg(long)]
        reference: Option<PathBuf>,
        /// OpenAI image model.
        #[arg(long, default_value = openai::DEFAULT_MODEL)]
        model: String,
        /// Image quality. Higher costs more; the gpt-image-2.5 models also
        /// accept `xhigh` and `max`.
        #[arg(long, default_value = "low", value_parser = ["low", "medium", "high", "xhigh", "max", "auto"])]
        quality: String,
    },
    /// Convert an existing image file to ASCII art.
    Convert {
        /// Path to the source image.
        image_path: PathBuf,
        #[command(flatten)]
        options: ConvertOptions,
    },
}

#[cfg(feature = "generate")]
#[derive(Clone, Copy, clap::ValueEnum)]
enum ImageShape {
    Square,
    Landscape,
    Portrait,
}

#[cfg(feature = "generate")]
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
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,

    /// Write output to this file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Emit plain monochrome ASCII instead of ANSI color.
    #[arg(long)]
    mono: bool,

    /// Backdrop color every cell's background is blended onto (or, with
    /// `--flat-bg`, the one flat background): `auto` to derive a dark color
    /// tinted toward the image's own average color, a hex color like
    /// `1e2b30` / `#1e2b30`, or `transparent` for no backdrop at all -- only
    /// the glyphs are drawn, over a transparent PNG or the terminal's own
    /// background (implies `--flat-bg`). Append `@<0-255>` to `auto` or a
    /// hex color (e.g. `auto@128`, `1e2b30@80`) for a semi-transparent
    /// backdrop in PNG exports, so whatever the art gets composited onto
    /// shows through; terminals have no opacity, so ANSI output uses the
    /// color as-is. Anything other than full opacity is PNG output only. In
    /// the terminal, `--flat-bg` leaves the background to the terminal.
    #[arg(long, default_value = "auto")]
    bg: String,

    /// Use one flat background color behind everything instead of giving
    /// each cell its own. By default each cell is split into its two main
    /// colors -- the glyph takes the lighter one, the background a dimmed
    /// version of the darker one -- and glyphs are picked by the color the
    /// cell actually ends up showing, with strong edges getting shape-matched
    /// glyphs like `/`, `_`, `|`. This turns all of that off for the simpler
    /// single-color-per-cell look. Implied by `--mono` and `--bg transparent`.
    #[arg(long)]
    flat_bg: bool,

    /// How brightness and color are mapped: `equalized` stretches the
    /// image's own brightness range and boosts color, which rescues dim or
    /// moody images; `natural` keeps the source's own brightness and colors,
    /// which reads better for bright, even artwork like maps.
    #[arg(long, value_enum, default_value = "equalized")]
    tone: ToneArg,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ToneArg {
    Equalized,
    Natural,
}

impl From<ToneArg> for pipeline::Tone {
    fn from(tone: ToneArg) -> Self {
        match tone {
            ToneArg::Equalized => pipeline::Tone::Equalized,
            ToneArg::Natural => pipeline::Tone::Natural,
        }
    }
}

/// A parsed `--bg` value: the color (`None` for `auto`, which can only be
/// worked out from the converted image) and its opacity.
struct BgSpec {
    color: Option<[u8; 3]>,
    alpha: u8,
}

/// Parses the `--bg` value. The color part is `auto` (derived from the grid's
/// own average color) or a `RRGGBB` (optionally `#`-prefixed) hex color;
/// `transparent` is shorthand for black at zero opacity. An optional
/// `@<0-255>` suffix on `auto`/hex picks the opacity directly, so a background
/// can sit anywhere between fully opaque and fully invisible instead of only
/// those two extremes. Parsed before conversion rather than after, since
/// per-cell backgrounds need an explicit color to pick glyphs against.
fn parse_bg(bg: &str) -> Result<BgSpec, String> {
    if bg.eq_ignore_ascii_case("transparent") {
        return Ok(BgSpec {
            color: Some([0, 0, 0]),
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
        None
    } else {
        let hex = spec.strip_prefix('#').unwrap_or(spec);
        if hex.len() != 6 {
            return Err(format!(
                "invalid color '{spec}': expected 'auto', 'transparent', or 6 hex digits, like 1e2b30"
            ));
        }
        // Checked up front rather than left to `from_str_radix`, which
        // accepts a leading `+` (so `+1+2+3` would parse), and because
        // slicing by byte range below panics on a non-ASCII character.
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid color '{spec}': not valid hex"));
        }
        let channel =
            |range| u8::from_str_radix(&hex[range], 16).expect("validated as ASCII hex above");
        Some([channel(0..2), channel(2..4), channel(4..6)])
    };

    Ok(BgSpec { color, alpha })
}

/// Resolves a parsed `--bg` against a converted image, filling in `auto`.
fn resolve_bg(bg: &BgSpec, auto_bg: [u8; 3]) -> pipeline::Background {
    let color = bg.color.unwrap_or(auto_bg);
    if bg.alpha == 255 {
        pipeline::Background::Opaque(color)
    } else {
        pipeline::Background::Translucent {
            color,
            alpha: bg.alpha,
        }
    }
}

impl ConvertOptions {
    fn bg_spec(&self) -> BgSpec {
        parse_bg(&self.bg).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        })
    }

    /// Whether cells get their own backgrounds (the default). Not for
    /// `--mono`, which has no color to put in them, nor for a fully
    /// transparent `--bg`, which would render them invisible while glyphs
    /// were still picked as if they showed -- a transparent export keeps
    /// the flat look instead. A semi-transparent `--bg` keeps them, at its
    /// opacity.
    fn cell_bg(&self) -> bool {
        !self.flat_bg && !self.mono && self.bg_spec().alpha != 0
    }

    fn settings(&self) -> pipeline::ConvertSettings {
        pipeline::ConvertSettings {
            width: self.width,
            cell_bg: self.cell_bg(),
            bg: self.bg_spec().color,
            tone: self.tone.into(),
        }
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
#[cfg(feature = "generate")]
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
        #[cfg(feature = "generate")]
        Command::Generate {
            prompt,
            prompt_file,
            options,
            size,
            save_image,
            count,
            reference,
            model,
            quality,
        } => {
            let prompt = match resolve_prompt(prompt, prompt_file.as_deref()) {
                Ok(prompt) => prompt,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
            };
            let reference = match reference.as_deref().map(std::fs::read).transpose() {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!("failed to read the reference image: {err}");
                    std::process::exit(1);
                }
            };
            let request = openai::Options {
                model: &model,
                size: size.as_openai_size(),
                quality: &quality,
                count,
                reference: reference.as_deref(),
                extra_references: &[],
            };
            let images = match openai::generate(&prompt, &request) {
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
                match pipeline::convert_bytes(&bytes, &options.settings()) {
                    Ok(conversion) => {
                        let out = options.out.as_deref().map(|p| indexed_path(p, i, total));
                        write_output(&conversion, &options, out.as_deref())
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
        } => match pipeline::convert_image(&image_path, &options.settings()) {
            Ok(conversion) => write_output(&conversion, &options, options.out.as_deref()),
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
#[cfg(feature = "generate")]
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

fn write_output(conversion: &pipeline::Conversion, options: &ConvertOptions, out: Option<&Path>) {
    let grid = &conversion.grid;
    let mono = options.mono;
    let bg = resolve_bg(&options.bg_spec(), conversion.auto_bg);
    let base = match bg {
        pipeline::Background::Opaque(color) | pipeline::Background::Translucent { color, .. } => {
            color
        }
    };
    let art = pipeline::render_ansi(grid, mono, options.cell_bg(), base);
    print!("{art}");

    let Some(path) = out else { return };

    if is_image_path(path) {
        if matches!(bg, pipeline::Background::Translucent { .. }) && !supports_alpha(path) {
            eprintln!(
                "a non-opaque `--bg` needs an alpha channel, which {} doesn't support here -- use a `.png` path instead.",
                path.display()
            );
            std::process::exit(1);
        }
        let image = pipeline::render_image(grid, mono, bg, options.cell_bg());
        if let Err(err) = image.save(path) {
            eprintln!("failed to write {}: {}", path.display(), err);
            std::process::exit(1);
        }
    } else if let Err(err) = std::fs::write(path, &art) {
        eprintln!("failed to write {}: {}", path.display(), err);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn convert_options(args: &[&str]) -> ConvertOptions {
        let cli = Cli::try_parse_from(
            ["halftone", "convert", "image.png"]
                .iter()
                .chain(args)
                .copied(),
        )
        .unwrap();
        match cli.command {
            Command::Convert { options, .. } => options,
            #[cfg(feature = "generate")]
            _ => unreachable!(),
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    // --- --bg parsing ---

    fn bg(value: &str) -> (Option<[u8; 3]>, u8) {
        let spec = parse_bg(value).unwrap();
        (spec.color, spec.alpha)
    }

    #[test]
    fn bg_auto_and_transparent() {
        assert_eq!(bg("auto"), (None, 255));
        assert_eq!(bg("AUTO"), (None, 255));
        assert_eq!(bg("transparent"), (Some([0, 0, 0]), 0));
        assert_eq!(bg("Transparent"), (Some([0, 0, 0]), 0));
    }

    #[test]
    fn bg_hex_colors() {
        assert_eq!(bg("1e2b30"), (Some([0x1e, 0x2b, 0x30]), 255));
        assert_eq!(bg("#1e2b30"), (Some([0x1e, 0x2b, 0x30]), 255));
        assert_eq!(bg("FFaa00"), (Some([0xff, 0xaa, 0x00]), 255));
    }

    #[test]
    fn bg_alpha_suffix() {
        assert_eq!(bg("auto@128"), (None, 128));
        assert_eq!(bg("1e2b30@0"), (Some([0x1e, 0x2b, 0x30]), 0));
        assert_eq!(bg("#1e2b30@255"), (Some([0x1e, 0x2b, 0x30]), 255));
    }

    #[test]
    fn bg_rejects_malformed_values() {
        for value in [
            "",
            "black",
            "1e2b3",
            "1e2b300",
            "#12345",
            "zzzzzz",
            "+1+2+3",
            "aé123",
            "ééé",
            "auto@",
            "auto@256",
            "auto@-1",
            "auto@x",
            "1e2b30@80@80",
            "transparent@80",
        ] {
            assert!(parse_bg(value).is_err(), "{value:?} should be rejected");
        }
    }

    #[test]
    fn resolve_bg_fills_in_auto_and_picks_opacity() {
        let auto = [1, 2, 3];
        assert!(matches!(
            resolve_bg(&parse_bg("auto").unwrap(), auto),
            pipeline::Background::Opaque([1, 2, 3])
        ));
        assert!(matches!(
            resolve_bg(&parse_bg("aabbcc").unwrap(), auto),
            pipeline::Background::Opaque([0xaa, 0xbb, 0xcc])
        ));
        assert!(matches!(
            resolve_bg(&parse_bg("auto@10").unwrap(), auto),
            pipeline::Background::Translucent {
                color: [1, 2, 3],
                alpha: 10
            }
        ));
    }

    // --- option interplay ---

    #[test]
    fn cell_backgrounds_are_on_by_default() {
        assert!(convert_options(&[]).cell_bg());
        assert!(convert_options(&["--bg", "1e2b30"]).cell_bg());
        assert!(convert_options(&["--bg", "auto@128"]).cell_bg());
    }

    #[test]
    fn cell_backgrounds_turn_off_for_flat_mono_and_transparent() {
        assert!(!convert_options(&["--flat-bg"]).cell_bg());
        assert!(!convert_options(&["--mono"]).cell_bg());
        assert!(!convert_options(&["--bg", "transparent"]).cell_bg());
        assert!(!convert_options(&["--bg", "auto@0"]).cell_bg());
    }

    #[test]
    fn settings_carry_width_and_explicit_bg() {
        let settings = convert_options(&["--width", "42", "--bg", "#102030"]).settings();
        assert_eq!(settings.width, 42);
        assert_eq!(settings.bg, Some([0x10, 0x20, 0x30]));
        assert!(settings.cell_bg);
        assert_eq!(convert_options(&[]).settings().bg, None);
    }

    // --- output paths ---

    #[test]
    fn image_paths_by_extension() {
        for path in [
            "a.png", "a.PNG", "a.jpg", "a.jpeg", "a.bmp", "a.tiff", "a.webp",
        ] {
            assert!(is_image_path(Path::new(path)), "{path}");
        }
        for path in ["a.txt", "a.ans", "a", "png"] {
            assert!(!is_image_path(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn only_png_supports_alpha() {
        assert!(supports_alpha(Path::new("a.png")));
        assert!(supports_alpha(Path::new("a.Png")));
        assert!(!supports_alpha(Path::new("a.jpg")));
        assert!(!supports_alpha(Path::new("a.webp")));
        assert!(!supports_alpha(Path::new("a")));
    }

    #[cfg(feature = "generate")]
    #[test]
    fn indexed_path_only_suffixes_batches() {
        let path = Path::new("out/art.png");
        assert_eq!(indexed_path(path, 0, 1), PathBuf::from("out/art.png"));
        assert_eq!(indexed_path(path, 0, 3), PathBuf::from("out/art-1.png"));
        assert_eq!(indexed_path(path, 2, 3), PathBuf::from("out/art-3.png"));
        assert_eq!(indexed_path(Path::new("art"), 1, 2), PathBuf::from("art-2"));
        assert_eq!(
            indexed_path(Path::new("art.tar.gz"), 0, 2),
            PathBuf::from("art.tar-1.gz")
        );
    }

    // --- generate ---

    #[cfg(feature = "generate")]
    fn generate_args(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(["halftone", "generate"].iter().chain(args).copied())
    }

    #[cfg(feature = "generate")]
    #[test]
    fn generate_needs_exactly_one_prompt_source() {
        assert!(generate_args(&["a dragon"]).is_ok());
        assert!(generate_args(&["--prompt-file", "p.txt"]).is_ok());
        assert!(generate_args(&[]).is_err());
        assert!(generate_args(&["a dragon", "--prompt-file", "p.txt"]).is_err());
    }

    #[cfg(feature = "generate")]
    #[test]
    fn generate_takes_a_reference_model_and_quality() {
        let Command::Generate {
            reference,
            model,
            quality,
            ..
        } = generate_args(&[
            "x",
            "--reference",
            "sketch.png",
            "--model",
            "gpt-image-2.5-sunburst",
            "--quality",
            "high",
        ])
        .unwrap()
        .command
        else {
            unreachable!()
        };
        assert_eq!(reference, Some(PathBuf::from("sketch.png")));
        assert_eq!(model, "gpt-image-2.5-sunburst");
        assert_eq!(quality, "high");
        assert!(generate_args(&["x", "--quality", "ultra"]).is_err());
        let Command::Generate { model, quality, .. } = generate_args(&["x"]).unwrap().command
        else {
            unreachable!()
        };
        assert_eq!((model.as_str(), quality.as_str()), ("gpt-image-1", "low"));
    }

    #[cfg(feature = "generate")]
    #[test]
    fn generate_count_is_bounded() {
        assert!(generate_args(&["x", "-n", "1"]).is_ok());
        assert!(generate_args(&["x", "-n", "10"]).is_ok());
        assert!(generate_args(&["x", "-n", "0"]).is_err());
        assert!(generate_args(&["x", "-n", "11"]).is_err());
    }

    #[cfg(feature = "generate")]
    #[test]
    fn image_shapes_map_to_openai_sizes() {
        assert_eq!(ImageShape::Square.as_openai_size(), "1024x1024");
        assert_eq!(ImageShape::Landscape.as_openai_size(), "1536x1024");
        assert_eq!(ImageShape::Portrait.as_openai_size(), "1024x1536");
    }

    #[cfg(feature = "generate")]
    fn temp_prompt_file(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("halftone-test-{}-{name}.txt", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[cfg(feature = "generate")]
    #[test]
    fn prompt_comes_from_argument_or_trimmed_file() {
        assert_eq!(
            resolve_prompt(Some("a dragon".into()), None).unwrap(),
            "a dragon"
        );
        let path = temp_prompt_file("trimmed", "\n  a castle at dusk\n\n");
        assert_eq!(
            resolve_prompt(None, Some(&path)).unwrap(),
            "a castle at dusk"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(feature = "generate")]
    #[test]
    fn prompt_file_must_exist_and_have_content() {
        let path = temp_prompt_file("blank", " \n\t\n");
        let err = resolve_prompt(None, Some(&path)).unwrap_err();
        assert!(err.contains("is empty"), "{err}");
        std::fs::remove_file(&path).unwrap();

        let err = resolve_prompt(None, Some(&path)).unwrap_err();
        assert!(err.contains("failed to read prompt file"), "{err}");
    }
}
