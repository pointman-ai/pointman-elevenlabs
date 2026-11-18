//! `elevenlabs.live` under the stand-in node, with the fake CLI and the machine's ffmpeg: a 12 s clip with a
//! picture and a tone, heard in 5 s chunks, each posted to the thread as `[HH:MM:SS UTC] words`, stills posted as
//! files, and a transcript.jsonl. Skipped where there's no ffmpeg.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use pointman_extension::testing::StandIn;
use serde_json::{json, Value};

fn ffmpeg() -> Option<PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg", "/run/current-system/sw/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// A clip with a test picture and a tone.
fn clip(ffmpeg: &Path, dir: &Path, seconds: u32) -> PathBuf {
    let out = dir.join("talk.mp4");
    let ok = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=size=320x240:rate=10:duration={seconds}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .args(["-shortest", "-pix_fmt", "yuv420p"])
        .arg(&out)
        .status()
        .unwrap()
        .success();
    assert!(ok, "ffmpeg made no clip");
    out
}

type Posted = Arc<Mutex<Vec<(String, Value)>>>;

fn listening_node(fake: &Fake, roots: &[&Path], posted: &Posted) -> StandIn {
    let (p1, p2) = (posted.clone(), posted.clone());
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .settings(json!({"cli": fake.path().to_string_lossy()}))
        .secrets([("elevenlabs.main".to_string(), KEY.to_string())])
        .roots(roots.iter().map(|r| r.to_path_buf()))
        .method("board.post", move |params: Value| {
            let mut p = p1.lock().unwrap();
            p.push(("board.post".into(), params));
            Ok(json!({"post": {"id": format!("post_{}", p.len())}}))
        })
        .method("files.publish", move |params: Value| {
            let mut p = p2.lock().unwrap();
            p.push(("files.publish".into(), params));
            Ok(json!({"file": {"id": "file_1"}, "version": {"id": "ver_1"}, "post": {"id": format!("post_{}", p.len())}}))
        })
        .start()
        .unwrap()
}

#[test]
fn a_stream_is_heard_in_chunks_and_posted_as_lines() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("no ffmpeg here: skipped");
        return;
    };
    let fake = Fake::new();
    let folder = tempfile::tempdir().unwrap();
    let file = clip(&ffmpeg, folder.path(), 12);
    let posted: Posted = Arc::default();
    let node = listening_node(&fake, &[folder.path()], &posted);

    let params = json!({"url": file, "thread": "thr_7", "every_seconds": 5, "stills_every_seconds": 10});
    let done = node.run_timeout("elevenlabs.live", params, Duration::from_secs(90)).unwrap();
    let r = &done.result;

    let posts = posted.lock().unwrap().clone();
    let lines: Vec<String> =
        posts.iter().filter(|(m, _)| m == "board.post").map(|(_, p)| p["text"].as_str().unwrap().to_string()).collect();
    let heard: Vec<&String> = lines.iter().filter(|l| l.ends_with("UTC] hello world")).collect();
    assert!(heard.len() >= 2, "{lines:#?}");
    for l in &heard {
        let clock = &l[1..9];
        assert!(l.starts_with('[') && clock.chars().filter(|c| *c == ':').count() == 2, "{l}");
    }
    assert!(posts.iter().all(|(m, p)| m != "board.post" || p["thread"]["id"] == "thr_7"), "{posts:?}");
    assert!(lines.last().unwrap().starts_with("Stopped listening to "), "the summary last: {lines:#?}");
    assert_eq!(r["posts"].as_u64().unwrap() as usize, heard.len());
    assert_eq!(r["stopped"], "the stream ended");

    let stills: Vec<&Value> = posts.iter().filter(|(m, _)| m == "files.publish").map(|(_, p)| p).collect();
    assert!(!stills.is_empty(), "no still: {posts:?}");
    let body = &stills[0]["body"];
    assert_eq!(body["post"]["thread_id"].as_f64(), Some(7.0), "{body}");
    assert_eq!(body["files"][0]["mime"], "image/jpeg");
    assert!(Path::new(body["files"][0]["file"].as_str().unwrap()).is_file());

    assert!(r["lag"]["count"].as_u64().unwrap() >= 4, "{r}");
    assert!(r["lag"]["median_s"].as_f64().is_some(), "{r}");

    let stt: Vec<FakeCall> = fake.calls().into_iter().filter(|c| c.is("speech-to-text", "convert")).collect();
    assert_eq!(stt.len(), r["posts"].as_u64().unwrap() as usize + r["quiet_chunks"].as_u64().unwrap() as usize);
    assert!(stt.iter().all(|c| c.args.contains(&"--file-format=pcm_s16le_16".to_string())), "{stt:?}");
    assert!(stt.iter().all(|c| c.key == KEY));

    let transcript = done.outputs.iter().find(|o| o.path.ends_with("transcript.jsonl")).expect("the transcript");
    let rows: Vec<Value> =
        std::fs::read_to_string(&transcript.path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), heard.len());
    for row in &rows {
        assert!(row["t_utc"].as_str().unwrap().ends_with('Z'), "{row}");
        assert!(row["end_s"].as_f64().unwrap() > row["start_s"].as_f64().unwrap(), "{row}");
        assert_eq!(row["text"], "hello world");
    }
    assert!(rows[1]["start_s"].as_f64().unwrap() >= 4.0, "the second chunk starts later in the stream: {rows:?}");
}

#[test]
fn what_isnt_a_stream_is_refused() {
    let fake = Fake::new();
    let posted: Posted = Arc::default();
    let node = listening_node(&fake, &[], &posted);
    for url in ["file:///etc/passwd", "concat:/a|/b", "-i"] {
        let e = node.run("elevenlabs.live", json!({"url": url, "thread": "thr_7"})).unwrap_err();
        assert!(!e.to_string().is_empty(), "{url}");
    }
    let e = node.run("elevenlabs.live", json!({"url": "https://youtu.be/x"})).unwrap_err();
    assert!(e.to_string().contains("thread is missing"), "{e}");
    assert!(posted.lock().unwrap().is_empty());
    assert!(fake.calls().iter().all(|c| !c.is("speech-to-text", "convert")));
}
