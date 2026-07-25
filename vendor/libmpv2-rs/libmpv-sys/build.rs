use std::{
    env,
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

    let alsa = discover_system_package("alsa");
    let vulkan = discover_system_package("vulkan");
    let pkg_config_libdir = out_path.join("isolated-pkgconfig");
    fs::create_dir_all(&pkg_config_libdir)
        .expect("could not create isolated pkg-config directory");
    clear_pkg_config_files(&pkg_config_libdir);
    write_system_pkg_config(&pkg_config_libdir, &alsa, None);

    let libplacebo_source = crate_path
        .join("../../libplacebo")
        .canonicalize()
        .expect("vendored libplacebo source directory is missing");
    let vulkan_headers = libplacebo_source.join("3rdparty/Vulkan-Headers/include");
    write_system_pkg_config(&pkg_config_libdir, &vulkan, Some(&vulkan_headers));

    // PKG_CONFIG_LIBDIR replaces pkg-config's default search graph. Never
    // append the caller's PKG_CONFIG_PATH: optional host libraries must not
    // silently become dependencies of this static media stack.
    env::remove_var("PKG_CONFIG_PATH");
    env::set_var("PKG_CONFIG_LIBDIR", &pkg_config_libdir);

    let hardware = build_hardware_loaders(crate_path, out_path, &pkg_config_libdir);
    let libplacebo = build_vendored_libplacebo(&libplacebo_source, out_path);
    copy_pkg_config_files(
        &libplacebo.pkgconfig,
        &pkg_config_libdir,
        &["libplacebo.pc"],
    );

    let ffmpeg_lib = build_vendored_ffmpeg(crate_path, out_path);
    build_ffmpeg_thumbnail(crate_path, out_path, &ffmpeg_lib);
    copy_pkg_config_files(
        &ffmpeg_lib.join("pkgconfig"),
        &pkg_config_libdir,
        &[
            "libavcodec.pc",
            "libavfilter.pc",
            "libavformat.pc",
            "libavutil.pc",
            "libswresample.pc",
            "libswscale.pc",
        ],
    );

    let source = crate_path
        .join("../../mpv")
        .canonicalize()
        .expect("vendored mpv source directory is missing");
    let build = out_path.join("mpv-build");
    emit_rerun_tree(&source);

    let reconfigure = build.join("build.ninja").exists();
    if reconfigure {
        // Refresh Meson's project-option registry before setting newly added
        // options. Meson otherwise rejects a new -D option against an older
        // existing build directory before it has reread meson.options.
        run(
            Command::new("meson")
                .arg("setup")
                .arg(&build)
                .arg(&source)
                .arg("--reconfigure"),
            "refresh vendored libmpv configuration",
        );
    }

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
        .arg("-Dlibass=disabled")
        .arg("-Dlua=disabled")
        .arg("-Dshaderc=disabled")
        .arg("-Dvulkan=enabled")
        .arg("-Dcuda-hwaccel=enabled")
        .arg("-Dcuda-interop=enabled")
        .arg("-Dvaapi=enabled")
        .arg("-Dvaapi-drm=enabled")
        // Keep one broadly supported Linux audio path for attachment playback.
        // Desktop sound servers expose ALSA devices without adding their full
        // client and codec stacks to this binary's ELF dependencies.
        .arg("-Dalsa=enabled")
        .arg("-Dpulse=disabled");
    if reconfigure {
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
    copy_pkg_config_files(&pc_dir, &pkg_config_libdir, &["mpv.pc"]);

    let dependencies = pkg_config::Config::new()
        .cargo_metadata(false)
        .statik(false)
        .probe("mpv")
        .expect("could not resolve vendored libmpv's system dependencies");

    println!("cargo:rustc-link-search=native={}", build.display());
    println!(
        "cargo:rustc-link-search=native={}",
        libplacebo.library.display()
    );
    println!("cargo:rustc-link-search=native={}", ffmpeg_lib.display());
    println!(
        "cargo:rustc-link-search=native={}",
        hardware.library.display()
    );
    println!("cargo:rustc-link-lib=static=mpv");
    for path in dependencies.link_paths {
        if path.is_dir() {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }
    let mut linked_placebo = false;
    for library in dependencies.libs {
        match library.as_str() {
            "mpv" | "chatt_vaapi_loader" => {}
            "avcodec" | "avfilter" | "avformat" | "avutil" | "swresample" | "swscale" => {
                println!("cargo:rustc-link-lib=static={library}");
            }
            "placebo" => {
                println!("cargo:rustc-link-lib=static=placebo");
                linked_placebo = true;
            }
            "asound" | "vulkan" | "dl" | "m" | "pthread" | "rt" | "atomic" => {
                println!("cargo:rustc-link-lib=dylib={library}");
            }
            unexpected => panic!(
                "isolated vendored libmpv graph exposed unexpected native library {unexpected:?}"
            ),
        }
    }
    assert!(linked_placebo, "vendored mpv did not resolve static libplacebo");
    for path in dependencies.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in dependencies.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=static=chatt_vaapi_loader");
    println!("cargo:rustc-link-lib=dylib=dl");
}

#[cfg(feature = "vendored")]
fn build_ffmpeg_thumbnail(crate_path: &Path, out_path: &Path, ffmpeg_lib: &Path) {
    println!(
        "cargo:rerun-if-changed={}",
        crate_path.join("ffmpeg_thumbnail.c").display()
    );
    let include = ffmpeg_lib
        .parent()
        .expect("vendored FFmpeg library directory has no parent")
        .join("include");
    cc::Build::new()
        .file(crate_path.join("ffmpeg_thumbnail.c"))
        .include(include)
        .warnings(true)
        .extra_warnings(true)
        .out_dir(out_path)
        .compile("chatt_ffmpeg_thumbnail");
}

#[cfg(feature = "vendored")]
struct HardwareBuild {
    library: PathBuf,
}

#[cfg(feature = "vendored")]
struct LibplaceboBuild {
    library: PathBuf,
    pkgconfig: PathBuf,
}

#[cfg(feature = "vendored")]
struct SystemPackage {
    name: &'static str,
    version: String,
    libdir: PathBuf,
    includedir: PathBuf,
}

#[cfg(feature = "vendored")]
fn discover_system_package(name: &'static str) -> SystemPackage {
    let query = |argument: &str| {
        let output = Command::new("pkg-config")
            .env_remove("PKG_CONFIG_PATH")
            .env_remove("PKG_CONFIG_LIBDIR")
            .arg(argument)
            .arg(name)
            .output()
            .unwrap_or_else(|error| panic!("failed to query system {name}.pc: {error}"));
        assert!(
            output.status.success(),
            "required system package {name} is unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("pkg-config returned non-UTF-8 output")
            .trim()
            .to_owned()
    };

    SystemPackage {
        name,
        version: query("--modversion"),
        libdir: query("--variable=libdir").into(),
        includedir: query("--variable=includedir").into(),
    }
}

#[cfg(feature = "vendored")]
fn write_system_pkg_config(
    pkgconfig: &Path,
    package: &SystemPackage,
    include_override: Option<&Path>,
) {
    let includedir = include_override.unwrap_or(&package.includedir);
    let library = match package.name {
        "alsa" => "asound",
        "vulkan" => "vulkan",
        _ => unreachable!("only approved system packages may enter the isolated graph"),
    };
    write_pkg_config(
        &pkgconfig.join(format!("{}.pc", package.name)),
        package.name,
        &package.version,
        includedir,
        &format!("-I{}", includedir.display()),
        &format!("-L{} -l{library}", package.libdir.display()),
        None,
    );
}

#[cfg(feature = "vendored")]
fn copy_pkg_config_files(source: &Path, destination: &Path, filenames: &[&str]) {
    for filename in filenames {
        let path = source.join(filename);
        let output = destination.join(filename);
        fs::copy(&path, &output).unwrap_or_else(|error| {
            panic!(
                "could not copy {} to {}: {error}",
                path.display(),
                output.display()
            )
        });
    }
}

#[cfg(feature = "vendored")]
fn clear_pkg_config_files(directory: &Path) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("could not read isolated pkg-config entry").path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("pc") {
            fs::remove_file(&path).unwrap_or_else(|error| {
                panic!("could not clear stale metadata {}: {error}", path.display())
            });
        }
    }
}

#[cfg(feature = "vendored")]
fn build_vendored_libplacebo(source: &Path, out_path: &Path) -> LibplaceboBuild {
    let build = out_path.join("libplacebo-build");
    let install = out_path.join("libplacebo-install");
    emit_rerun_tree(source);

    let reconfigure = build.join("build.ninja").exists();
    if reconfigure {
        run(
            Command::new("meson")
                .arg("setup")
                .arg(&build)
                .arg(source)
                .arg("--reconfigure"),
            "refresh vendored libplacebo configuration",
        );
    }

    let mut setup = Command::new("meson");
    setup
        .arg("setup")
        .arg(&build)
        .arg(source)
        .arg(format!("--prefix={}", install.display()))
        .arg("--libdir=lib")
        .arg("--wrap-mode=nofallback")
        .arg("--default-library=static")
        .arg("--buildtype=release")
        .arg("-Dauto_features=disabled")
        .arg("-Dprefer_static=true")
        .arg("-Db_staticpic=true")
        .arg("-Dc_args=-ffunction-sections -fdata-sections")
        .arg("-Dvulkan=enabled")
        .arg("-Dvk-proc-addr=disabled")
        .arg("-Dnaga=enabled")
        .arg("-Drust-num-convert=enabled")
        .arg("-Dopengl=disabled")
        .arg("-Dd3d11=disabled")
        .arg("-Dshaderc=disabled")
        .arg("-Dglslang=disabled")
        .arg("-Dlcms=disabled")
        .arg("-Ddovi=disabled")
        .arg("-Dlibdovi=disabled")
        .arg("-Dunwind=disabled")
        .arg("-Dxxhash=disabled")
        .arg("-Ddemos=false")
        .arg("-Dtests=false")
        .arg("-Dbench=false")
        .arg("-Dfuzz=false")
        .arg("-Ddebug-abort=false");
    if reconfigure {
        setup.arg("--reconfigure");
    }
    run(&mut setup, "configure vendored libplacebo");

    let mut compile = Command::new("meson");
    compile.arg("compile").arg("-C").arg(&build);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        compile.arg("-j").arg(jobs);
    }
    run(&mut compile, "compile vendored libplacebo");
    run(
        Command::new("meson").arg("install").arg("-C").arg(&build),
        "install vendored libplacebo",
    );

    let library = install.join("lib");
    let archive = library.join("libplacebo.a");
    assert!(
        archive.is_file(),
        "Meson did not produce {}",
        archive.display()
    );
    LibplaceboBuild {
        pkgconfig: library.join("pkgconfig"),
        library,
    }
}

