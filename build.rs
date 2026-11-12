use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let feature = |name: &str| env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    // Explicit rerun triggers replace Cargo's default of rerunning on any change.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if target_os == "linux" {
        println!("cargo:rerun-if-changed=src/capabilities/v4l2.c");
        cc::Build::new()
            .file("src/capabilities/v4l2.c")
            .compile("tacklecast_v4l2_caps");
    }
    if target_os == "windows" {
        if feature("GPU_DECODE") {
            build_mf_bridge(&target_env);
        }
        if feature("STATIC_FFMPEG") {
            link_static_ffmpeg_dependencies(&target_env);
        }
        write_gpu_runtime_bundle(feature("EMBED_GPU_RUNTIME"));
    }
    write_license_notice(
        feature("STATIC_FFMPEG"),
        target_os == "windows" && feature("EMBED_GPU_RUNTIME"),
    );
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("cargo:warning=Failed to compile Windows resource: {e}");
        }
    }
}

/// Compile the Media Foundation GPU bridge straight into the executable.
fn build_mf_bridge(target_env: &str) {
    println!("cargo:rerun-if-changed=src/mf_gpu_native.cpp");
    let mut build = cc::Build::new();
    build.cpp(true).file("src/mf_gpu_native.cpp");
    // Same flags as the bridge's former DLL build: strict on GCC, /W4 on MSVC.
    if target_env == "msvc" {
        build.flag("/std:c++17").flag("/EHsc").flag("/W4");
    } else {
        // Link libstdc++ statically so the MinGW build needs no runtime DLLs.
        build
            .flag("-std=c++17")
            .flag("-Wextra")
            .warnings_into_errors(true)
            .cpp_link_stdlib(None);
        // Link arguments land after the bridge archive, as GNU ld requires.
        // Use this compiler's own archives by path: rustc links with
        // -nodefaultlibs and puts its older bundled libgcc first on the search
        // path, which lacks symbols the matching libstdc++ needs.
        let compiler = build.get_compiler();
        let file = |name: &str| {
            let output = std::process::Command::new(compiler.path())
                .arg(format!("-print-file-name={name}"))
                .output()
                .expect("query the C++ compiler for its runtime libraries");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        for arg in [
            file("libstdc++.a"),
            file("libgcc_eh.a"),
            file("libgcc.a"),
            file("libwinpthread.a"),
        ] {
            println!("cargo:rustc-link-arg={arg}");
        }
        for lib in ["mingwex", "msvcrt", "kernel32"] {
            println!("cargo:rustc-link-arg=-l{lib}");
        }
    }
    build.compile("tacklecast_mf_gpu");
    for lib in [
        "mfplat",
        "mf",
        "mfreadwrite",
        "mfuuid",
        "ole32",
        "d3d11",
        "d3d12",
        "dxgi",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}

/// ffmpeg-sys-next links the static FFmpeg libraries from `FFMPEG_DIR` but not
/// the Windows libraries they call (DirectShow, COM, ...). FFmpeg records those
/// in its pkg-config files, so link exactly what this build of FFmpeg needs.
fn link_static_ffmpeg_dependencies(target_env: &str) {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let ffmpeg_dir = PathBuf::from(
        env::var_os("FFMPEG_DIR").expect("static-ffmpeg needs FFMPEG_DIR set to a static build"),
    );
    let pkgconfig = ffmpeg_dir.join("lib").join("pkgconfig");
    let own = [
        "avcodec",
        "avdevice",
        "avfilter",
        "avformat",
        "avutil",
        "swresample",
        "swscale",
    ];
    let mut libs: Vec<String> = Vec::new();
    for name in own {
        let path = pkgconfig.join(format!("lib{name}.pc"));
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}); FFMPEG_DIR must be a static FFmpeg build \
                 from scripts/build_static_ffmpeg.sh",
                path.display()
            )
        });
        // A static-only build lists its dependencies under Libs, not Libs.private.
        for line in text.lines() {
            let Some(flags) = line
                .strip_prefix("Libs:")
                .or_else(|| line.strip_prefix("Libs.private:"))
            else {
                continue;
            };
            for token in flags.split_whitespace() {
                let lib = token
                    .strip_prefix("-l")
                    .map(str::to_string)
                    .or_else(|| token.strip_suffix(".lib").map(str::to_string));
                let Some(lib) = lib else { continue };
                // The C math and atomics libraries are part of the MSVC CRT.
                let builtin = target_env == "msvc" && matches!(lib.as_str(), "m" | "atomic");
                if !builtin && !own.contains(&lib.as_str()) && !libs.contains(&lib) {
                    libs.push(lib);
                }
            }
        }
    }
    for lib in libs {
        if target_env == "msvc" {
            println!("cargo:rustc-link-lib=dylib={lib}");
        } else {
            // GNU ld resolves left to right, so these must follow the FFmpeg
            // archives, which link arguments do.
            println!("cargo:rustc-link-arg=-l{lib}");
        }
    }
}

