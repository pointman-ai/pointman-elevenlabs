//! The commands against the stand-in node and the fake CLI: the key only in each call's environment, credits
//! read around each paid call, outputs posted back, `run` dry unless told, and deleting or cloning only with
//! `confirm`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use pointman_extension::testing::RunOptions;
use serde_json::{json, Value};

/// The key reaches the CLI as ELEVENLABS_API_KEY and nowhere else: not its args, not the log, not the answer,
/// not a file (no `.env`). The CLI runs in a folder of the extension's own, with only the node's few variables.
#[test]
fn the_key_is_in_the_calls_environment_only() {
    let fake = Fake::new();
    let (folder, file) = media();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[folder.path()]);
    let done = node.run("elevenlabs.isolate", json!({"file": file})).unwrap();

    let calls = fake.calls();
    let isolate = calls.iter().find(|c| c.is("audio-isolation", "convert")).expect("the isolate's call");
    assert_eq!(isolate.key, KEY);
    assert_eq!(isolate.via, "pointman-elevenlabs/0.1.0");
    for c in &calls {
        assert!(c.args.iter().all(|a| !a.contains(KEY)), "the key in args: {:?}", c.args);
        let state = node.state_dir_path().canonicalize().unwrap();
        let cwd = std::path::Path::new(&c.cwd).canonicalize().unwrap();
        assert!(cwd.starts_with(&state), "{} runs outside the extension's folders", c.cwd);
        for name in &c.env {
            let allowed = [
                "PATH",
                "HOME",
                "USER",
                "LANG",
                "LC_ALL",
                "TMPDIR",
                "NO_COLOR",
                "ELEVENLABS_VIA",
                "ELEVENLABS_API_KEY",
                "PWD",
                "SHLVL",
                "_",
                "OLDPWD",
            ];
            assert!(allowed.contains(&name.as_str()), "the CLI got {name}");
        }
    }
    assert!(!done.result.to_string().contains(KEY));
    assert!(!node.log().contains(KEY), "the key in the extension's log");
    for f in files_under(node.state_dir_path()) {
        assert_ne!(f.file_name().unwrap(), ".env", "{}", f.display());
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(!bytes.windows(KEY.len()).any(|w| w == KEY.as_bytes()), "the key is in {}", f.display());
    }
}

/// An isolate runs in the background, says how far it's got while ElevenLabs works, posts the clean audio, and
/// says what it cost.
#[test]
fn isolate_reports_progress_posts_the_audio_and_its_credits() {
    let fake = Fake::new();
    fake.takes(2.5);
    let (folder, file) = media();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[folder.path()]);
    let done = node.run("elevenlabs.isolate", json!({"file": file})).unwrap();

    assert!(done.progress.len() >= 2, "{:?}", done.progress);
    let stages: Vec<String> = done.progress.iter().filter_map(|p| p.stage.clone()).collect();
    assert!(stages[0].starts_with("isolating the voice in talk.wav"), "{stages:?}");
    assert!(stages.iter().any(|s| s.ends_with("so far")), "{stages:?}");
    assert_eq!(done.outputs.len(), 1);
    let out = &done.outputs[0];
    assert_eq!((out.kind.as_str(), out.role.as_str()), ("audio", "main"));
    assert!(out.path.ends_with("talk-isolated.mp3"), "{}", out.path.display());
    assert_eq!(std::fs::read(&out.path).unwrap(), b"ID3isolated");
    assert_eq!(out.meta["file"], "talk.wav");
    assert_eq!(done.result["credits"], json!({"used": 333, "left": 98_667, "shared": false}));
    let text = done.result["text"].as_str().unwrap();
    assert!(
        text.starts_with("Isolated the voice in talk.wav") && text.ends_with("Used 333 credits; 98,667 left."),
        "{text}"
    );
}

/// Cancelling an isolate stops the CLI at once.
#[test]
fn an_isolate_can_be_cancelled() {
    let fake = Fake::new();
    fake.takes(30.0);
    let (folder, file) = media();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[folder.path()]);
    let started = Instant::now();
    std::thread::scope(|s| {
        let running = s.spawn(|| {
            let options = RunOptions { cid: Some("iso".into()), ..Default::default() };
            node.run_with("elevenlabs.isolate", json!({"file": file}), options)
        });
        std::thread::sleep(Duration::from_millis(1500));
        node.cancel("iso").unwrap();
        let failed = running.join().unwrap().unwrap_err();
        assert!(failed.cancelled, "{failed:?}");
    });
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
}

/// A file outside the folders the node allows is refused before anything is sent.
#[test]
fn a_file_outside_the_extensions_folders_is_refused() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    let failed = node.run("elevenlabs.isolate", json!({"file": "/etc/hosts"})).unwrap_err();
    assert!(failed.message.contains("outside the folders"), "{failed}");
    assert!(fake.calls().iter().all(|c| !c.is("audio-isolation", "convert")));
}

