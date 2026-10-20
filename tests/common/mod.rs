//! What the tests share: the extension under the SDK's stand-in node, with a fake `elevenlabs` (a shell script
//! that logs each call beside itself, keeps the account's credits in a file there, and answers as the real CLI
//! does) and a fake GitHub release to install one from. No test reaches ElevenLabs or GitHub.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use pointman_extension::testing::StandIn;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");
pub const KEY: &str = "sk_test0123456789abcdef0123456789abcdef";
pub const REFUSED: &str = "sk_refused0000000000000000000000";

/// The fake CLI. It finds its folder from how it was started, so each test's is a link to this one script,
/// written once: one test writing a script while another starts a process is "text file busy" on Linux.
const FAKE: &str = r#"#!/bin/sh
here=$(dirname "$0")
{
  echo "----"
  echo "cwd=$(pwd)"
  echo "key=${ELEVENLABS_API_KEY-<none>}"
  echo "via=${ELEVENLABS_VIA-}"
  echo "env=$(env | cut -d= -f1 | sort | tr '\n' ' ')"
  for a in "$@"; do echo "arg=$a"; done
} >> "$here/calls.log"
if [ "$1" = "--version" ]; then echo "elevenlabs 9.9.9"; exit 0; fi
dry=0; out=""; prev=""
for a in "$@"; do
  case "$a" in --dry-run) dry=1;; --output=*) out="${a#--output=}";; esac
  if [ "$prev" = "-o" ] || [ "$prev" = "--output" ]; then out="$a"; fi
  prev="$a"
done
if [ $dry = 1 ]; then echo '{"dry_run": true, "method": "POST", "url": "https://api.elevenlabs.io/v1/x", "headers": [["xi-api-key", "[REDACTED]"]]}'; exit 0; fi
if [ -z "$ELEVENLABS_API_KEY" ] || [ "$ELEVENLABS_API_KEY" = "sk_refused0000000000000000000000" ]; then
  echo '{"error": {"code": 401, "message": "Invalid API key", "reason": "unauthorized"}}' >&2; exit 1
fi
count=$(cat "$here/credits" 2>/dev/null || echo 1000)
spend() { echo $((count + $1)) > "$here/credits"; }
case "$1 $2" in
  "user subscription")
    printf '{"tier": "creator", "character_count": %s, "character_limit": 100000, "next_character_count_reset_unix": 1893456000}\n' "$count";;
  "audio-isolation convert")
    sleep "$(cat "$here/sleep" 2>/dev/null || echo 0)"
    printf 'ID3isolated' > "$out"; spend 333
    printf '{"bytes": 11, "mimeType": "audio/mpeg", "saved_file": "%s", "status": "success"}\n' "$out";;
  "text-to-speech convert")
    printf 'ID3speech' > "$out"; spend 42
    printf '{"bytes": 9, "mimeType": "audio/mpeg", "saved_file": "%s", "status": "success"}\n' "$out";;
  "text-to-sound-effects convert")
    printf 'ID3sfx' > "${out:-sfx.mp3}"; spend 100; echo '{"status": "success"}';;
  "speech-to-text convert")
    spend 50
    echo '{"language_code": "en", "text": "hello world", "words": [{"text": "hello", "start": 0.0, "end": 0.4, "type": "word"}, {"text": " ", "start": 0.4, "end": 0.5, "type": "spacing"}, {"text": "world", "start": 0.5, "end": 0.9, "type": "word"}]}';;
  "forced-alignment create")
    spend 20
    echo '{"loss": 0.05, "words": [{"text": "hello", "start": 0.0, "end": 0.4, "loss": 0.1}, {"text": "world", "start": 0.5, "end": 0.9, "loss": 0.0}]}';;
  "voices search")
    echo '{"voices": [{"voice_id": "21m00Tcm4TlvDq8ikWAM", "name": "Rachel", "category": "premade"}, {"voice_id": "JBFqnCBsd6RMkjVDRZzb", "name": "George - warm", "category": "premade"}], "has_more": false}';;
  "voices delete")
    echo '{"status": "ok"}';;
  *)
    echo "{\"error\": {\"message\": \"unknown command: $1 $2\"}}" >&2; exit 2;;
esac
"#;

fn script() -> PathBuf {
    static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pointman-elevenlabs-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-elevenlabs");
            std::fs::write(&path, FAKE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        })
        .clone()
}

/// One test's fake CLI: its folder (the log, the credits, how long an isolate takes) and its path.
pub struct Fake {
    pub dir: TempDir,
}

impl Fake {
    pub fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(script(), dir.path().join("elevenlabs")).unwrap();
        Fake { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("elevenlabs")
    }

