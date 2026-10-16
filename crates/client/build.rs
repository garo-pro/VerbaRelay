//! Embeds build metadata used by the updater.
//!
//! Environment variables read at build time (all optional):
//! * `VERBARELAY_COMMIT`: commit hash of this build (CI sets it; falls back to GITHUB_SHA).
//! * `VERBARELAY_CHANNEL`: "stable" or "dev".
//! * `VERBARELAY_UPDATE_REPO`: GitHub "owner/repo" that publishes releases.
//! * `VERBARELAY_UPDATE_PUBKEY`: minisign public key (base64) that signs releases.
//!
//! Without a repository and a public key, automatic updates are disabled.
//!
//! On Windows it also embeds an application manifest. wxWidgets requires the
//! Common Controls v6 dependency it declares and warns at startup without it.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("VerbaRelay.Client"))
            .expect("unable to embed the Windows application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
    for var in [
        "VERBARELAY_COMMIT",
        "GITHUB_SHA",
        "VERBARELAY_CHANNEL",
        "VERBARELAY_UPDATE_REPO",
        "VERBARELAY_UPDATE_PUBKEY",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let commit = std::env::var("VERBARELAY_COMMIT")
        .or_else(|_| std::env::var("GITHUB_SHA"))
        .unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=VERBARELAY_BUILD_COMMIT={commit}");
    let channel = std::env::var("VERBARELAY_CHANNEL").unwrap_or_else(|_| "stable".into());
    println!("cargo:rustc-env=VERBARELAY_BUILD_CHANNEL={channel}");
    println!(
        "cargo:rustc-env=VERBARELAY_BUILD_REPO={}",
        std::env::var("VERBARELAY_UPDATE_REPO").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=VERBARELAY_BUILD_PUBKEY={}",
        std::env::var("VERBARELAY_UPDATE_PUBKEY").unwrap_or_default()
    );
}