/// With no key, health asks for one, and a command says how to add it without sending anything.
#[test]
fn no_key_is_health_failing_with_auth() {
    let fake = Fake::new();
    let (folder, file) = media();
    let node = node(&fake, &[], &[folder.path()]);
    let key = part(&node, "key");
    assert_eq!(key["state"], "failing");
    assert_eq!(key["needs"], json!(["auth"]));
    assert!(key["why"].as_str().unwrap().contains("/elevenlabs add"), "{key}");
    let failed = node.run("elevenlabs.isolate", json!({"file": file})).unwrap_err();
    assert!(failed.message.contains("/elevenlabs add"), "{failed}");
    assert!(fake.calls().iter().all(|c| !c.is("audio-isolation", "convert")));
}

/// A key ElevenLabs refuses: health asks for a new one, naming it, and the isolate isn't sent.
#[test]
fn a_refused_key_is_named() {
    let fake = Fake::new();
    let (folder, file) = media();
    let node = node(&fake, &[("elevenlabs.old", REFUSED)], &[folder.path()]);
    let key = part(&node, "key");
    assert_eq!((key["state"].as_str(), &key["needs"]), (Some("failing"), &json!(["auth"])));
    assert!(key["why"].as_str().unwrap().contains("refused the key in elevenlabs.old"), "{key}");
    let failed = node.run("elevenlabs.isolate", json!({"file": file})).unwrap_err();
    assert!(failed.message.contains("refused the key in elevenlabs.old"), "{failed}");
    assert!(fake.calls().iter().all(|c| !c.is("audio-isolation", "convert")), "nothing uploaded with a refused key");
}

/// Health: the CLI's version and where it came from, and the key's account with its credits.
#[test]
fn health_says_the_clis_version_and_the_accounts_credits() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY), ("elevenlabs.spare", "sk_other00000000000000000000")], &[]);
    let cli = part(&node, "cli");
    assert_eq!((cli["state"].as_str(), cli["version"].as_str()), (Some("ok"), Some("9.9.9")));
    assert!(cli["why"].as_str().unwrap().starts_with("9.9.9, the `cli` setting's"), "{cli}");
    let key = part(&node, "key");
    assert_eq!(key["state"], "ok");
    assert_eq!(key["why"], "elevenlabs.main: creator plan, 99,000 of 100,000 credits left");
}

#[test]
fn usage_says_credits_used_and_left() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    let done = node.run("elevenlabs.usage", json!({})).unwrap();
    assert_eq!((done.result["used"].as_i64(), done.result["left"].as_i64()), (Some(1000), Some(99_000)));
    let text = done.result["text"].as_str().unwrap();
    assert!(text.starts_with("ElevenLabs (elevenlabs.main, creator plan): 1,000 of 100,000 credits used"), "{text}");
    assert!(text.contains("They reset on 2030-01-01"), "{text}");
}

#[test]
fn voices_are_listed_and_tts_finds_one_by_name() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    let listed = node.run("elevenlabs.voices", json!({})).unwrap().result;
    assert_eq!(listed["voices"].as_array().unwrap().len(), 2);
    assert!(listed["text"].as_str().unwrap().contains("| Rachel | `21m00Tcm4TlvDq8ikWAM` | premade |"), "{listed}");

    let done = node.run("elevenlabs.tts", json!({"text": "Hello there, James.", "voice": "george"})).unwrap();
    assert_eq!(done.result["voice"], json!({"name": "George - warm", "id": "JBFqnCBsd6RMkjVDRZzb"}));
    let call = fake.calls().into_iter().find(|c| c.is("text-to-speech", "convert")).unwrap();
    assert!(call.args.contains(&"--voice-id=JBFqnCBsd6RMkjVDRZzb".to_string()), "{:?}", call.args);
    assert!(call.args.contains(&"--text=Hello there, James.".to_string()), "{:?}", call.args);
    assert_eq!(done.outputs[0].kind, "audio");
    assert_eq!(std::fs::read(&done.outputs[0].path).unwrap(), b"ID3speech");
    assert_eq!(done.result["credits"]["used"], 42);

    let unknown = node.run("elevenlabs.tts", json!({"text": "Hi", "voice": "Nobody"})).unwrap_err();
    assert!(unknown.message.contains("No voice called Nobody"), "{unknown}");
}