    /// How long its isolate takes, in seconds.
    pub fn takes(&self, seconds: f64) {
        std::fs::write(self.dir.path().join("sleep"), seconds.to_string()).unwrap();
    }

    /// Each call it had: its working folder, key, environment's names and args.
    pub fn calls(&self) -> Vec<FakeCall> {
        calls_in(&self.dir.path().join("calls.log"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeCall {
    pub cwd: String,
    pub key: String,
    pub via: String,
    pub env: Vec<String>,
    pub args: Vec<String>,
}

impl FakeCall {
    pub fn is(&self, a: &str, b: &str) -> bool {
        self.args.first().map(String::as_str) == Some(a) && self.args.get(1).map(String::as_str) == Some(b)
    }
}

pub fn calls_in(log: &Path) -> Vec<FakeCall> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut calls = Vec::new();
    for record in text.split("----\n").filter(|r| !r.trim().is_empty()) {
        let mut c = FakeCall::default();
        for line in record.lines() {
            match line.split_once('=') {
                Some(("cwd", v)) => c.cwd = v.into(),
                Some(("key", v)) => c.key = v.into(),
                Some(("via", v)) => c.via = v.into(),
                Some(("env", v)) => c.env = v.split_whitespace().map(String::from).collect(),
                Some(("arg", v)) => c.args.push(v.into()),
                _ => {}
            }
        }
        calls.push(c);
    }
    calls
}

/// A folder the node lets the extension use, with an audio file in it.
pub fn media() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("talk.wav");
    std::fs::write(&file, b"RIFF....WAVEfmt fake audio").unwrap();
    (dir, file)
}

/// The extension under the stand-in, with the fake CLI as its `cli` setting and `secrets` in the vault.
pub fn node(fake: &Fake, secrets: &[(&str, &str)], roots: &[&Path]) -> StandIn {
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_elevenlabs"))
        .settings(json!({"cli": fake.path().to_string_lossy()}))
        .secrets(secrets.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .roots(roots.iter().map(|r| r.to_path_buf()))
        .env("POINTMAN_SECRET_NOT_FOR_THE_CLI", "x")
        .start()
        .unwrap()
}

/// health.check's part `id`.
pub fn part(node: &StandIn, id: &str) -> Value {
    let parts = node.call("health.check", json!({})).unwrap();
    parts.as_array().unwrap().iter().find(|p| p["id"] == id).cloned().unwrap_or_else(|| panic!("no {id} in {parts}"))
}

/// Every file under `dir`, recursively.
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// A fake GitHub: answers each path it's given, and nothing else.
pub struct Server {
    pub url: String,
    pub routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let r = routes.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let r = r.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap_or(0);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = r.lock().unwrap().get(&path).cloned();
                    let mut stream = stream;
                    let (status, body) = match body {
                        Some(b) => ("200 OK", b),
                        None => ("404 Not Found", b"not here".to_vec()),
                    };
                    let head =
                        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
            }
        });
        Server { url, routes }
    }

    pub fn serve(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(path.into(), body.into());
    }
}

/// A release of the fake CLI (`v9.9.9`) for this platform, as ElevenLabs publishes one: the archive with
/// `elevenlabs-cli-<target>/elevenlabs` in it, and its `.sha256`. `checksum`: what that file says, else the
/// archive's own. Returns the latest-release URL.
pub fn release(server: &Server, checksum: Option<&str>) -> String {
    let target = elevenlabs::install::target().unwrap();
    let name = format!("elevenlabs-cli-{target}.tar.gz");
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join(format!("elevenlabs-cli-{target}"));
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("elevenlabs"), FAKE).unwrap();
    std::fs::set_permissions(inner.join("elevenlabs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(inner.join("README.md"), "the fake CLI\n").unwrap();
    let archive = dir.path().join(&name);
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir.path())
        .arg(format!("elevenlabs-cli-{target}"))
        .status()
        .unwrap();
    assert!(ok.success());
    let bytes = std::fs::read(&archive).unwrap();
    let sum = checksum.map(String::from).unwrap_or_else(|| format!("{:x}", Sha256::digest(&bytes)));
    server.serve(&format!("/download/{name}"), bytes);
    server.serve(&format!("/download/{name}.sha256"), format!("{sum} *{name}\n"));
    let latest = json!({
        "tag_name": "v9.9.9",
        "assets": [
            {"name": name, "browser_download_url": format!("{}/download/{name}", server.url)},
            {"name": format!("{name}.sha256"), "browser_download_url": format!("{}/download/{name}.sha256", server.url)},
        ],
    });
    server.serve("/releases/latest", latest.to_string());
    format!("{}/releases/latest", server.url)
}
