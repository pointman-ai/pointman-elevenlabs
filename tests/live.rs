//! The extension against the real ElevenLabs, by hand: it spends credits (a 20 s isolate, a few hundred). It runs
//! under the stand-in node with this machine's `elevenlabs` and a key from the environment, which only someone
//! who holds the vault's key can give it, never written down:
//!
//! ```sh
//! cd ~/Documents/pointman-elevenlabs && pointman secrets run <ticket for ELEVENLABS_API_KEY> -- \
//!   env ELEVENLABS_LIVE_AUDIO=$HOME/Documents/pointman-elevenlabs-live/noisy-20s.wav \
//!   $HOME/.cargo/bin/cargo test --test live -- --ignored --nocapture
//! ```
//!
//! It prints what `elevenlabs.usage` and `elevenlabs.isolate` say, and copies the isolated audio beside the
//! input as `<name>-isolated.mp3`. It never prints the key.

mod common;

use common::files_under;
use pointman_extension::testing::StandIn;
use serde_json::json;

#[test]
#[ignore]
fn usage_then_a_real_isolate() {
    let key = std::env::var("ELEVENLABS_API_KEY").expect("ELEVENLABS_API_KEY, from `pointman secrets run`");
    let audio = std::path::PathBuf::from(std::env::var("ELEVENLABS_LIVE_AUDIO").expect("ELEVENLABS_LIVE_AUDIO"));
    let folder = audio.parent().unwrap().canonicalize().unwrap();
    let node = StandIn::new(common::ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .secret("elevenlabs.live", key.trim())
        .roots([folder.clone()])
        .start()
        .unwrap();

    let health = node.call("health.check", json!({})).unwrap();
    println!("health: {health:#}");
    let usage = node.run("elevenlabs.usage", json!({})).unwrap();
    println!("usage: {}", usage.result["text"].as_str().unwrap());

    let done =
        node.run_timeout("elevenlabs.isolate", json!({"file": audio}), std::time::Duration::from_secs(600)).unwrap();
    println!("isolate: {}", done.result["text"].as_str().unwrap());
    println!("progress: {:?}", done.progress.iter().filter_map(|p| p.stage.clone()).collect::<Vec<_>>());
    let out = &done.outputs[0];
    let kept = folder.join(std::path::Path::new(&out.path).file_name().unwrap());
    std::fs::copy(&out.path, &kept).unwrap();
    println!("isolated audio: {} ({} bytes)", kept.display(), std::fs::metadata(&kept).unwrap().len());

    let key = key.trim();
    assert!(
        !node.log().contains(key) && !done.result.to_string().contains(key) && !usage.result.to_string().contains(key)
    );
    for f in files_under(node.state_dir_path()) {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(!bytes.windows(key.len()).any(|w| w == key.as_bytes()), "the key is in {}", f.display());
    }
}
