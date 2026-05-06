//! Headless asset pre-processing for Bevy projects.
//!
//! Builds a `MinimalPlugins` app, loads every image under `input`,
//! and writes a compressed `.ktx2` (BCn/ASTC + mipmaps + zstd, via
//! `bevy_image::CompressedImageSaver`) under `output`. Non-image
//! files are byte-copied through. See `README.md` for the design
//! rationale and how this differs from `AssetMode::Processed`.
//!
//! Entry point: [`preprocess`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bevy::app::{App, AppExit, ScheduleRunnerPlugin, TaskPoolPlugin, Update};
use bevy::asset::io::AssetSourceId;
use bevy::asset::saver::{SavedAsset, save_using_saver};
use bevy::asset::{
    AssetApp, AssetMode, AssetPath, AssetPlugin, AssetServer, Assets, Handle, LoadState,
};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::prelude::*;
use bevy::image::{
    CompressedImageFormats, CompressedImageSaver, CompressedImageSaverSettings, Image, ImageLoader,
    ImagePlugin,
};
use bevy::log::LogPlugin;
use bevy::tasks::IoTaskPool;
use bevy::utils::default;

/// Counts of work units after a [`preprocess`] run.
#[derive(Debug, Default, Clone, Copy)]
pub struct PreprocessStats {
    /// Images successfully loaded, compressed, and written to `<output>/...ktx2`.
    pub baked: usize,
    /// Non-image files byte-copied into `<output>` unchanged.
    pub copied: usize,
    /// Inputs whose output was newer than the source — left untouched.
    pub skipped_fresh: usize,
    /// Images that failed to load or save. Non-fatal — see logs.
    pub failed: usize,
}

/// Caller-tunable knobs for [`preprocess`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PreprocessConfig {
    /// Re-bake every input regardless of mtime. Default: false (skip
    /// outputs whose mtime is newer than the input + the binary).
    pub force: bool,
}

/// Pre-process every asset under `input` into `output`. Image
/// extensions (`.png` / `.jpg` / `.jpeg`) are routed through Bevy's
/// `CompressedImageSaver`; everything else is byte-copied.
///
/// Idempotent: re-running with unchanged inputs produces byte-
/// identical outputs (saver settings carry no timestamp; non-images
/// are direct copies).
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
    // paths bypass the prefix entirely.
    let input = std::fs::canonicalize(input)
        .map_err(|e| format!("canonicalize input {}: {e}", input.display()))?;
    let output = std::fs::canonicalize(output)
        .map_err(|e| format!("canonicalize output {}: {e}", output.display()))?;
    let input = input.as_path();
    let output = output.as_path();

    // mtime floor: an output is "stale" if its mtime is older than
    // the input *or* the bake binary itself. Catches the "user upgraded
    // ctt / bumped saver settings, rebuilt the binary" case automatically.
    // `current_exe()` can fail on exotic platforms; on failure we fall
    // back to "no binary floor" (i.e. mtime check is purely vs. the
    // input).
    let binary_mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    let entries = walk_inputs(input)?;
    let mut stats = PreprocessStats::default();

    let mut images = Vec::<PathBuf>::new();
    for path in entries {
        if is_image(&path) {
            images.push(path);
            continue;
        }
        // Skip `<image>.meta` sidecars — the saver emits a fresh
        // `<output>.ktx2.meta` for every baked image; passing the
        // source through would shadow it with a stale extension
        // reference.
        if is_baked_image_meta(&path) {
            continue;
        }
        let rel = path.strip_prefix(input)?;
        let dst = output.join(rel);
        // Skip-if-fresh applies to passthrough too so a non-`force`
        // re-run keeps output mtimes stable.
        if !config.force && is_output_fresh(&path, &dst, binary_mtime) {
            stats.skipped_fresh += 1;
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&path, &dst)?;
        stats.copied += 1;
    }

    let images_to_bake: Vec<PathBuf> = if config.force {
        images
    } else {
        images
            .into_iter()
            .filter(|src| {
                let rel = src.strip_prefix(input).unwrap();
                let dst = output.join(rel).with_extension("ktx2");
                if is_output_fresh(src, &dst, binary_mtime) {
                    stats.skipped_fresh += 1;
                    false
                } else {
                    true
                }
            })
            .collect()
    };

    if images_to_bake.is_empty() {
        return Ok(stats);
    }

    let (baked, failed) = run_bake_app(input, output, images_to_bake)?;
    stats.baked = baked;
    stats.failed = failed;
    Ok(stats)
}

