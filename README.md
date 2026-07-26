# Chatt GUI

Chatt GUI is an experimental native desktop client for
[Chatt](https://github.com/chatt-im/chatt). It connects to a running Chatt
daemon through the renderer RPC API. The project exists primarily to develop
and validate that API; it is not currently intended to be a production client
for general use.

Warning: Largely AI Generated, Proof of Concept, (currently).

## Why this exists

A minimal test client can show that an RPC API works, but not that it can
support a responsive, efficient application. Chatt GUI exercises the API while
pursuing:

- Fast startup and low input latency.
- Efficient rendering and daemon communication.
- Hardware-accelerated video decoding.
- Low-latency video streaming and playback.

These performance goals make the GUI more than a disposable protocol test, but
they do not make it production-ready.

## Project status

The GUI was largely written by AI, including its high-level architecture. The
code has received less human review than Chatt's production components and
should be treated as experimental.

Two areas in particular need substantial work before broader use:

- **Platform coverage:** development and testing have focused on Linux and a
  limited set of GPUs. Hardware decoding, drivers, and video formats may behave
  differently on other systems.
- **Security:** native media decoding, its C interfaces, and the surrounding
  application have not undergone an extensive security audit. The current
  design does not meet the security standard of Chatt's production clients.

## Security warning

**Do not use this GUI in rooms or with media you do not trust.**

Unlike Chatt's web view, which has been tuned for untrusted content and leaves
media decoding to a hardened web browser, this GUI accesses the native
FFmpeg/libmpv media stack through C interfaces. Media decoding is not
sandboxed. A malicious or malformed attachment could exploit a vulnerability
in a decoder, native dependency, binding, or the GUI itself.

Use the GUI only with trusted participants and trusted media, or in a controlled
development environment where this risk is understood.

## Build prerequisites

The current build targets Linux. Install:

- A current stable Rust toolchain and Cargo.
- Git, curl, tar with xz support, make, pkg-config, Meson, and Ninja.
- A C toolchain.
- Development metadata for ALSA and the Vulkan loader (`alsa.pc` and
  `vulkan.pc`).

Package names vary by distribution. VAAPI and CUDA/NVDEC are loaded at runtime
when their system libraries and drivers are available; they are not required to
start the application.

The renderer protocol crates come from the Chatt Git revision pinned in
`Cargo.toml`. The media stack is also built from pinned source instead of using
whatever FFmpeg or mpv features happen to be installed on the build host.

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

## License

Chatt GUI is licensed under the [GNU General Public License version 3](LICENSE).
Vendored dependencies remain subject to their own license terms.
