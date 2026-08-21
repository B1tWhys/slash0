use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds the wasm client (and, transitively via the client's own build script,
/// the rust-gpu shader) into `crates/client/dist`, which the server serves at
/// runtime. Runs as part of every server build so the served assets can never
/// drift out of sync with the server binary.
fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("server crate lives at <root>/crates/server")
        .to_path_buf();

    // Re-run trunk whenever anything that feeds the wasm bundle changes. A pure
    // server-code edit touches none of these, so it skips the client rebuild.
    for input in [
        "crates/client/src",
        "crates/client/index.html",
        "crates/client/Trunk.toml",
        "crates/client/Cargo.toml",
        "crates/client/Cargo.lock",
        "crates/shader/src",
        "crates/shader/Cargo.toml",
        "crates/core/src",
        "crates/core/Cargo.toml",
    ] {
        println!(
            "cargo::rerun-if-changed={}",
            workspace_root.join(input).display()
        );
    }

    let client_dir = workspace_root.join("crates/client");
    let mut trunk = Command::new("trunk");
    trunk.arg("build").arg("--release").current_dir(&client_dir);

    let status = trunk.status().unwrap_or_else(|err| {
        panic!(
            "failed to run `trunk build` in {} ({err}). Install trunk with `cargo install trunk`.",
            client_dir.display()
        )
    });
    assert!(status.success(), "`trunk build` failed with {status}");
}
