use std::{env, fs, io::Write, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=migrations");

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

    // Get the latest commit hash for the ui submodule
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&ui_dir)
        .output()
        .expect("Failed to get git commit hash for ui submodule");
    let current_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Read the last built commit hash
    let hash_file = workspace_root.join(".last_build_commit");
    let last_hash = fs::read_to_string(&hash_file).unwrap_or_default();

    if last_hash != current_hash {
        println!("Building UI: new commit detected.");

        // Install/update npm dependencies
        println!("Installing npm dependencies...");
        let install_status = Command::new("npm")
            .args(["install", "--engine-strict=false"])
            .current_dir(&ui_dir)
            .status()
            .expect("Failed to run npm install");
        assert!(install_status.success(), "npm install failed");

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

        // Generate UI version file for runtime access
        generate_ui_version_file(workspace_root);
    } else {
        println!("cargo:warning=UI unchanged, skipping build");
    }
}

fn generate_ui_version_file(workspace_root: &std::path::Path) {
    let ui_dir = workspace_root.join("ui");

    // Get UI version
    let version_output = Command::new("git")
        .args([
            "-C",
            ui_dir.to_str().unwrap(),
            "describe",
            "--tags",
            "--abbrev=0",
        ])
        .output()
        .expect("Failed to get UI version");
    let ui_version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .trim_start_matches('v')
        .to_string();

    // Get UI commit hash
    let commit_output = Command::new("git")
        .args(["-C", ui_dir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .expect("Failed to get UI commit hash");
    let ui_commit = String::from_utf8_lossy(&commit_output.stdout)
        .trim()
        .to_string();

    // Write version file
    let version_content = format!("UI_VERSION={}\nUI_COMMIT={}\n", ui_version, ui_commit);

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("static");
    let version_file_in_dist = dist_dir.join("ui-version.env");
    fs::write(&version_file_in_dist, version_content)
        .expect("Failed to write ui-version.env in static/");

    println!(
        "Generated ui-version.env with UI_VERSION={} UI_COMMIT={}",
        ui_version, ui_commit
    );
}