#[cfg(feature = "vendored")]
fn build_hardware_loaders(
    crate_path: &Path,
    out_path: &Path,
    pkgconfig: &Path,
) -> HardwareBuild {
    let vendor = crate_path
        .join("../../hw-headers")
        .canonicalize()
        .expect("vendored hardware headers are missing");
    let va_include = vendor.join("libva/include");
    let nv_include = vendor.join("nv-codec/include");
    let loader = crate_path.join("loader");
    let library = out_path.join("hardware-loader");
    emit_rerun_tree(&vendor);
    emit_rerun_tree(&loader);

    cc::Build::new()
        .cargo_metadata(false)
        .file(loader.join("vaapi_loader.c"))
        .include(&va_include)
        .include(&loader)
        .out_dir(&library)
        .flag_if_supported("-std=c11")
        .compile("chatt_vaapi_loader");

    write_pkg_config(
        &pkgconfig.join("libva.pc"),
        "libva",
        "1.24.0",
        &va_include,
        &format!(
            "-I{} -I{} -DCHATT_VAAPI_LAZY_LOADER=1",
            va_include.display(),
            loader.display()
        ),
        &format!("-L{} -lchatt_vaapi_loader -ldl", library.display()),
        None,
    );
    write_pkg_config(
        &pkgconfig.join("libva-drm.pc"),
        "libva-drm",
        "1.24.0",
        &va_include,
        &format!("-I{}", va_include.display()),
        "",
        Some("libva"),
    );
    write_pkg_config(
        &pkgconfig.join("ffnvcodec.pc"),
        "ffnvcodec",
        "12.2.72.0",
        &nv_include,
        &format!("-I{}", nv_include.display()),
        "",
        None,
    );

    HardwareBuild { library }
}

