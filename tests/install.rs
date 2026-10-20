//! A machine without the CLI: the extension installs ElevenLabs' release build into its state folder, from a
//! fake GitHub, only when the archive matches its published sha256.

mod common;

use std::time::{Duration, Instant};

use common::*;
use pointman_extension::testing::StandIn;
use serde_json::{json, Value};

/// The extension on a machine with no `elevenlabs` anywhere it looks, and releases from `releases`.
fn bare(releases: &str) -> StandIn {
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .secret("elevenlabs.main", KEY)
        .env("PATH", "/usr/bin:/bin")
        .env("POINTMAN_ELEVENLABS_PLACES", "")
        .env("POINTMAN_ELEVENLABS_RELEASES", releases)
        .start()
        .unwrap()
}

/// health.check's `cli` part, once `test` passes (the install runs as the extension starts).
fn cli_part_when(node: &StandIn, test: impl Fn(&Value) -> bool) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let cli = part(node, "cli");
        if test(&cli) || Instant::now() > until {
            return cli;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_missing_cli_is_installed_from_elevenlabs_release() {
    let server = Server::start();
    let node = bare(&release(&server, None));
    let cli = cli_part_when(&node, |p| p["state"] == "ok");
    assert_eq!((cli["state"].as_str(), cli["version"].as_str()), (Some("ok"), Some("9.9.9")), "{cli}");
    let why = cli["why"].as_str().unwrap();
    assert!(why.starts_with("9.9.9, ElevenLabs' release build, installed by the extension at"), "{why}");
    let installed = node.state_dir_path().join("cli/9.9.9/elevenlabs");
    assert!(installed.is_file());
    assert_eq!(std::fs::read_link(node.state_dir_path().join("cli/current")).unwrap(), std::path::Path::new("9.9.9"));

    // and the commands use it
    let done = node.run("elevenlabs.usage", json!({})).unwrap();
    assert_eq!(done.result["left"], 99_000);
    let calls = calls_in(&node.state_dir_path().join("cli/current/calls.log"));
    assert!(calls.iter().any(|c| c.is("user", "subscription") && c.key == KEY));
}

#[test]
fn a_bad_checksum_is_refused() {
    let server = Server::start();
    let node = bare(&release(&server, Some(&"0".repeat(64))));
    let cli = cli_part_when(&node, |p| p["why"].as_str().is_some_and(|w| w.contains("sha256")));
    assert_eq!(cli["state"], "failing");
    assert_eq!(cli["needs"], json!(["update"]));
    assert!(cli["why"].as_str().unwrap().contains("doesn't match its published sha256: not installed"), "{cli}");
    assert!(!node.state_dir_path().join("cli/current").exists(), "nothing installed");
    let failed = node.run("elevenlabs.usage", json!({})).unwrap_err();
    assert!(failed.message.contains("published sha256"), "{failed}");
}
