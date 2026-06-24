use std::process::Command;

/// Stamp the build with the git short SHA + a UTC build timestamp so the running
/// app can log exactly which build it is (observability: answers "am I on the
/// latest build?" from data instead of inference).
fn main() {
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());

    // `git status --porcelain` non-empty => uncommitted changes ("+dirty").
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let git_sha = if dirty {
        format!("{git_sha}+dirty")
    } else {
        git_sha
    };

    let build_time = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=GIT_SHA={git_sha}");
    println!("cargo:rustc-env=BUILD_TIME={build_time}");
    // Re-run (refresh the stamp) when HEAD moves — a new commit or checkout.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    tauri_build::build();
}
