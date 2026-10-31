use std::{env, fs, path::PathBuf};

const PLACEHOLDER: &str = "<!doctype html><html><body><h1>Jellyswarrm development build</h1><p>The web client is not built. Run <code>just ui</code> and rebuild.</p></body></html>\n";

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=static");
    println!("cargo:rerun-if-env-changed=JELLYSWARRM_ALLOW_PLACEHOLDER_UI");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let static_dir = manifest_dir.join("static");
    let index = static_dir.join("index.html");

    // rust-embed requires an existing directory, even for check/test builds.
    if !index.is_file() {
        fs::create_dir_all(&static_dir).expect("Failed to create static/");
        fs::write(&index, PLACEHOLDER).expect("Failed to write development UI placeholder");
    }

    let contents = fs::read_to_string(&index).expect("Failed to read static/index.html");
    let placeholder = contents == PLACEHOLDER || contents.trim().is_empty();
    if placeholder {
        let release = env::var("PROFILE").as_deref() == Ok("release");
        let allow_placeholder = env::var("JELLYSWARRM_ALLOW_PLACEHOLDER_UI").as_deref() == Ok("1");
        assert!(
            !release || allow_placeholder,
            "Release builds require the web client. Run `just ui` first. For deliberately UI-less checks only, set JELLYSWARRM_ALLOW_PLACEHOLDER_UI=1."
        );
        println!("cargo:warning=Web client not built; embedding a development placeholder. Run `just ui` to build it.");
    }
}