/// With `embed-gpu-runtime`, store the nvJPEG and CUDA runtime DLLs inside the
/// executable. gpu_decode.rs unpacks them on first use. Without the feature an
/// empty bundle is generated so the include always resolves.
fn write_gpu_runtime_bundle(embed: bool) {
    println!("cargo:rerun-if-env-changed=GPU_RUNTIME_DIR");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if embed {
        let dir = env::var_os("GPU_RUNTIME_DIR").map(PathBuf::from).expect(
            "embed-gpu-runtime needs GPU_RUNTIME_DIR \
             (scripts/setup_windows_dependencies.ps1 -IncludeGpuRuntime sets it)",
        );
        files = find_gpu_runtime(&dir).unwrap_or_else(|| {
            panic!(
                "GPU_RUNTIME_DIR ({}) has no matching nvjpeg64_N.dll and CUDA runtime pair",
                dir.display()
            )
        });
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut source = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        for byte in fs::read(path).expect("read GPU runtime DLL") {
            hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
        }
        let _ = writeln!(source, "    ({name:?}, include_bytes!({:?})),", path);
    }
    source.push_str("];\n");
    let _ = writeln!(
        source,
        "/// Names the unpack directory, so a different runtime never reuses stale files.\n\
         pub const BUNDLE_ID: &str = \"{}-{hash:016x}\";",
        env::var("CARGO_PKG_VERSION").unwrap_or_default()
    );
    fs::write(out_dir.join("gpu_runtime.rs"), source).expect("write gpu_runtime.rs");
}

/// License text for everything linked or embedded into this build, shown in
/// Settings > Display > About. A standalone EXE carries no separate files, so
/// the notices FFmpeg (LGPL) and NVIDIA require must travel inside it.
fn write_license_notice(static_ffmpeg: bool, gpu_runtime: bool) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let mut sections: Vec<(String, PathBuf)> =
        vec![("TackleCast".into(), PathBuf::from("LICENSE"))];
    if static_ffmpeg {
        if let Some(dir) = env::var_os("FFMPEG_DIR") {
            sections.push((
                "FFmpeg (GNU LGPL 2.1 or later; source: https://ffmpeg.org)".into(),
                PathBuf::from(dir).join("LICENSE.txt"),
            ));
        }
    }
    if gpu_runtime {
        if let Some(dir) = env::var_os("GPU_RUNTIME_DIR") {
            let mut licenses: Vec<PathBuf> = fs::read_dir(PathBuf::from(dir).join("licenses"))
                .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            licenses.sort();
            for path in licenses {
                sections.push(("NVIDIA CUDA runtime and nvJPEG".into(), path));
            }
        }
    }

    let mut notice = String::new();
    let mut seen: Vec<String> = Vec::new();
    for (title, path) in sections {
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("license file {} is missing: {error}", path.display()));
        // The CUDA runtime and nvJPEG share one EULA; show it once.
        if seen.contains(&text) {
            continue;
        }
        let _ = write!(notice, "==== {title} ====\n\n{}\n\n", text.trim());
        seen.push(text);
    }
    fs::write(out_dir.join("third_party_licenses.txt"), notice).expect("write license notice");
}

/// The newest nvJPEG with a CUDA runtime of the same major version.
fn find_gpu_runtime(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    for major in [13, 12, 11] {
        let jpeg = format!("nvjpeg64_{major}.dll");
        let runtime = if major == 11 {
            "cudart64_110.dll".to_string()
        } else {
            format!("cudart64_{major}.dll")
        };
        if dir.join(&jpeg).is_file() && dir.join(&runtime).is_file() {
            return Some(vec![
                (runtime.clone(), dir.join(&runtime)),
                (jpeg.clone(), dir.join(&jpeg)),
            ]);
        }
    }
    None
}
