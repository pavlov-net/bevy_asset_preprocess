//! `bevy-asset-preprocess [--force] <input_dir> <output_dir>`
//!
//! Walks `input_dir`, compresses every PNG/JPEG into a `.ktx2`
//! (BCn/ASTC + mipmaps + zstd via `bevy_image::CompressedImageSaver`),
//! byte-copies everything else, and writes the result tree into
//! `output_dir`. See the crate-level docs for the design.

use std::path::PathBuf;
use std::process::ExitCode;

use bevy_asset_preprocess::PreprocessConfig;

fn main() -> ExitCode {
    let mut config = PreprocessConfig::default();
    let mut positional: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--force" | "-f" => config.force = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: bevy-asset-preprocess [--force] <input_dir> <output_dir>\n\n\
                     By default, outputs whose mtime is newer than both the input and the\n\
                     bake binary are left alone. Pass --force to rebake everything."
                );
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("bevy-asset-preprocess: unknown flag {other}");
                return ExitCode::from(2);
            }
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let [input, output] = positional.as_slice() else {
        eprintln!("usage: bevy-asset-preprocess [--force] <input_dir> <output_dir>");
        return ExitCode::from(2);
    };

    match bevy_asset_preprocess::preprocess(input, output, &config) {
        Ok(stats) => {
            eprintln!(
                "bevy-asset-preprocess: {} baked, {} copied, {} skipped (fresh), {} failed ({} → {})",
                stats.baked,
                stats.copied,
                stats.skipped_fresh,
                stats.failed,
                input.display(),
                output.display(),
            );
            if stats.failed > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("bevy-asset-preprocess: failed: {err}");
            ExitCode::FAILURE
        }
    }
}
