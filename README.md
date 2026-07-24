# Chatt GUI

Chatt GUI is a Rust 2024 GPUI desktop client for a separately running Chatt
daemon. The renderer protocol crates come from a pinned Chatt Git revision,
and the media stack is built from pinned source rather than whichever FFmpeg
or mpv features happen to be installed on the build host.

## Build prerequisites

The current native build targets Linux. Install:

- Rust and Cargo with Rust 2024 edition support.
- Git, curl, tar with xz support, make, pkg-config, Meson, and Ninja.
- A C toolchain.
- Development metadata for ALSA and the Vulkan loader (`alsa.pc` and
  `vulkan.pc`).

Package names vary by distribution. VAAPI and CUDA/NVDEC are loaded at runtime
when their system libraries and drivers are available; they are not required to
start the application.

## Reproducible checkout and build

Download and verify the pinned FFmpeg source release and apply the tracked
build patch:

```sh
./scripts/fetch-ffmpeg.sh
```

Then build or test using the committed dependency lock:

```sh
cargo check --locked
cargo build --locked
cargo test --locked
```

`vendor/ffmpeg` is intentionally ignored by Git. The fetch script verifies the
FFmpeg 8.1.2 archive before extracting it, then applies the small configure
patch in `patches/`. It refuses to overwrite a differing tree unless `--force`
is supplied.

Run the Chatt daemon separately and point both processes at the same runtime
directory with `CHATT_RUN_DIR` when the default discovery location is not
appropriate. `devsm.toml` contains the development profiles used by this
checkout.

## Developing against a local Chatt checkout

The normal build uses the exact Chatt Git revision pinned in `Cargo.toml`. To
use an active checkout without editing tracked manifests:

```sh
./scripts/use-local-chatt.sh /code/chatt
cargo check
```

The helper writes ignored Cargo patches to `.cargo/local.toml`. The checkout
argument defaults to `/code/chatt`. Restore the reproducible pinned Git
dependencies with:

```sh
./scripts/use-local-chatt.sh --pinned
cargo check --locked
```

Local Chatt changes that alter their crates' dependency graphs can require a
temporary lockfile update. Do not commit that update unless the Chatt Git pins
are advanced to the same revision.
