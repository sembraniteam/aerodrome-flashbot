//! Build script: embed the current commit hash for the L1 Live Lock.
//!
//! Sets `BUILD_COMMIT` to `git rev-parse HEAD` at compile time. When git is
//! unavailable (or the tree state cannot be read) it falls back to
//! `"unknown"` -- the live lock then refuses (fail closed) unless the
//! manifest names the same value, which a real bundle never does.

fn main() {
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    // Re-run when the commit changes (best effort; missing git = "unknown").
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
}
