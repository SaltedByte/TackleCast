fn main() {
    // Rebuild the embedded identity after commits or source edits. Packaging
    // records the same HEAD in build-info.json; dirty builds identify themselves.
    for path in [
        ".git/HEAD",
        ".git/index",
        ".git/refs",
        ".git/packed-refs",
        "src",
        "build.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown-source".into());
    let dirty = git(&["diff", "--name-only", "HEAD"]).is_none_or(|s| !s.is_empty());
    println!(
        "cargo:rustc-env=TACKLECAST_BUILD_ID={commit}{}",
        if dirty { "-dirty" } else { "" }
    );
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rerun-if-changed=src/capabilities/v4l2.c");
        cc::Build::new()
            .file("src/capabilities/v4l2.c")
            .compile("tacklecast_v4l2_caps");
    }
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("cargo:warning=Failed to compile Windows resource: {e}");
        }
    }
}
