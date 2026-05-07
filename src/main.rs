//! CLI wrapper — see [`bevy_asset_preprocess`] for the full design.

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
                println!(
                    "usage: bevy-asset-preprocess [--force] <input_dir> <output_dir>\n\n\
                     By default, unchanged assets are skipped via hash-based change detection.\n\
                     Pass --force to reprocess images regardless of cache."
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
                "bevy-asset-preprocess: {} baked, {} failed ({} → {})",
                stats.baked,
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
