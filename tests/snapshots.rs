//! Golden-output tests: converts a fixed fixture image under each of the main
//! output modes and compares the ANSI text and rendered PNG against the files
//! in `tests/snapshots/`. Any change to conversion or rendering -- intended or
//! not -- shows up here.
//!
//! After an intended change, regenerate the snapshots with
//!
//!     UPDATE_SNAPSHOTS=1 cargo test --test snapshots
//!
//! and look over the new PNGs before committing them. On a mismatch, the
//! actual output is written next to the test binary's temp dir (the failure
//! message says where) so it can be compared with the snapshot.

use halftone::pipeline::{self, Background, ConvertSettings};
use std::path::{Path, PathBuf};

const FIXTURE: &str = "tests/fixtures/source.png";

struct Case {
    name: &'static str,
    width: u32,
    cell_bg: bool,
    bg: Option<[u8; 3]>,
    alpha: u8,
    mono: bool,
}

// These mirror what the CLI flags resolve to (see `ConvertOptions` in
// main.rs), so each snapshot corresponds to a real invocation.
const CASES: &[Case] = &[
    // halftone convert source.png --width 60
    Case {
        name: "default",
        width: 60,
        cell_bg: true,
        bg: None,
        alpha: 255,
        mono: false,
    },
    // --flat-bg
    Case {
        name: "flat",
        width: 60,
        cell_bg: false,
        bg: None,
        alpha: 255,
        mono: false,
    },
    // --mono
    Case {
        name: "mono",
        width: 60,
        cell_bg: false,
        bg: None,
        alpha: 255,
        mono: true,
    },
    // --bg 1e2b30@80
    Case {
        name: "explicit-bg-translucent",
        width: 60,
        cell_bg: true,
        bg: Some([0x1e, 0x2b, 0x30]),
        alpha: 80,
        mono: false,
    },
    // --bg transparent
    Case {
        name: "transparent",
        width: 60,
        cell_bg: false,
        bg: Some([0, 0, 0]),
        alpha: 0,
        mono: false,
    },
    // A narrow render, where every cell covers a lot of source pixels.
    Case {
        name: "narrow",
        width: 16,
        cell_bg: true,
        bg: None,
        alpha: 255,
        mono: false,
    },
];

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

fn updating() -> bool {
    std::env::var_os("UPDATE_SNAPSHOTS").is_some()
}

fn render(case: &Case) -> (String, image::DynamicImage) {
    let conversion = pipeline::convert_image(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE),
        &ConvertSettings {
            width: case.width,
            cell_bg: case.cell_bg,
            bg: case.bg,
        },
    )
    .expect("fixture converts");
    let color = case.bg.unwrap_or(conversion.auto_bg);
    let background = if case.alpha == 255 {
        Background::Opaque(color)
    } else {
        Background::Translucent {
            color,
            alpha: case.alpha,
        }
    };
    let ansi = pipeline::render_ansi(&conversion.grid, case.mono, case.cell_bg, color);
    let png = pipeline::render_image(&conversion.grid, case.mono, background, case.cell_bg);
    (ansi, png)
}

/// Saves `actual` somewhere inspectable and returns a note pointing at it.
fn save_actual(file_name: &str, write: impl FnOnce(&Path)) -> String {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(file_name);
    write(&path);
    format!("actual output written to {}", path.display())
}

#[test]
fn outputs_match_snapshots() {
    let dir = snapshot_dir();
    let mut failures = Vec::new();

    for case in CASES {
        let (ansi, png) = render(case);
        let ansi_path = dir.join(format!("{}.ans", case.name));
        let png_path = dir.join(format!("{}.png", case.name));

        if updating() {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&ansi_path, &ansi).unwrap();
            png.save(&png_path).unwrap();
            continue;
        }

        match std::fs::read_to_string(&ansi_path) {
            Ok(expected) if expected == ansi => {}
            Ok(_) => failures.push(format!(
                "{}: ANSI output differs; {}",
                case.name,
                save_actual(&format!("{}.ans", case.name), |p| {
                    std::fs::write(p, &ansi).unwrap()
                })
            )),
            Err(err) => failures.push(format!("{}: {err}", ansi_path.display())),
        }

        // Compared as decoded pixels rather than file bytes, so a change in
        // the PNG encoder's compression alone doesn't count as a regression.
        match image::open(&png_path) {
            Ok(expected)
                if expected.color() == png.color()
                    && expected.width() == png.width()
                    && expected.height() == png.height()
                    && expected.as_bytes() == png.as_bytes() => {}
            Ok(_) => failures.push(format!(
                "{}: PNG output differs; {}",
                case.name,
                save_actual(&format!("{}.png", case.name), |p| png.save(p).unwrap())
            )),
            Err(err) => failures.push(format!("{}: {err}", png_path.display())),
        }
    }

    assert!(
        failures.is_empty(),
        "snapshot mismatches (rerun with UPDATE_SNAPSHOTS=1 if intended):\n  {}",
        failures.join("\n  ")
    );
}

/// Guards against a stale snapshot directory: every file in it should belong
/// to a current case, so a renamed or removed case doesn't leave an old
/// snapshot around looking authoritative.
#[test]
fn no_orphaned_snapshots() {
    if updating() {
        return;
    }
    let expected: Vec<String> = CASES
        .iter()
        .flat_map(|case| [format!("{}.ans", case.name), format!("{}.png", case.name)])
        .collect();
    for entry in std::fs::read_dir(snapshot_dir()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(expected.contains(&name), "orphaned snapshot {name}");
    }
}
