//! Headless asset pre-processing for Bevy projects.
//!
//! Uses Bevy's built-in [`AssetProcessor`] with [`LoadTransformAndSave`] to
//! compress images (PNG/JPEG via `CompressedImageSaver`). Non-image files are
//! byte-copied unchanged by the processor's built-in passthrough. See
//! `README.md` for the design rationale.
//!
//! **Processed-file paths:** `AssetProcessor` stores processed assets at the
//! *same relative path* as the source (e.g. `texture.png` → `output/texture.png`
//! with KTX2 content), plus a `.meta` sidecar. The game loads from `output/` in
//! `AssetMode::Processed`; the `.meta` tells it which loader to use.
//!
//! **Note:** `AssetProcessor` writes `.meta` sidecar files back into the
//! source (`input`) directory for any asset that doesn't already have one.
//! These record which processor and settings apply per-file and are intended
//! to be committed alongside your source assets.
//!
//! Entry point: [`preprocess`].

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use bevy::app::{App, AppExit, ScheduleRunnerPlugin, Startup, TaskPoolPlugin, Update};
use bevy::asset::processor::AssetProcessor;
use bevy::asset::{AssetApp, AssetMode, AssetPlugin};
use bevy::audio::{AudioLoader, AudioSource};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::prelude::*;
use bevy::gltf::GltfPlugin;
use bevy::image::{CompressedImageFormats, ImageLoader, ImagePlugin};
use bevy::log::LogPlugin;
use bevy::tasks::{IoTaskPool, Task};
use bevy::utils::default;

/// Counts of work units after a [`preprocess`] run.
#[derive(Debug, Default, Clone, Copy)]
pub struct PreprocessStats {
    /// Images present in the output directory after processing (newly
    /// compressed or already up-to-date from a prior run).
    pub baked: usize,
    /// Images missing from output after processing — load or save failed.
    pub failed: usize,
}

/// Caller-tunable knobs for [`preprocess`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PreprocessConfig {
    /// Re-compress every image regardless of the processor's hash cache.
    /// Default: false (hash-based change detection skips unchanged images).
    ///
    /// Non-image passthrough files are managed by the processor's own change
    /// detection and are not affected by this flag.
    pub force: bool,
}

/// Pre-process every asset under `input` into `output`. Image extensions
/// (`.png` / `.jpg` / `.jpeg`) are compressed to KTX2 format via Bevy's
/// `AssetProcessor` + `CompressedImageSaver`; everything else is
/// byte-copied unchanged by the processor's built-in passthrough.
///
/// Processed files retain their original filename — a compressed `texture.png`
/// is stored as `output/texture.png` (KTX2 bytes) with a `texture.png.meta`
/// sidecar. The game loads from `output/` in `AssetMode::Processed`.
///
/// Idempotent: re-running with unchanged inputs is a no-op (the processor's
/// hash check skips assets whose content has not changed).
pub fn preprocess(
    input: &Path,
    output: &Path,
    config: &PreprocessConfig,
) -> Result<PreprocessStats, Box<dyn std::error::Error>> {
    if !input.is_dir() {
        return Err(format!("input is not a directory: {}", input.display()).into());
    }
    std::fs::create_dir_all(output)?;

    // Canonicalize before handing paths to Bevy. `AssetPlugin::file_path`
    // resolves relative paths against Bevy's base dir (BEVY_ASSET_ROOT →
    // CARGO_MANIFEST_DIR → current_exe), not the user's cwd; absolute
    // paths bypass that prefix entirely.
    let input = std::fs::canonicalize(input)
        .map_err(|e| format!("canonicalize input {}: {e}", input.display()))?;
    let output = std::fs::canonicalize(output)
        .map_err(|e| format!("canonicalize output {}: {e}", output.display()))?;
    let input_str = input
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 input path: {}", input.display()))?;
    let output_str = output
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 output path: {}", output.display()))?;
    let input = input.as_path();
    let output = output.as_path();

    let mut stats = PreprocessStats::default();

    // Collect image paths for outcome counting after the processor finishes.
    // Non-cli builds skip stats (no walkdir dependency).
    let images: Vec<PathBuf> = {
        #[cfg(feature = "cli")]
        {
            walk_inputs(input)
                .into_iter()
                .filter(|p| is_image(p))
                .collect()
        }
        #[cfg(not(feature = "cli"))]
        {
            Vec::new()
        }
    };

    // AssetProcessor writes processed assets at the same relative path as the
    // source (same extension, KTX2 content), with a .meta sidecar alongside.
    let processed_paths = images
        .iter()
        .map(|src| src.strip_prefix(input).map(|rel| output.join(rel)))
        .collect::<Result<Vec<_>, _>>()?;

    // When forcing, delete the processed file and its .meta so the processor
    // reprocesses unconditionally rather than hash-matching against stale state.
    if config.force {
        for processed in &processed_paths {
            if let Err(e) = std::fs::remove_file(processed)
                && e.kind() != ErrorKind::NotFound
            {
                return Err(e.into());
            }
            let meta_path = {
                let mut s = processed.as_os_str().to_os_string();
                s.push(".meta");
                PathBuf::from(s)
            };
            if let Err(e) = std::fs::remove_file(&meta_path)
                && e.kind() != ErrorKind::NotFound
            {
                return Err(e.into());
            }
        }
    }

    run_bake_app(input_str, output_str);

    for processed in &processed_paths {
        match processed.try_exists() {
            Ok(true) => stats.baked += 1,
            Ok(false) => stats.failed += 1,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(stats)
}

#[cfg(feature = "cli")]
fn walk_inputs(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name().to_str().is_some_and(|n| !n.starts_with('.')))
        .map(|e| e.into_path())
        .collect();
    // Sort for determinism: stable order across runs keeps logs reproducible.
    out.sort();
    out
}

#[cfg(feature = "cli")]
fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg")
    )
}

