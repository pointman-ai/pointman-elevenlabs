//! ElevenLabs' real CLI under the extension, against two local servers and no key of anyone's: the CLI reads a
//! `.env` in its working folder and the folders above it, so one there (in the job's folder and above it) says to
//! send everything, key and all, to a decoy. The call must still go to the pinned address with the vault's key.
//!
//! It needs the real `elevenlabs`: ELEVENLABS_REAL_CLI (the test workflow downloads ElevenLabs' release), else
//! Homebrew's or one on the PATH. Without one it says so and passes, unless ELEVENLABS_REAL_CLI names one.

mod common;

use common::*;
use pointman_extension::testing::{RunOptions, StandIn};
use serde_json::json;

fn real_cli() -> Option<std::path::PathBuf> {
    if let Some(given) = std::env::var_os("ELEVENLABS_REAL_CLI").filter(|g| !g.is_empty()) {
        let given = std::path::PathBuf::from(given);
        assert!(given.is_file(), "ELEVENLABS_REAL_CLI is {}, which isn't there", given.display());
        return Some(given);
    }
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("elevenlabs")).collect::<Vec<_>>())
        .unwrap_or_default();
    ["/opt/homebrew/bin/elevenlabs", "/usr/local/bin/elevenlabs"]
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
}

#[test]
fn a_dotenv_above_the_job_cannot_send_the_key_elsewhere() {
    let Some(cli) = real_cli() else {
        eprintln!("skipped: no real `elevenlabs` here (set ELEVENLABS_REAL_CLI)");
        return;
    };
    let pinned = Server::start();
    pinned.serve(
        "/v1/user/subscription",
        json!({"tier": "creator", "character_count": 1000, "character_limit": 100000}).to_string(),
    );
    let decoy = Server::start();
    decoy.serve(
        "/v1/user/subscription",
        json!({"tier": "decoy", "character_count": 0, "character_limit": 1}).to_string(),
    );

    let node = StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .settings(json!({"cli": cli.to_string_lossy(), "base_url": pinned.url}))
        .secret("elevenlabs.main", KEY)
        .start()
        .unwrap();
    let dotenv = format!(
        "ELEVENLABS_API_KEY=sk_fromdotenv000000000000000000\nELEVENLABS_BASE_URL={0}\nELEVENLABS_PROXY={0}\n\
         HTTPS_PROXY={0}\nHTTP_PROXY={0}\nALL_PROXY={0}\n",
        decoy.url
    );
    let job = node.work_dir().join("dotenv");
    std::fs::create_dir_all(&job).unwrap();
    for folder in [job.as_path(), node.work_dir(), node.state_dir_path()] {
        std::fs::write(folder.join(".env"), &dotenv).unwrap();
    }

    let options = RunOptions { cid: Some("dotenv".into()), ..Default::default() };
    let done = node.run_with("elevenlabs.usage", json!({}), options).unwrap();
    assert_eq!((done.result["tier"].as_str(), done.result["used"].as_i64()), (Some("creator"), Some(1000)));
    let asked = pinned.asked.lock().unwrap().clone();
    assert_eq!(asked, [("GET /v1/user/subscription HTTP/1.1".to_string(), Some(KEY.to_string()))]);
    assert!(decoy.asked.lock().unwrap().is_empty(), "the decoy was asked: {:?}", decoy.asked.lock().unwrap());
}
