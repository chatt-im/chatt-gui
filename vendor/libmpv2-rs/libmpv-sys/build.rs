use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let out_path = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let crate_path = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"),
    );

    generate_bindings(&crate_path, &out_path);

    #[cfg(feature = "vendored")]
    build_vendored_libmpv(&crate_path, &out_path);

    #[cfg(not(feature = "vendored"))]
    println!("cargo:rustc-link-lib=mpv");
}

#[cfg(not(feature = "use-bindgen"))]
fn generate_bindings(crate_path: &Path, out_path: &Path) {
    fs::copy(
        crate_path.join("pregenerated_bindings.rs"),
        out_path.join("bindings.rs"),
    )
    .expect("could not copy pregenerated libmpv bindings");
}

#[cfg(feature = "use-bindgen")]
fn generate_bindings(crate_path: &Path, out_path: &Path) {
    let mpv_include = crate_path
        .join("../../mpv/include")
        .canonicalize()
        .expect("vendored mpv include directory is missing");
    let bindings = bindgen::Builder::default()
        .formatter(bindgen::Formatter::Prettyplease)
        .header(mpv_include.join("mpv/client.h").to_string_lossy())
        .header(mpv_include.join("mpv/render.h").to_string_lossy())
        .header(mpv_include.join("mpv/render_gl.h").to_string_lossy())
        .header(mpv_include.join("mpv/render_vk.h").to_string_lossy())
        .header(mpv_include.join("mpv/stream_cb.h").to_string_lossy())
        .clang_arg(format!("-I{}", mpv_include.display()))
        .impl_debug(true)
        .opaque_type("mpv_handle")
        .opaque_type("mpv_render_context")
        .generate()
        .expect("unable to generate libmpv bindings");
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("could not write libmpv bindings");
}

#[cfg(feature = "vendored")]
fn build_vendored_libmpv(crate_path: &Path, out_path: &Path) {
    let host = env::var("HOST").expect("HOST is not set");
    let target = env::var("TARGET").expect("TARGET is not set");
    assert_eq!(
        host, target,
        "the vendored libmpv build currently supports native builds only"
    );

    let source = crate_path
        .join("../../mpv")
        .canonicalize()
        .expect("vendored mpv source directory is missing");
    let build = out_path.join("mpv-build");
    emit_rerun_tree(&source);

    if !build.join("build.ninja").exists() {
        run(
            Command::new("meson")
                .arg("setup")
                .arg(&build)
                .arg(&source)
                .arg("--wrap-mode=nofallback")
                .arg("--default-library=static")
                .arg("--buildtype=release")
                .arg("-Dcplayer=false")
                .arg("-Dlibmpv=true")
                .arg("-Dtests=false")
                .arg("-Dbuild-date=false")
                .arg("-Dvulkan=enabled"),
            "configure vendored libmpv",
        );
    }

    let mut compile = Command::new("meson");
    compile.arg("compile").arg("-C").arg(&build);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        compile.arg("-j").arg(jobs);
    }
    compile.arg("mpv");
    run(&mut compile, "compile vendored libmpv");

    let archive = build.join("libmpv.a");
    assert!(archive.is_file(), "Meson did not produce {}", archive.display());

    let pc_dir = build.join("meson-private");
    let old_pkg_config_path = env::var_os("PKG_CONFIG_PATH");
    let mut pkg_config_path = pc_dir.into_os_string();
    if let Some(old) = old_pkg_config_path {
        pkg_config_path.push(OsStr::new(":"));
        pkg_config_path.push(old);
    }
    env::set_var("PKG_CONFIG_PATH", pkg_config_path);

    let dependencies = pkg_config::Config::new()
        .cargo_metadata(false)
        .statik(false)
        .probe("mpv")
        .expect("could not resolve vendored libmpv's system dependencies");

    println!("cargo:rustc-link-search=native={}", build.display());
    println!("cargo:rustc-link-lib=static=mpv");
    for path in dependencies.link_paths {
        if path.is_dir() {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }
    for library in dependencies.libs {
        if library != "mpv" {
            println!("cargo:rustc-link-lib=dylib={library}");
        }
    }
    for path in dependencies.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in dependencies.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

#[cfg(feature = "vendored")]
fn emit_rerun_tree(root: &Path) {
    println!("cargo:rerun-if-changed={}", root.display());
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("could not read vendored mpv entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

#[cfg(feature = "vendored")]
fn run(command: &mut Command, description: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(status.success(), "failed to {description}: {status}");
}