/// Any I/O failure conservatively returns `false` so the caller falls
/// through to the bake path — "rebuild on doubt" is the only safe
/// default for a content tool.
fn is_output_fresh(src: &Path, dst: &Path, floor: Option<std::time::SystemTime>) -> bool {
    let Ok(dst_meta) = std::fs::metadata(dst) else {
        return false;
    };
    let Ok(dst_mtime) = dst_meta.modified() else {
        return false;
    };
    let Ok(src_meta) = std::fs::metadata(src) else {
        return false;
    };
    let Ok(src_mtime) = src_meta.modified() else {
        return false;
    };
    if dst_mtime < src_mtime {
        return false;
    }
    if let Some(floor) = floor
        && dst_mtime < floor
    {
        return false;
    }
    true
}

fn walk_inputs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(feature = "cli")]
    {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.file_name().to_str().is_some_and(|n| !n.starts_with('.')))
        {
            out.push(entry.into_path());
        }
        // Sort for determinism: stable load order across runs keeps
        // logs and stats reproducible across CI re-runs.
        out.sort();
        Ok(out)
    }
    #[cfg(not(feature = "cli"))]
    {
        // Library-only consumers must supply their own input list via
        // a future `preprocess_with` API. The walker is a CLI nicety,
        // not core functionality.
        let _ = root;
        Err(std::io::Error::other(
            "walk_inputs is only available with the `cli` feature",
        ))
    }
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg")
    )
}

/// `true` if `path` is a Bevy `.meta` sidecar paired with an asset
/// we'll bake. The saver writes a fresh `.ktx2.meta` for every baked
/// image, so passing the source meta through would leave a stale
/// `.png.meta` next to the new `.ktx2`.
fn is_baked_image_meta(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".meta") else {
        return false;
    };
    is_image(Path::new(stem))
}

/// Name of the writable `AssetSource` we register for save outputs.
/// Used both at registration and on every `out://...` save path.
const OUT_SOURCE: &str = "out";

/// Atomic counters shared between the spawned save futures and the
/// ECS exit system. `pending` uses `Release`/`Acquire` so the final
/// reads of `baked`/`failed` happen-after every worker's writes on
/// weak-memory targets (ARM); the count fields themselves stay
/// `Relaxed` since they're only ever read once `pending` has settled.
#[derive(Clone, Default)]
struct BakeCounters {
    pending: Arc<AtomicUsize>,
    baked: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
}

#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Loading,
    Saving,
}

#[derive(Resource)]
struct BakePlan {
    to_load: Vec<(PathBuf, String)>,
    handles: Vec<(PathBuf, Handle<Image>)>,
    counters: BakeCounters,
    phase: Phase,
}

fn run_bake_app(
    input: &Path,
    output: &Path,
    images: Vec<PathBuf>,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let counters = BakeCounters::default();
    let input_root = input.to_path_buf();

    // Asset paths are forward-slash regardless of host OS — Bevy
    // normalises internally too.
    let to_load: Vec<(PathBuf, String)> = images
        .iter()
        .map(|abs| {
            let rel = abs.strip_prefix(&input_root).unwrap().to_path_buf();
            let asset_path = rel.to_string_lossy().replace('\\', "/");
            (rel, asset_path)
        })
        .collect();

    let mut app = App::new();

    // `register_asset_source` must run *before* `AssetPlugin` is added —
    // the plugin builds the source set during its own `build()`, so any
    // sources registered afterwards never get instantiated.
    app.register_asset_source(
        AssetSourceId::Name(OUT_SOURCE.into()),
        bevy::asset::io::AssetSourceBuilder::platform_default(
            output.to_string_lossy().as_ref(),
            None,
        ),
    );

    app.add_plugins((
        TaskPoolPlugin::default(),
        LogPlugin::default(),
        ScheduleRunnerPlugin::default(),
        AssetPlugin {
            file_path: input_root.to_string_lossy().into_owned(),
            mode: AssetMode::Unprocessed,
            // Default `meta_check: Always` so source `.meta` files
            // (`is_srgb`, sampler) feed the load and `CompressedImageSaver`
            // propagates them to the output `.ktx2.meta`.
            ..default()
        },
        ImagePlugin::default(),
    ));

    // `ImagePlugin` only `preregister`s `ImageLoader` — the actual
    // registration happens in `bevy_render::texture`, which we don't
    // pull in. Without this line every `asset_server.load::<Image>(_)`
    // hangs at `LoadState::Loading` forever. `CompressedImageFormats::empty()`
    // means we only decode formats whose decoders our cargo features
    // enable (jpeg, png, ktx2 here).
    app.register_asset_loader(ImageLoader::new(CompressedImageFormats::empty()));

    app.insert_resource(BakePlan {
        to_load,
        handles: Vec::new(),
        counters: counters.clone(),
        phase: Phase::Loading,
    });
    app.add_systems(Update, drive_bakes);

    app.run();

    Ok((
        counters.baked.load(Ordering::Relaxed),
        counters.failed.load(Ordering::Relaxed),
    ))
}

