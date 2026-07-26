# Dependency compatibility forks

These crates are copied from their crates.io releases and patched through the
root `Cargo.toml` to keep one version of shared pre-1.0 dependencies:

- `cosmic-text 0.19.0`: use `skrifa 0.44.0`
- `harfrust 0.5.2`: use `read-fonts 0.41.0`
- `fontconfig-parser 0.5.8`: use `roxmltree 0.21.1`

Source changes needed for compatibility are kept within each crate directory.