#[cfg(feature = "vendored")]
fn write_pkg_config(
    path: &Path,
    name: &str,
    version: &str,
    includedir: &Path,
    cflags: &str,
    libs: &str,
    requires: Option<&str>,
) {
    let requires = requires
        .map(|value| format!("Requires: {value}\n"))
        .unwrap_or_default();
    fs::write(
        path,
        format!(
            "prefix=/\nincludedir={}\nName: {name}\nDescription: chatt-gui build-only {name}\nVersion: {version}\n{requires}Cflags: {cflags}\nLibs: {libs}\n",
            includedir.display()
        ),
    )
    .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
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
        .arg("--enable-vulkan")
        .arg("--enable-ffnvcodec")
        .arg("--enable-cuda")
        .arg("--enable-nvdec")
        .arg("--enable-vaapi")
        .arg("--enable-decoder=aac,aac_fixed,aac_latm,ac3,alac,av1,dca,eac3,flac,gif,h264,hevc,mjpeg,mp3,mp3float,opus,pcm_alaw,pcm_f32le,pcm_f64le,pcm_mulaw,pcm_s16be,pcm_s16le,pcm_s24be,pcm_s24le,pcm_s32be,pcm_s32le,pcm_u8,png,vorbis,vp8,vp9,webp")
        .arg("--enable-hwaccel=av1_nvdec,av1_vaapi,av1_vulkan,h264_nvdec,h264_vaapi,h264_vulkan,hevc_nvdec,hevc_vaapi,hevc_vulkan,mjpeg_nvdec,mjpeg_vaapi,vp8_nvdec,vp8_vaapi,vp9_nvdec,vp9_vaapi,vp9_vulkan")
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