fn run_bake_app(input: &str, output: &str) {
    let mut app = App::new();

    app.add_plugins((
        TaskPoolPlugin::default(),
        LogPlugin::default(),
        // `ScheduleRunnerPlugin::default()` runs schedules in a tight loop and
        // returns only when `AppExit` is written — making `app.run()` blocking.
        ScheduleRunnerPlugin::default(),
        AssetPlugin {
            file_path: input.to_owned(),
            processed_file_path: output.to_owned(),
            mode: AssetMode::Processed,
            watch_for_changes_override: Some(false),
            use_asset_processor_override: Some(true),
            ..default()
        },
        ImagePlugin::default(),
        // Registered so the processor can resolve loader names from source
        // `.meta` files. We never load these — they fall through to the
        // processor's byte-copy branch. AudioPlugin is *not* used because its
        // build() opens the system audio device and adds per-tick systems that
        // are pure waste for a one-shot bake; we register the loader manually
        // below.
        GltfPlugin::default(),
    ));

    app.init_asset::<AudioSource>()
        .init_asset_loader::<AudioLoader>();

    // ImagePlugin only `preregister_asset_loader`s ImageLoader (a name
    // reservation); the real instance is normally registered by bevy_render,
    // which we don't pull in. Register it manually.
    //
    // ImagePlugin::build *does* register and default the
    // LoadTransformAndSave<ImageLoader, _, CompressedImageSaver> processor for
    // png/jpeg/jpg when the `compressed_image_saver` feature is on. Don't
    // re-register it — duplicate registration leaves get_processor's
    // short_type_path table in Ambiguous(vec![same, same]) state and breaks
    // every meta lookup.
    app.register_asset_loader(ImageLoader::new(CompressedImageFormats::empty()));

    app.add_systems(Startup, start_exit_task);
    app.add_systems(Update, check_exit_task);

    app.run();
}

#[derive(Resource)]
struct ExitTask(Task<()>);

fn start_exit_task(processor: Res<AssetProcessor>, mut commands: Commands) {
    let data = processor.data().clone();
    let task = IoTaskPool::get().spawn(async move {
        data.wait_until_finished().await;
    });
    commands.insert_resource(ExitTask(task));
}

fn check_exit_task(task: Option<Res<ExitTask>>, mut app_exit: MessageWriter<AppExit>) {
    if let Some(task) = task
        && task.0.is_finished()
    {
        app_exit.write(AppExit::Success);
    }
}
