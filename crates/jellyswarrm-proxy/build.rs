use std::{env, fs, io::Write, path::PathBuf, process::Command};

fn main() {
    if std::env::var("JELLYSWARRM_SKIP_UI").ok().as_deref() == Some("1") {
        println!("cargo:warning=Skipping internal UI build (JELLYSWARRM_SKIP_UI=1)");
        return;
    }
    // Get the path to the crate's directory
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Assume workspace root is two levels up from this crate (adjust if needed)
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();

    let ui_dir = workspace_root.join("ui");
    let dist_dir = manifest_dir.join("static"); // static/ in the crate

    // Get the latest commit hash for the ui directory
    let output = Command::new("git")
        .args([
            "log",
            "-n",
            "1",
            "--pretty=format:%H",
            "--",
            ui_dir.to_str().unwrap(),
        ])
        .current_dir(workspace_root)
        .output()
        .expect("Failed to get git commit hash for ui directory");
    let current_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Read the last built commit hash
    let hash_file = workspace_root.join(".last_build_commit");
    let last_hash = fs::read_to_string(&hash_file).unwrap_or_default();

    if last_hash != current_hash {
        println!("Building UI: new commit detected.");
        // Build the UI
        let status = Command::new("npm")
            .args(["run", "build:production"])
            .current_dir(&ui_dir)
            .status()
            .expect("Failed to run npm build");
        assert!(status.success(), "UI build failed");

        // Copy dist/* to static/
        let src = ui_dir.join("dist");
        let dst = &dist_dir;

        if dst.exists() {
            fs::remove_dir_all(dst).expect("Failed to remove old static dir");
        }
        fs_extra::dir::copy(
            &src,
            dst,
            &fs_extra::dir::CopyOptions::new().content_only(true),
        )
        .expect("Failed to copy dist to static");

        // Save the new commit hash
        let mut file = fs::File::create(&hash_file).expect("Failed to write hash file");
        file.write_all(current_hash.as_bytes())
            .expect("Failed to write hash");
    } else {
        println!("cargo:warning=UI unchanged, skipping build");
    }
}
