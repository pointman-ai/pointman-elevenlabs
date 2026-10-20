//! ElevenLabs' CLI: where it is, its version, and running it. Each call runs in a scratch folder of the
//! extension's own (the CLI reads a `.env` in its working folder and the ones above it), with only the node's
//! few environment variables and, when there is one, the key as ELEVENLABS_API_KEY: never on its command line,
//! never on disk. Whatever comes back has the key taken out.

use std::ffi::OsStr;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use pointman_extension::Cancelled;
use serde_json::Value;

/// What a call passes on from the extension's own environment (the node's): never anything else.
const ENV_KEEP: [&str; 6] = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR"];
/// How much of a call's stdout is kept: a voices list is well under it, and `-o -` audio isn't wanted back.
const STDOUT_MAX: usize = 32 * 1024 * 1024;
const STDERR_MAX: usize = 256 * 1024;

/// Where the `elevenlabs` in use came from: health says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum From {
    /// The `cli` setting.
    Setting,
    /// Homebrew's (`brew install elevenlabs/tap/elevenlabs`).
    Homebrew,
    /// Another on the PATH, or in /opt/homebrew/bin or /usr/local/bin.
    Machine,
    /// ElevenLabs' release build, installed by this extension in its state folder.
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub from: From,
}

impl Found {
    /// Where it came from, as health and the commands say it.
    pub fn origin(&self) -> String {
        let at = self.path.display();
        match self.from {
            From::Setting => format!("the `cli` setting's, {at}"),
            From::Homebrew => format!("from Homebrew, {at}"),
            From::Machine => format!("this machine's, {at}"),
            From::Installed => format!("ElevenLabs' release build, installed by the extension at {at}"),
        }
    }
}

/// Where `elevenlabs` is: the setting, else on the PATH, else in `places` (Homebrew's two folders), else the one
/// this extension installed in its state folder. Never `own`, this extension's binary, which has the same name.
pub fn find(
    setting: &str,
    path_var: Option<&OsStr>,
    places: &[PathBuf],
    state: Option<&Path>,
    own: Option<&Path>,
) -> Option<Found> {
    if !setting.trim().is_empty() {
        return Some(Found { path: PathBuf::from(setting.trim()), from: From::Setting });
    }
    let own = own.and_then(|p| p.canonicalize().ok());
    let on_path: Vec<PathBuf> =
        path_var.map(|p| std::env::split_paths(p).map(|d| d.join("elevenlabs")).collect()).unwrap_or_default();
    let found = on_path
        .into_iter()
        .chain(places.iter().cloned())
        .filter(|p| p.is_absolute() && p.is_file())
        .find(|p| own.is_none() || p.canonicalize().ok() != own);
    if let Some(path) = found {
        let brewed = path.canonicalize().map(|real| real.to_string_lossy().contains("/Cellar/")).unwrap_or(false);
        return Some(Found { path, from: if brewed { From::Homebrew } else { From::Machine } });
    }
    state.and_then(crate::install::installed).map(|path| Found { path, from: From::Installed })
}

/// The CLI's version, from `elevenlabs --version` ("elevenlabs 1.6.1").
pub fn version(cli: &Path, scratch: &Path) -> anyhow::Result<String> {
    let ran = run(
        &Call::new(cli, scratch, vec!["--version".into()]).timeout(Duration::from_secs(15)),
        &|| false,
        &mut |_| {},
    )?;
    let out = String::from_utf8_lossy(&ran.stdout);
    match (ran.code, out.split_whitespace().last()) {
        (Some(0), Some(v)) if v.starts_with(|c: char| c.is_ascii_digit()) => Ok(v.to_string()),
        _ => {
            bail!("`{} --version` said {:?}", cli.display(), clip(&format!("{}{}", out.trim(), ran.stderr.trim()), 200))
        }
    }
}

/// One call of the CLI.
pub struct Call<'a> {
    pub cli: &'a Path,
    pub cwd: &'a Path,
    pub args: Vec<String>,
    pub key: Option<&'a str>,
    /// ELEVENLABS_VIA: the product token ElevenLabs' CLI adds to its User-Agent.
    pub via: Option<&'a str>,
    pub timeout: Option<Duration>,
}

impl<'a> Call<'a> {
    pub fn new(cli: &'a Path, cwd: &'a Path, args: Vec<String>) -> Self {
        Call { cli, cwd, args, key: None, via: None, timeout: None }
    }

    pub fn key(mut self, key: Option<&'a str>) -> Self {
        self.key = key;
        self
    }

    pub fn via(mut self, via: &'a str) -> Self {
        self.via = Some(via);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// What a call gave back, with the key taken out of its stderr. `code` is None when it was killed.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Its stdout as JSON, if it is.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.stdout).ok()
    }
}

/// An error the CLI reported: its HTTP status when it had one, and its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub status: Option<u64>,
    pub message: String,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "ElevenLabs said {s}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CliError {}

impl CliError {
    /// The key was refused.
    pub fn unauthorized(&self) -> bool {
        self.status == Some(401)
    }
}

