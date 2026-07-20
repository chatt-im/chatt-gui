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

    let ffmpeg_lib = build_vendored_ffmpeg(crate_path, out_path);
    let ffmpeg_pc = ffmpeg_lib.join("pkgconfig");
    let old_pkg_config_path = env::var_os("PKG_CONFIG_PATH");
    let mut pkg_config_path = ffmpeg_pc.into_os_string();
    if let Some(old) = old_pkg_config_path.as_ref() {
        pkg_config_path.push(OsStr::new(":"));
        pkg_config_path.push(old);
    }
    env::set_var("PKG_CONFIG_PATH", &pkg_config_path);

    let source = crate_path
        .join("../../mpv")
        .canonicalize()
        .expect("vendored mpv source directory is missing");
    let build = out_path.join("mpv-build");
    emit_rerun_tree(&source);

    let mut setup = Command::new("meson");
    setup
        .arg("setup")
        .arg(&build)
        .arg(&source)
        .arg("--wrap-mode=nofallback")
        .arg("--default-library=static")
        .arg("--buildtype=release")
        // An embedded render client must not inherit whichever optional
        // libraries happen to be installed on the build host.
        .arg("-Dauto_features=disabled")
        .arg("-Dcplayer=false")
        .arg("-Dlibmpv=true")
        .arg("-Dtests=false")
        .arg("-Dbuild-date=false")
        .arg("-Dmanpage-build=disabled")
        .arg("-Dgl=disabled")
        .arg("-Dlua=disabled")
        .arg("-Dvulkan=enabled")
        // Keep one broadly supported Linux audio path for attachment playback.
        // Desktop sound servers expose ALSA devices without adding their full
        // client and codec stacks to this binary's ELF dependencies.
        .arg("-Dalsa=enabled")
        .arg("-Dpulse=disabled");
    if build.join("build.ninja").exists() {
        setup.arg("--reconfigure");
    }
    run(&mut setup, "configure vendored libmpv");

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
    let mut pkg_config_path = pc_dir.into_os_string();
    pkg_config_path.push(OsStr::new(":"));
    pkg_config_path.push(ffmpeg_lib.join("pkgconfig"));
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
    println!("cargo:rustc-link-search=native={}", ffmpeg_lib.display());
    println!("cargo:rustc-link-lib=static=mpv");
    for path in dependencies.link_paths {
        if path.is_dir() {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }
    for library in dependencies.libs {
        if library != "mpv" {
            if matches!(
                library.as_str(),
                "avcodec" | "avfilter" | "avformat" | "avutil" | "swresample" | "swscale"
            ) {
                println!("cargo:rustc-link-lib=static={library}");
            } else {
                println!("cargo:rustc-link-lib=dylib={library}");
            }
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
fn build_vendored_ffmpeg(crate_path: &Path, out_path: &Path) -> PathBuf {
    let source = crate_path
        .join("../../ffmpeg")
        .canonicalize()
        .expect("vendored FFmpeg source directory is missing");
    let build = out_path.join("ffmpeg-build");
    let install = out_path.join("ffmpeg-install");
    fs::create_dir_all(&build).expect("could not create vendored FFmpeg build directory");
    emit_rerun_tree(&source);

    let mut configure = Command::new(source.join("configure"));
    configure
        .current_dir(&build)
        .arg(format!("--prefix={}", install.display()))
        .arg("--disable-autodetect")
        .arg("--disable-debug")
        .arg("--disable-doc")
        .arg("--disable-network")
        .arg("--disable-programs")
        .arg("--disable-everything")
        .arg("--enable-pic")
        .arg("--enable-static")
        .arg("--disable-shared")
        .arg("--enable-avcodec")
        .arg("--enable-avfilter")
        .arg("--enable-avformat")
        .arg("--enable-swresample")
        .arg("--enable-swscale")
        .arg("--enable-decoder=aac,aac_fixed,aac_latm,ac3,alac,ass,av1,dca,eac3,ffv1,flac,gif,h264,hevc,mjpeg,mp3,mp3float,opus,pcm_alaw,pcm_f32le,pcm_f64le,pcm_mulaw,pcm_s16be,pcm_s16le,pcm_s24be,pcm_s24le,pcm_s32be,pcm_s32le,pcm_u8,png,ssa,subrip,vorbis,vp8,vp9,webp,webvtt")
        .arg("--enable-demuxer=aac,ac3,aiff,ape,asf,avi,eac3,flac,flv,h264,hevc,image2,matroska,mov,mp3,mpegps,mpegts,mpegvideo,nut,ogg,rm,wav")
        .arg("--enable-parser=aac,aac_latm,ac3,av1,dca,flac,h264,hevc,mpegaudio,opus,vorbis,vp8,vp9")
        .arg("--enable-bsf=aac_adtstoasc,av1_frame_merge,extract_extradata,h264_mp4toannexb,hevc_mp4toannexb,null,opus_metadata,vp9_superframe")
        .arg("--enable-filter=aformat,amerge,anull,aresample,asetpts,asplit,atrim,format,hflip,null,scale,setpts,split,transpose,trim,vflip,yadif")
        .arg("--enable-protocol=fd,file,pipe");
    run(&mut configure, "configure vendored FFmpeg");

    let mut compile = Command::new("make");
    compile.current_dir(&build);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        compile.arg("-j").arg(jobs);
    }
    run(&mut compile, "compile vendored FFmpeg");

    run(
        Command::new("make").current_dir(&build).arg("install"),
        "install vendored FFmpeg",
    );

    install.join("lib")
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
