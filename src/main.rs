mod openai;
mod pipeline;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "halftone", about = "Turn images into ANSI/ASCII splash art")]
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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Generate { prompt, options, size, save_image } => {
            let bytes = match openai::generate_image(&prompt, size.as_openai_size()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
            };

            if let Some(path) = &save_image {
                match std::fs::write(path, &bytes) {
                    Ok(()) => eprintln!("Saved raw image to {}", path.display()),
                    Err(err) => eprintln!("failed to save generated image to {}: {}", path.display(), err),
                }
            }

            eprintln!("Converting to ASCII art...");
            match pipeline::convert_bytes(&bytes, options.width, options.mono) {
                Ok(art) => write_output(&art, options.out.as_deref()),
                Err(err) => {
                    eprintln!("failed to convert generated image: {err}");
                    std::process::exit(1);
                }
            }
        }
        Command::Convert { image_path, options } => {
            match pipeline::convert_image(&image_path, options.width, options.mono) {
                Ok(art) => write_output(&art, options.out.as_deref()),
                Err(err) => {
                    eprintln!("failed to convert {}: {}", image_path.display(), err);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn write_output(art: &str, out: Option<&Path>) {
    match out {
        Some(path) => {
            if let Err(err) = std::fs::write(path, art) {
                eprintln!("failed to write {}: {}", path.display(), err);
                std::process::exit(1);
            }
        }
        None => print!("{art}"),
    }
}