#[test]
fn stt_posts_the_transcript_and_align_the_word_timings() {
    let fake = Fake::new();
    let (folder, file) = media();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[folder.path()]);
    let done = node.run("elevenlabs.stt", json!({"file": file})).unwrap();
    assert_eq!((done.result["words"].as_u64(), done.result["language"].as_str()), (Some(2), Some("en")));
    let names: Vec<String> =
        done.outputs.iter().map(|o| o.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["talk-transcript.txt", "talk-transcript.json"]);
    assert!(done.outputs.iter().all(|o| o.kind == "document"));
    assert_eq!(std::fs::read_to_string(&done.outputs[0].path).unwrap(), "hello world\n");
    let call = fake.calls().into_iter().find(|c| c.is("speech-to-text", "convert")).unwrap();
    assert!(call.args.contains(&"--model-id=scribe_v1".to_string()));

    let aligned = node.run("elevenlabs.align", json!({"file": file, "text": "hello world"})).unwrap();
    assert_eq!(aligned.result["words"], 2);
    assert!(aligned.result["text"].as_str().unwrap().contains("hello 0.00–0.40"), "{}", aligned.result);
    let out: Value = serde_json::from_slice(&std::fs::read(&aligned.outputs[0].path).unwrap()).unwrap();
    assert_eq!(out["words"][1]["text"], "world");
}

/// `run` is a dry run unless told otherwise, and needs no key for one.
#[test]
fn run_is_a_dry_run_by_default() {
    let fake = Fake::new();
    let node = node(&fake, &[], &[]);
    let done = node.run("elevenlabs.run", json!({"args": ["voices", "delete", "21m00Tcm4TlvDq8ikWAM"]})).unwrap();
    assert_eq!(done.result["dry_run"], true);
    assert_eq!(done.result["output"]["dry_run"], true);
    assert!(
        done.result["text"].as_str().unwrap().starts_with("Dry run of `elevenlabs voices delete"),
        "{}",
        done.result
    );
    let call = fake.calls().into_iter().find(|c| c.is("voices", "delete")).unwrap();
    assert!(call.args.contains(&"--dry-run".to_string()), "{:?}", call.args);
    assert_eq!(call.key, "<none>");
}

/// Deleting or cloning for real needs confirm, and nothing is sent without it.
#[test]
fn deleting_or_cloning_needs_confirm() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    for args in [json!(["voices", "delete", "21m00Tcm4TlvDq8ikWAM"]), json!(["voices", "ivc", "create", "--name=Me"])] {
        let failed = node.run("elevenlabs.run", json!({"args": args, "dry_run": false})).unwrap_err();
        assert!(failed.message.contains("needs confirm: true"), "{failed}");
    }
    assert!(fake.calls().iter().all(|c| c.args.first().map(String::as_str) != Some("voices")), "nothing sent");

    let done = node
        .run(
            "elevenlabs.run",
            json!({"args": ["voices", "delete", "21m00Tcm4TlvDq8ikWAM"], "dry_run": false, "confirm": true}),
        )
        .unwrap();
    assert_eq!(done.result["dry_run"], false);
    let call = fake.calls().into_iter().find(|c| c.is("voices", "delete")).unwrap();
    assert!(!call.args.contains(&"--dry-run".to_string()));
    assert_eq!(call.key, KEY);
}

/// What `run` never passes on, even as a dry run: anything that shows or redirects the key, signing in, and
/// paths outside the extension's folders.
#[test]
fn run_refuses_what_would_leak_the_key_or_a_file() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    let refused = [
        (json!(["user", "subscription", "get", "--base-url=http://example.com"]), "--base-url"),
        (json!(["user", "subscription", "get", "--debug"]), "--debug"),
        (json!(["auth", "login"]), "auth"),
        (json!(["audio-isolation", "convert", "--audio=@/etc/hosts"]), "outside the folders"),
        (json!(["audio-isolation", "convert", "--audio", "../../../../../../etc/hosts"]), "outside the folders"),
    ];
    for (args, why) in refused {
        let failed = node.run("elevenlabs.run", json!({"args": args})).unwrap_err();
        assert!(failed.message.contains(why), "{args}: {failed}");
    }
    assert!(fake.calls().iter().all(|c| c.args.iter().all(|a| a != "--dry-run")), "nothing ran");
}

/// A real run that makes a file posts it, and says what it cost.
#[test]
fn a_real_run_posts_what_it_made() {
    let fake = Fake::new();
    let node = node(&fake, &[("elevenlabs.main", KEY)], &[]);
    let args = json!(["text-to-sound-effects", "convert", "--text=rain on a tin roof", "-o", "rain.mp3"]);
    let done = node.run("elevenlabs.run", json!({"args": args, "dry_run": false})).unwrap();
    assert_eq!(done.outputs.len(), 1);
    assert!(done.outputs[0].path.ends_with("rain.mp3"));
    assert_eq!(done.outputs[0].kind, "audio");
    assert_eq!(done.result["credits"]["used"], 100);
}
