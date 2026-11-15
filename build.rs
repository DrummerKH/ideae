//! Puts the git commit into the binary, for `--version` and the title bar's
//! new-build notice.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Twig has no grammar crate: its generated parser is vendored.
    println!("cargo:rerun-if-changed=vendor/tree-sitter-twig/src/parser.c");
    cc::Build::new()
        .include("vendor/tree-sitter-twig/src")
        .file("vendor/tree-sitter-twig/src/parser.c")
        .warnings(false)
        .compile("tree-sitter-twig");
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=IDEAE_COMMIT={commit}");
    // A new commit or checkout changes these.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
    if let Some(dir) = git(&["rev-parse", "--git-common-dir"]) {
        println!("cargo:rerun-if-changed={dir}/refs/heads");
        println!("cargo:rerun-if-changed={dir}/packed-refs");
    }
}
