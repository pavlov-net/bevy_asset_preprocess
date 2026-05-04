# bevy_asset_preprocess

Headless CLI + library for pre-processing Bevy assets. Walks a
source asset tree, compresses every PNG/JPEG into a `.ktx2`
(BCn/ASTC + mipmaps + zstd via [`CompressedImageSaver`][saver]),
byte-copies everything else, and writes the result tree to a
sibling directory.

[saver]: https://docs.rs/bevy_image/latest/bevy_image/struct.CompressedImageSaver.html

```bash
cargo run --release -- assets/ assets-baked/
# Skips inputs whose output is fresh; pass --force to rebake.
cargo run --release -- --force assets/ assets-baked/
```

Status: **prototype**. Tracks bevy `main` — depends on the
`compressed_image_saver` feature added in [bevyengine/bevy#23567][pr].

[pr]: https://github.com/bevyengine/bevy/pull/23567

## Why

Bevy's `AssetMode::Processed` runs in-app as a background task and
exposes no "processing finished" signal — unworkable as a CI bake
step. The alternative is rolling your own saver and walking the tree
yourself, which reinvents Bevy's loader/saver wheel.

This crate is a third option: a tiny headless `App` (no renderer, no
window, no game logic) that uses Bevy's public asset APIs
([`AssetServer`][as], [`save_using_saver`][sas]) to drive the saver
directly, and exits cleanly when every input we asked for has been
saved.

[as]: https://docs.rs/bevy_asset/latest/bevy_asset/struct.AssetServer.html
[sas]: https://docs.rs/bevy_asset/latest/bevy_asset/saver/fn.save_using_saver.html

## Caveats

### Normal maps + BCn don't preserve unit length

The saver has no "this is a normal map" hint; CTT compresses every
channel independently. Renormalize in your shader after
sampling/blending, or pre-compress with a normal-aware tool. Bevy's
in-engine usage of the saver has the same property.

### `.meta` sidecars are written

`CompressedImageSaver` writes `<output>.ktx2` *and*
`<output>.ktx2.meta` holding the runtime-needed `ImageLoaderSettings`
(sampler defaults, sRGB flag, etc). Pipelines that don't ship `.meta`
files have two options:

- Drop them post-bake (`find <out> -name '*.meta' -delete`).
- Ship them — they're tiny (~360 bytes each) and let you delete the
  per-load `with_settings(...)` runtime overrides at the call site.

### Wasm format choice

WebGPU's BC support exists on desktop Chrome/Edge/Firefox but not
Safari/mobile. If you target broad wasm, you'll want
`compressed_image_saver_universal` (UASTC/Basis, no mipmaps) — Bevy
doesn't yet have a story for serving format-variants per device.

### CTT compile cost

`compressed_image_saver` pulls in CTT (ISPC encoders), which adds
non-trivial cold-cache CI time (~3–4 minutes on an 8-core runner
from scratch; ~30 seconds incremental). Cache the target dir
aggressively — `Swatinem/rust-cache` works fine.

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
