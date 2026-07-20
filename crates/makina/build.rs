use std::process::Command;

fn main() {
    // Re-run when HEAD moves so the stamped sha stays current.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        && out.status.success()
        && let Ok(sha) = String::from_utf8(out.stdout)
    {
        let sha = sha.trim();
        if !sha.is_empty() {
            println!("cargo:rustc-env=MAKINA_GIT_SHA={sha}");
        }
    }
    // When git is unavailable the env var is simply unset; option_env! yields None.
}
