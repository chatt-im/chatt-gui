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

## GUI configuration

The renderer owns `chatt/gui.toml` in the platform configuration directory.
This is separate from the daemon-owned `client.toml`; the GUI never reads or
writes daemon configuration. Set `CHATT_GUI_CONFIG` to use an explicit path
(including a relative path), or `XDG_CONFIG_HOME` to override the platform
configuration root.

Open the native Settings surface with the sidebar gear or `secondary-,`.
Appearance and typography changes preview immediately; composer mode and key
bindings take effect after a successful save. A missing file is normal and is
only created on Save. Invalid or externally changed files are not overwritten
without an explicit Replace or reload confirmation.

Every valid appearance or typography edit is also relayed immediately through
the Chatt daemon to all connected GUI instances. The most recently received
edit controls the live preview. Cancel restores the last committed appearance,
while Save persists `gui.toml` and makes that appearance the daemon-session
baseline. A GUI that is offline continues to preview locally and rejoins live
sharing when it reconnects.

Files use schema version 1, kebab-case setting names, hex colors (`#rgb`,
`#rgba`, `#rrggbb`, or `#rrggbbaa`), and GPUI key syntax. Partial files inherit
built-in defaults. Binding tables map sequences to commands; assigning
`"Unbind"` removes an inherited sequence. Tables are separated by dispatch
context: `application`, `composer`, `completion`, `vim`, `code-search`,
`code-viewer`, `formatted-message`, and `non-input`. Supported text rendering
values are `platform-default`, `subpixel`, and `grayscale`. Subpixel rendering
can fall back when the platform backend, window transparency, or renderer does
not support it.
