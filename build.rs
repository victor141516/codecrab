use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    println!("cargo:rerun-if-changed=assets/codecrab.ico");
    embed_windows_resources();

    let web = manifest.join("web");
    for path in [
        "package.json",
        "package-lock.json",
        "index.html",
        "vite.config.js",
        "src/App.vue",
        "src/main.js",
        "src/pwa.js",
        "src/markdown.js",
        "src/style.css",
        "src/editor-panel.js",
        "../code-server-extension/package.json",
        "../code-server-extension/extension.js",
        "pwa/manifest.webmanifest",
        "pwa/service-worker.js",
        "pwa/icon-32.png",
        "pwa/icon-192.png",
        "pwa/icon-512.png",
    ] {
        println!("cargo:rerun-if-changed=web/{path}");
    }

    if !web.join("node_modules").is_dir() {
        run_npm(&web, &["ci"]);
    }
    run_npm(&web, &["run", "build"]);

    let dist = web.join("dist");
    let embedded =
        PathBuf::from(env::var_os("OUT_DIR").expect("build output directory")).join("web");
    fs::create_dir_all(&embedded).expect("create embedded web directory");
    for file in ["index.html", "app.js", "app.css"] {
        let source = dist.join(file);
        if !source.is_file() {
            panic!("web build did not produce {}", source.display());
        }
        fs::copy(&source, embedded.join(file)).expect("copy embedded web asset");
    }
    for file in [
        "manifest.webmanifest",
        "service-worker.js",
        "icon-32.png",
        "icon-192.png",
        "icon-512.png",
    ] {
        let source = web.join("pwa").join(file);
        fs::copy(&source, embedded.join(file)).expect("copy embedded PWA asset");
    }

    let mut produced = fs::read_dir(&dist)
        .expect("read web build output")
        .map(|entry| entry.expect("read web build entry").file_name())
        .collect::<Vec<_>>();
    produced.sort();
    let expected = ["app.css", "app.js", "index.html"];
    assert_eq!(
        produced
            .iter()
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>(),
        expected,
        "the web build must contain exactly one HTML, one JavaScript, and one CSS file"
    );
}

fn embed_windows_resources() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    winresource::WindowsResource::new()
        .set_icon("assets/codecrab.ico")
        .compile()
        .expect("compile Windows application resources");
}

fn run_npm(directory: &Path, args: &[&str]) {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let status = Command::new(npm)
        .args(args)
        .current_dir(directory)
        .status()
        .unwrap_or_else(|error| panic!("cannot run npm: {error}"));
    if !status.success() {
        panic!("npm {} failed with {status}", args.join(" "));
    }
}