/// Why a call failed: the CLI's JSON error (`{"error": {"code", "message", …}}`, on stderr or stdout), else its
/// last words on stderr.
pub fn error_of(ran: &Ran) -> CliError {
    let parsed = serde_json::from_str::<Value>(ran.stderr.trim()).ok().or_else(|| ran.json());
    if let Some(e) = parsed.as_ref().map(|v| &v["error"]).filter(|e| e.is_object()) {
        let mut message = e["message"].as_str().unwrap_or("it failed").to_string();
        if let Some(status) = e["details"]["detail"]["status"].as_str() {
            message = format!("{message} ({status})");
        }
        return CliError { status: e["code"].as_u64(), message: clip(&message, 500) };
    }
    let last = ran.stderr.lines().rev().map(str::trim).find(|l| !l.is_empty());
    let message = match (last, ran.code) {
        (Some(line), _) => line.to_string(),
        (None, Some(code)) => format!("the CLI exited with {code}"),
        (None, None) => "the CLI was stopped".to_string(),
    };
    CliError { status: None, message: clip(&message, 500) }
}

/// Run a call, checking `cancelled` (then killing it, as `Cancelled`) and calling `tick` with the time so far
/// about every two seconds.
pub fn run(call: &Call, cancelled: &dyn Fn() -> bool, tick: &mut dyn FnMut(Duration)) -> anyhow::Result<Ran> {
    std::fs::create_dir_all(call.cwd).with_context(|| format!("making {}", call.cwd.display()))?;
    let mut command = Command::new(call.cli);
    command.args(&call.args).current_dir(call.cwd).env_clear();
    for key in ENV_KEEP {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("NO_COLOR", "1");
    if let Some(via) = call.via {
        command.env("ELEVENLABS_VIA", via);
    }
    if let Some(key) = call.key {
        command.env("ELEVENLABS_API_KEY", key);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| format!("starting {}", call.cli.display()))?;
    let out = child.stdout.take().map(|s| std::thread::spawn(move || drain(s, STDOUT_MAX)));
    let err = child.stderr.take().map(|s| std::thread::spawn(move || drain(s, STDERR_MAX)));
    let started = Instant::now();
    let mut ticked = started;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Cancelled.into());
        }
        if call.timeout.is_some_and(|t| started.elapsed() > t) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if ticked.elapsed() >= Duration::from_secs(2) {
            ticked = Instant::now();
            tick(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = out.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = redact(&String::from_utf8_lossy(&stderr), call.key);
    if status.is_none() {
        bail!("{} didn't finish in {} s", call.cli.display(), call.timeout.unwrap_or_default().as_secs());
    }
    Ok(Ran { code: status.and_then(|s| s.code()), stdout: redact_bytes(stdout, call.key), stderr })
}

fn drain(mut from: impl Read, max: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            break;
        }
        let room = max.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    kept
}

/// `text` with the key, wherever it is, as `[REDACTED]`.
pub fn redact(text: &str, key: Option<&str>) -> String {
    match key.filter(|k| k.len() >= 8) {
        Some(key) => text.replace(key, "[REDACTED]"),
        None => text.to_string(),
    }
}

fn redact_bytes(bytes: Vec<u8>, key: Option<&str>) -> Vec<u8> {
    let Some(key) = key.map(str::as_bytes).filter(|k| k.len() >= 8) else { return bytes };
    if !bytes.windows(key.len()).any(|w| w == key) {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(key) {
            out.extend_from_slice(b"[REDACTED]");
            i += key.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// At most `max` characters of `s`, with … when cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_taken_out_of_what_comes_back() {
        let key = "sk_0123456789abcdef";
        assert_eq!(redact(&format!("a {key} b {key}"), Some(key)), "a [REDACTED] b [REDACTED]");
        assert_eq!(redact_bytes(format!("x{key}y").into_bytes(), Some(key)), b"x[REDACTED]y");
        assert_eq!(redact("nothing here", None), "nothing here");
    }

    #[test]
    fn errors_are_the_clis_own_words() {
        let ran = Ran {
            code: Some(1),
            stdout: vec![],
            stderr: r#"{"error": {"code": 401, "message": "Invalid API key", "details": {"detail": {"status": "invalid_api_key"}}}}"#.into(),
        };
        let e = error_of(&ran);
        assert!(e.unauthorized());
        assert_eq!(e.to_string(), "ElevenLabs said 401: Invalid API key (invalid_api_key)");
        let plain =
            Ran {
                code: Some(2), stdout: vec![], stderr: "error: unexpected argument '--nope'\n\nFor more…\n".into()
            };
        assert_eq!(error_of(&plain).message, "For more…");
    }

    #[test]
    fn the_setting_then_the_path_then_homebrew_then_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("elevenlabs"), "").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(find(" /x/elevenlabs ", Some(&path), &[], None, None).unwrap().from, From::Setting);
        let found = find("", Some(&path), &[], None, None).unwrap();
        assert_eq!((found.path, found.from), (bin.join("elevenlabs"), From::Machine));
        // never this extension's own binary, which has the same name
        assert_eq!(find("", Some(&path), &[], None, Some(&bin.join("elevenlabs"))), None);
        assert_eq!(find("", None, &[bin.join("elevenlabs")], None, None).unwrap().path, bin.join("elevenlabs"));
        assert_eq!(find("", None, &[], Some(dir.path()), None), None);
    }
}
