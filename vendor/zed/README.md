# Vendored Zed subset

This directory contains the GPUI and Zed UI sources required by Chatt GUI. It
was extracted from <https://github.com/zed-industries/zed> at commit
`4f9d466398606adf0003722933118af3d042ab3b`, whose imported upstream base is
`f032f4d`.

The retained platform boundary is macOS plus Linux with Wayland or X11. Windows,
Wasm, Zed application crates, upstream examples and benches, and dependency-crate
test tooling are intentionally omitted. Chatt's default build enables Wayland;
an X11-only dependency graph can be selected with:

```sh
cargo check --no-default-features --features x11
```

The nested workspace manifest carries only dependency declarations used by the
retained crates. When updating this snapshot, preserve the existing crate paths,
copy all applicable license files, update the source commit above, and validate
the result through the top-level Chatt GUI manifest rather than building a full
Zed checkout.

The `assets/fonts` subtree contains the IBM Plex Sans and Lilex files embedded by
Chatt. Their license texts are stored beside the font files. Zed crate license
symlinks resolve to `LICENSE-APACHE` and `LICENSE-GPL` in this directory.
