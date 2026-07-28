use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let web = manifest.join("web");
    for path in [
        "package.json",
        "package-lock.json",
        "index.html",
        "vite.config.js",
        "src/App.vue",
        "src/main.js",
        "src/markdown.js",
        "src/style.css",
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
