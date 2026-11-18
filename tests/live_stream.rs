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
    clip_as(ffmpeg, dir, seconds, "talk.mp4")
}

fn clip_as(ffmpeg: &Path, dir: &Path, seconds: u32, name: &str) -> PathBuf {
    let out = dir.join(name);
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
    listening_node_with(roots, posted, json!({"cli": fake.path().to_string_lossy()}))
}

fn listening_node_with(roots: &[&Path], posted: &Posted, settings: Value) -> StandIn {
    let (p1, p2) = (posted.clone(), posted.clone());
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .settings(settings)
        .env("POINTMAN_ELEVENLABS_RETRY_MS", "500")
        .env("POINTMAN_ELEVENLABS_QUIET_SECS", "8")
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

/// A scheduled live that hasn't started: yt-dlp says so twice, then gives the stream. The job waits (one line
/// saying so) rather than failing, and posts when it starts.
#[test]
fn a_stream_that_hasnt_started_is_waited_for() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("no ffmpeg here: skipped");
        return;
    };
    let fake = Fake::new();
    let folder = tempfile::tempdir().unwrap();
    let ts = clip_as(&ffmpeg, folder.path(), 6, "talk.ts");
    let server = Server::start();
    server.serve("/live/talk.ts", std::fs::read(&ts).unwrap());
    let ytdlp = folder.path().join("yt-dlp");
    let tries = folder.path().join("tries");
    std::fs::write(
        &ytdlp,
        format!(
            "#!/bin/sh\nn=$(( $(cat '{t}' 2>/dev/null || echo 0) + 1 )); echo $n > '{t}'\n\
             if [ $n -le 2 ]; then echo 'ERROR: [youtube] x: This live event will begin in 5 minutes.' >&2; exit 1; fi\n\
             printf 'False\\nThe webinar\\n{url}/live/talk.ts\\n'\n",
            t = tries.display(),
            url = server.url
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ytdlp, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let posted: Posted = Arc::default();
    let settings = json!({"cli": fake.path().to_string_lossy(), "yt_dlp": ytdlp.to_string_lossy()});
    let node = listening_node_with(&[folder.path()], &posted, settings);

    let params = json!({"url": "https://www.youtube.com/watch?v=x", "thread": "thr_7", "every_seconds": 5, "stills_every_seconds": 0, "minutes": 1});
    let done = node.run_timeout("elevenlabs.live", params, Duration::from_secs(90)).unwrap();
    let lines: Vec<String> = posted
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "board.post")
        .map(|(_, p)| p["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(std::fs::read_to_string(&tries).unwrap().trim(), "3", "asked yt-dlp three times");
    assert!(lines[0].ends_with("UTC] Waiting for the stream to start."), "{lines:#?}");
    assert_eq!(lines.iter().filter(|l| l.contains("Waiting for the stream")).count(), 1, "said once: {lines:#?}");
    assert!(lines[1].ends_with("UTC] The stream has started."), "{lines:#?}");
    assert!(lines.iter().any(|l| l.ends_with("UTC] hello world")), "{lines:#?}");
    assert!(lines.last().unwrap().starts_with("Stopped listening to The webinar: the stream ended."), "{lines:#?}");
    assert!(done.result["posts"].as_u64().unwrap() >= 1);
}

/// A stretch with no words (music, a pause) is said in the thread every so often, so it isn't taken for a fault;
/// the summary counts the chunks. (The tests say it every 8 s; a stream every 2 min.)
#[test]
fn a_quiet_stretch_is_said() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("no ffmpeg here: skipped");
        return;
    };
    let fake = Fake::new();
    fake.hears_nothing();
    let folder = tempfile::tempdir().unwrap();
    let file = clip(&ffmpeg, folder.path(), 12);
    let posted: Posted = Arc::default();
    let node = listening_node(&fake, &[folder.path()], &posted);
    let params = json!({"url": file, "thread": "thr_7", "every_seconds": 5, "stills_every_seconds": 0});
    let done = node.run_timeout("elevenlabs.live", params, Duration::from_secs(90)).unwrap();
    let lines: Vec<String> =
        posted.lock().unwrap().iter().map(|(_, p)| p["text"].as_str().unwrap_or_default().to_string()).collect();
    let quiet: Vec<&String> = lines.iter().filter(|l| l.ends_with("UTC] 1 min, no words.")).collect();
    assert_eq!(quiet.len(), 1, "one line for the first 8 s and more of quiet: {lines:#?}");
    assert_eq!(done.result["posts"], 0);
    let chunks = done.result["quiet_chunks"].as_u64().unwrap();
    assert!(chunks >= 2, "{}", done.result);
    assert!(lines.last().unwrap().contains(&format!(", {chunks} chunks with no words;")), "{lines:#?}");
    assert!(node.log().contains("live: chunk 1: 0 words"), "each chunk is logged: {}", node.log());
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

/// A real stream, by hand: `LIVE_URL=https://www.youtube.com/@SkyNews/live cargo test --test live_stream -- --ignored
/// --nocapture`. yt-dlp and ffmpeg read it for real; speech to text is the fake CLI's, so it spends nothing and
/// posts only to the stand-in.
#[test]
#[ignore]
fn a_real_stream_through_the_fake_cli() {
    let url = std::env::var("LIVE_URL").expect("LIVE_URL");
    let minutes: f64 = std::env::var("LIVE_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(1.0);
    let fake = Fake::new();
    let posted: Posted = Arc::default();
    let node = listening_node(&fake, &[], &posted);
    let params = json!({"url": url, "thread": "thr_7", "minutes": minutes, "stills_every_seconds": 20});
    let done = node.run_timeout("elevenlabs.live", params, Duration::from_secs_f64(minutes * 60.0 + 120.0)).unwrap();
    for (m, p) in posted.lock().unwrap().iter() {
        match m.as_str() {
            "board.post" => println!("post: {}", p["text"]),
            _ => println!("still: {}", p["body"]["files"][0]["file"]),
        }
    }
    println!("{:#}", done.result);
}
