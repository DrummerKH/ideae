//! Puts the git commit into `ideae-server --version`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=IDEAE_COMMIT={commit}");
}
