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

use bevy::MinimalPlugins;
use bevy::app::{App, AppExit, Startup, Update};
use bevy::asset::io::Reader;
use bevy::asset::processor::AssetProcessor;
use bevy::asset::{Asset, AssetApp, AssetLoader, AssetMode, AssetPlugin, LoadContext};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::prelude::*;
use bevy::gltf::GltfPlugin;
use bevy::image::{CompressedImageFormats, ImageLoader, ImagePlugin};
use bevy::log::LogPlugin;
use bevy::reflect::TypePath;
use bevy::shader::{Shader, ShaderLoader};
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
        MinimalPlugins,
        LogPlugin::default(),
        AssetPlugin {
            file_path: input.to_owned(),
            processed_file_path: output.to_owned(),
            mode: AssetMode::Processed,
            watch_for_changes_override: Some(false),
            use_asset_processor_override: Some(true),
            ..default()
        },
        ImagePlugin::default(),
        // Registered so the processor can resolve `loader: "..."` names in
        // source `.meta` files. We never load these — they fall through to
        // the processor's byte-copy branch.
        GltfPlugin::default(),
    ));

    app.init_asset::<SeedlingSampleStubAsset>()
        .register_asset_loader(SeedlingSampleLoaderStub);

    // ShaderLoader is normally registered by `RenderPlugin` (which we don't
    // pull in). Register it manually so source `.meta` files referencing
    // `bevy_shader::shader::ShaderLoader` deserialize, and so .wgsl/.vert/etc.
    // without source metas get a Load meta synthesized instead of Ignore.
    app.init_asset::<Shader>()
        .init_asset_loader::<ShaderLoader>();

    // ImagePlugin only `preregister_asset_loader`s ImageLoader (a name
    // reservation); the real instance is normally registered by bevy_render,
    // which we don't pull in. Register it manually. The `empty()` arg lists
    // GPU-side compressed *input* formats we accept (e.g. existing KTX2 in
    // the source tree) — we transcode PNG/JPEG → KTX2 here, never read a
    // pre-compressed input, so empty is correct.
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

// Stub for `bevy_seedling::sample::assets::SampleLoader`. We don't depend on
// bevy_seedling because its `SeedlingPlugin` only registers the real loader
// after starting a cpal audio stream — fatal on Linux CI without an audio
// device. The processor's lookup-by-name uses the loader's
// `TypePath::type_path()` string (see bevy_asset's AssetLoaders::push), so a
// stub with a hand-written type_path resolves identically. SampleLoader's
// `Settings = ()`, so meta deserialize/serialize round-trips trivially.
//
// Brittle if seedling ever changes Settings from `()` to a real struct.
#[derive(TypePath)]
#[type_path = "bevy_seedling::sample::assets"]
#[type_name = "SampleLoader"]
struct SeedlingSampleLoaderStub;

#[derive(Asset, TypePath)]
struct SeedlingSampleStubAsset;

impl AssetLoader for SeedlingSampleLoaderStub {
    type Asset = SeedlingSampleStubAsset;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        _reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        unreachable!("stub loader: registered for meta resolution at bake time, never loaded")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The whole point of the stub is that `type_path()` matches seedling's
    // real loader. If this assertion ever fails, the processor's name lookup
    // would silently miss our stub and synthesize Ignore for audio assets.
    #[test]
    fn seedling_stub_typepath_matches_real_loader() {
        assert_eq!(
            SeedlingSampleLoaderStub::type_path(),
            "bevy_seedling::sample::assets::SampleLoader",
        );
        assert_eq!(SeedlingSampleLoaderStub::short_type_path(), "SampleLoader");
    }
}
