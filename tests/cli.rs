//! End-to-end tests that run the built `halftone` binary: flag handling,
//! output files, exit codes, and error messages.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/source.png");

fn halftone(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_halftone"))
        .args(args)
        // Keeps `generate` tests from ever reaching the real API.
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// A per-test scratch directory, emptied at the start of each run.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn convert_prints_ansi_art() {
    let output = halftone(&["convert", FIXTURE, "--width", "30"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let art = stdout(&output);
    // The fixture is square, so 30 columns is 15 rows at the 2:1 cell ratio.
    assert_eq!(art.lines().count(), 15);
    assert!(art.contains("\x1b[38;2;"), "no foreground colors");
    assert!(art.contains("\x1b[48;2;"), "no cell backgrounds by default");
}

#[test]
fn width_defaults_to_80() {
    let output = halftone(&["convert", FIXTURE, "--mono"]);
    let art = stdout(&output);
    assert_eq!(art.lines().count(), 40);
    assert!(art.lines().all(|line| line.chars().count() == 80));
}

#[test]
fn mono_output_has_no_escape_codes() {
    let output = halftone(&["convert", FIXTURE, "--width", "30", "--mono"]);
    assert!(output.status.success());
    assert!(!stdout(&output).contains('\x1b'));
}

#[test]
fn flat_bg_drops_cell_backgrounds() {
    let output = halftone(&["convert", FIXTURE, "--width", "30", "--flat-bg"]);
    assert!(output.status.success());
    let art = stdout(&output);
    assert!(art.contains("\x1b[38;2;"));
    assert!(!art.contains("\x1b[48;2;"));
}

#[test]
fn out_txt_writes_the_same_art_as_stdout() {
    let dir = scratch("out_txt");
    let path = dir.join("art.txt");
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "30",
        "--out",
        path_str(&path),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), stdout(&output));
}

#[test]
fn out_png_writes_an_image_sized_by_cells() {
    let dir = scratch("out_png");
    let path = dir.join("art.png");
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "30",
        "--out",
        path_str(&path),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let image = image::open(&path).unwrap();
    assert_eq!((image.width(), image.height()), (30 * 10, 15 * 20));
    assert!(matches!(image, image::DynamicImage::ImageRgb8(_)));
}

#[test]
fn out_image_extension_is_case_insensitive() {
    let dir = scratch("out_upper");
    let path = dir.join("ART.PNG");
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "10",
        "--out",
        path_str(&path),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(image::open(&path).is_ok(), "wasn't written as an image");
}

#[test]
fn transparent_png_has_an_alpha_channel() {
    let dir = scratch("transparent_png");
    let path = dir.join("art.png");
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "20",
        "--bg",
        "transparent",
        "--out",
        path_str(&path),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let image = image::open(&path).unwrap().to_rgba8();
    assert!(image.pixels().any(|p| p[3] == 0), "nothing transparent");
    assert!(image.pixels().any(|p| p[3] > 0), "nothing drawn");
}

#[test]
fn translucent_bg_rejects_formats_without_alpha() {
    let dir = scratch("translucent_jpg");
    let path = dir.join("art.jpg");
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "20",
        "--bg",
        "auto@80",
        "--out",
        path_str(&path),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains(".png"), "{}", stderr(&output));
    assert!(!path.exists());
}

#[test]
fn invalid_bg_is_an_error_not_a_panic() {
    for bg in ["nope", "aé123", "+1+2+3", "auto@300"] {
        let output = halftone(&["convert", FIXTURE, "--bg", bg]);
        assert_eq!(output.status.code(), Some(1), "--bg {bg}");
        let err = stderr(&output);
        assert!(!err.contains("panicked"), "--bg {bg}: {err}");
        assert!(err.contains("invalid"), "--bg {bg}: {err}");
    }
}

#[test]
fn missing_input_file_is_reported() {
    let output = halftone(&["convert", "does/not/exist.png"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("failed to convert does/not/exist.png"));
}

#[test]
fn non_image_input_is_reported() {
    let output = halftone(&[
        "convert",
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("failed to convert"));
}

#[test]
fn unwritable_output_is_reported() {
    let output = halftone(&[
        "convert",
        FIXTURE,
        "--width",
        "10",
        "--out",
        "does/not/exist/art.png",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("failed to write"));
}

#[test]
fn zero_width_is_rejected() {
    let output = halftone(&["convert", FIXTURE, "--width", "0"]);
    assert!(!output.status.success());
    assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
}

#[cfg(feature = "generate")]
mod generate {
    use super::*;

    #[test]
    fn needs_an_api_key() {
        let output = halftone(&["generate", "a dragon"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("OPENAI_API_KEY"));
    }

    #[test]
    fn empty_prompt_file_is_rejected_before_calling_the_api() {
        let dir = scratch("empty_prompt");
        let path = dir.join("prompt.txt");
        std::fs::write(&path, "\n\n").unwrap();
        let output = halftone(&["generate", "--prompt-file", path_str(&path)]);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("is empty"));
    }
}

#[cfg(not(feature = "generate"))]
#[test]
fn generate_is_absent_without_the_feature() {
    let output = halftone(&["generate", "a dragon"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unrecognized subcommand"));
}
