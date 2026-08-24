//! Every crate that speaks HTTP must be built with a TLS backend.
//!
//! `reqwest` compiles happily with no TLS backend at all, and the result is not
//! a build error but a runtime one: it rejects any `https://` URL up front with
//! "invalid URL, scheme is not http", *before* opening a socket. Worse,
//! reqwest's `Display` repeats the URL you passed and buries that cause one
//! `source()` down, so the failure reads as a bad URL or a server that is down.
//! Velos shipped exactly that through v0.2.0: `veloslet` retried a join against
//! an https control plane forever, logging a URL-shaped error the whole time.
//!
//! This scans the workspace manifests rather than listing the crates it knows
//! about, so a crate that picks up `reqwest` later is covered the day it does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

/// Feature names that pull in a TLS backend. `default-tls` is an alias for
/// `rustls` in reqwest 0.13, and the `native-tls` family links the system
/// OpenSSL/Security framework; any one of them means https can work.
const TLS_FEATURES: &[&str] = &["rustls", "rustls-no-provider", "default-tls", "native-tls"];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/tests has a parent")
        .to_path_buf()
}

/// The `reqwest = ...` dependency line of a manifest, if it has one. Velos
/// declares each on a single line, which keeps this a string scan rather than a
/// TOML parse (and so free of a dependency this crate would otherwise not need).
fn reqwest_line(manifest: &str) -> Option<&str> {
    manifest
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("reqwest = ") || line.starts_with("reqwest.workspace"))
}

#[test]
fn every_reqwest_dependency_enables_tls() {
    let mut checked = Vec::new();
    let mut offenders = Vec::new();

    let entries = fs::read_dir(crates_dir()).expect("reading crates/");
    for entry in entries {
        let dir = entry.expect("reading a crates/ entry").path();
        let manifest_path = dir.join("Cargo.toml");
        let Ok(manifest) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Some(line) = reqwest_line(&manifest) else {
            continue;
        };
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        checked.push(name.clone());
        if !TLS_FEATURES.iter().any(|feat| {
            // Match the quoted feature so `rustls` does not also match
            // `rustls-tls-native-roots-no-provider` in a future rename.
            line.contains(&format!("\"{feat}\""))
        }) {
            offenders.push(format!("{name}: {line}"));
        }
    }

    // A scan that silently matched nothing would pass forever. Velos has at
    // least the CLI, the worker, and this crate speaking HTTP.
    assert!(
        checked.len() >= 3,
        "expected to scan at least 3 crates using reqwest, found {checked:?} — \
         has the manifest layout changed?"
    );
    assert!(
        offenders.is_empty(),
        "these crates depend on reqwest without a TLS backend, so every https:// \
         URL will fail with \"scheme is not http\" before a packet is sent:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn reqwest_line_is_found_and_judged() {
    // Guards the scan itself: if `reqwest_line` stopped matching, the test above
    // would find zero crates and the count assertion is what would catch it —
    // but only if these two halves genuinely disagree on a bad manifest.
    let with = "reqwest = { version = \"0.13\", features = [\"json\", \"rustls\"] }";
    let without =
        "reqwest = { version = \"0.13\", default-features = false, features = [\"json\"] }";
    assert_eq!(
        reqwest_line(&format!("[dependencies]\n{with}\n")),
        Some(with)
    );
    assert!(
        TLS_FEATURES
            .iter()
            .any(|f| with.contains(&format!("\"{f}\"")))
    );
    assert!(
        !TLS_FEATURES
            .iter()
            .any(|f| without.contains(&format!("\"{f}\"")))
    );
}