fn drive_bakes(
    asset_server: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut plan: ResMut<BakePlan>,
    mut app_exit: MessageWriter<AppExit>,
) {
    if plan.phase == Phase::Loading {
        if !plan.to_load.is_empty() {
            let queue = std::mem::take(&mut plan.to_load);
            bevy::log::info!(count = queue.len(), "kicking off image loads");
            for (rel, asset_path) in queue {
                let handle: Handle<Image> = asset_server.load(&asset_path);
                plan.handles.push((rel, handle));
            }
        }

        // Wait for every load to either finish or fail. Failures
        // count toward the exit condition so we don't hang on a
        // corrupt input.
        let mut still_loading = false;
        for (_, handle) in &plan.handles {
            match asset_server.load_state(handle) {
                LoadState::Loaded | LoadState::Failed(_) => {}
                LoadState::NotLoaded | LoadState::Loading => still_loading = true,
            }
        }
        if still_loading {
            return;
        }

        let pool = IoTaskPool::get();
        for (rel, handle) in std::mem::take(&mut plan.handles) {
            let load_state = asset_server.load_state(&handle);
            if matches!(load_state, LoadState::Failed(_)) {
                plan.counters.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Move the asset out of `Assets<Image>` so the future owns
            // it and `Assets<Image>` can shrink as saves drain — an 8K
            // RGBA8 texture is ~256 MB; cloning would double peak RAM.
            let Some(image) = images.remove(&handle) else {
                plan.counters.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let asset_server = asset_server.clone();
            let counters = plan.counters.clone();
            // Slash-normalize for Windows: `Path::display()` emits
            // backslashes that Bevy's asset path parser doesn't accept.
            let out_rel = rel
                .with_extension("ktx2")
                .to_string_lossy()
                .replace('\\', "/");
            let out_path = AssetPath::from(format!("{OUT_SOURCE}://{out_rel}"));

            counters.pending.fetch_add(1, Ordering::Relaxed);
            pool.spawn(async move {
                let saver = CompressedImageSaver::default();
                let settings = CompressedImageSaverSettings::default();
                let saved = SavedAsset::from_asset(&image);
                match save_using_saver(asset_server, &saver, &out_path, saved, &settings).await {
                    Ok(()) => counters.baked.fetch_add(1, Ordering::Relaxed),
                    Err(e) => {
                        bevy::log::error!(path = %out_path, error = %e, "compressed_image_saver failed");
                        counters.failed.fetch_add(1, Ordering::Relaxed)
                    }
                };
                // Release pairs with the Acquire load below: every
                // worker's `baked`/`failed` write is visible by the
                // time the exit system observes pending == 0.
                counters.pending.fetch_sub(1, Ordering::Release);
            })
            .detach();
        }
        plan.phase = Phase::Saving;
    }

    if plan.phase == Phase::Saving && plan.counters.pending.load(Ordering::Acquire) == 0 {
        bevy::log::info!(
            baked = plan.counters.baked.load(Ordering::Relaxed),
            failed = plan.counters.failed.load(Ordering::Relaxed),
            "preprocess complete"
        );
        app_exit.write(AppExit::Success);
    }
}
