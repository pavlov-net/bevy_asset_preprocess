# bevy_asset_preprocess

Headless CLI + library for pre-processing Bevy assets. Drives Bevy's
[`AssetProcessor`][ap] from a one-shot binary so you can bake compressed
textures (and process every other asset type whose loader you have a
plugin for) at build time, without launching a game.

[ap]: https://docs.rs/bevy_asset/latest/bevy_asset/processor/struct.AssetProcessor.html

```bash
cargo run --release -- assets/ assets-baked/
# Hash-based change detection skips unchanged inputs; pass --force to
# reprocess every image regardless of cache.
cargo run --release -- --force assets/ assets-baked/
```

Status: **prototype**. Tracks bevy `main` — depends on the
`compressed_image_saver` feature added in [bevyengine/bevy#23567][pr].

[pr]: https://github.com/bevyengine/bevy/pull/23567

## What it does

The crate spins up a minimal `App` (no renderer, no window, no game
logic) configured with `AssetMode::Processed` and a small set of asset
plugins:

- `ImagePlugin` — registers `LoadTransformAndSave<ImageLoader, _,
  CompressedImageSaver>` as the default processor for `png` / `jpg` /
  `jpeg` (this is `ImagePlugin`'s built-in behavior when the
  `compressed_image_saver` feature is on).
- `GltfPlugin` and `SeedlingPlugin` — pulled in solely so the processor
  can resolve `loader: "..."` strings inside source `.meta` files.

Other asset types (shaders, scenes, custom loaders, etc.) need their
own plugin or `register_asset_loader` call added to `run_bake_app`,
otherwise their source `.meta` files fail to deserialize.

`AssetProcessor` then walks the input tree and produces an output tree
at the same relative paths. Images get compressed to KTX2 in place
(`texture.png` → `output/texture.png` containing KTX2 bytes), with a
`.meta` sidecar carrying the runtime `ImageLoaderSettings`. Non-image
files fall through the processor's no-processor branch and are
byte-copied unchanged with a matching `.meta`.

The game then runs in `AssetMode::Processed` and reads from the output
tree; the `.meta` sidecars tell the asset server which loader to use
for each file.

## Source `.meta` files are authoritative

`AssetProcessor` reads source `.meta` files to decide what to do with
each asset. There are three actions:

- `AssetAction::Process { processor, settings }` — run the named
  processor (e.g. `LoadTransformAndSave<...>` to compress an image).
- `AssetAction::Load { loader, settings }` — byte-copy the file and
  emit a meta in the output that points at the named loader.
- `AssetAction::Ignore` — skip the file entirely.

When a file has no source `.meta`, the processor synthesizes one based
on extension: it picks a default processor if one is registered for the
extension, otherwise a default loader, otherwise `Ignore`.

**Practical consequence:** if you want a `.png` *compressed*, its
source meta must say `Process` (or there must be no source meta — the
processor's default for `png`/`jpg`/`jpeg` is the compression
processor). If the source meta says `Load`, the file is byte-copied
uncompressed.

## Build dependencies

- **clang** — `ctt-compressonator` (the encoder behind
  `CompressedImageSaver`) uses `-march=knl`, which GCC ≥ 15 doesn't
  recognize. Set `CXX=clang++` if the default compiler is GCC.
- **Linux only:** `libasound2-dev` and `pkg-config` — `bevy_seedling`
  pulls in `firewheel`/`cpal`, which links ALSA.

## Caveats

### Normal maps + BCn don't preserve unit length

`CompressedImageSaver` has no "this is a normal map" hint; CTT
compresses every channel independently. Renormalize in your shader
after sampling/blending, or pre-compress with a normal-aware tool.
Bevy's in-engine usage of the saver has the same property.

### Wasm format choice

WebGPU's BC support exists on desktop Chrome/Edge/Firefox but not
Safari/mobile. If you target broad wasm, you'll want
`compressed_image_saver_universal` (UASTC/Basis, no mipmaps) — Bevy
doesn't yet have a story for serving format-variants per device.

### `.meta` sidecars in output

`AssetProcessor` writes a `.meta` next to every processed asset
holding the loader settings (sampler, sRGB flag, etc.). They're tiny
and load-time authoritative; ship them with your processed assets.

### `imported_assets/` directory

The processor writes a transaction log to
`<output>/imported_assets/log` for crash recovery. It's harmless and
not needed by the game at runtime, but currently isn't cleaned up.

## Library use

```rust
use bevy_asset_preprocess::{preprocess, PreprocessConfig};

let stats = preprocess(
    Path::new("assets"),
    Path::new("assets-baked"),
    &PreprocessConfig { force: false },
)?;
```

The `cli` feature gates `walkdir` and the `bevy-asset-preprocess`
binary; library consumers can `default-features = false` to drop both.

## License

MIT OR Apache-2.0, matching the Bevy ecosystem.
