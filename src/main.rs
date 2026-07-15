mod pipeline;

use clap::{Args, Parser, Subcommand};
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
    },
    /// Convert an existing image file to ASCII art.
    Convert {
        /// Path to the source image.
        image_path: PathBuf,
        #[command(flatten)]
        options: ConvertOptions,
    },
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
        Command::Generate { prompt, options } => {
            eprintln!("generate: prompt={:?} width={} out={:?} mono={}", prompt, options.width, options.out, options.mono);
            eprintln!("not yet implemented");
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
