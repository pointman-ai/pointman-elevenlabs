//! The extension's process: the node starts it (`bin/elevenlabs`, from the extension's folder) and talks to it
//! over stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! Each command runs ElevenLabs' CLI on a thread of its own (no lane), so an 18-minute isolate holds up nobody,
//! and can be cancelled: the CLI is killed. Its key comes from the vault for each call (`elevenlabs.<label>`),
//! into that call's environment only. A paid call reads the account's credits before and after, and says what it
//! used. What it makes goes back as the command's outputs, posted where it was asked.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use elevenlabs::cli::{self, Call, Found, Ran};
use elevenlabs::{args, install, live, words};
use pointman_extension::convert::to_struct;
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::{
    BoardPostRequest, FilesPublishRequest, HealthCheckRequest, HealthCheckResponse, HealthFixRequest, HealthFixResponse,
};
use pointman_extension::v1::health::part::State;
use pointman_extension::v1::health::{Action, Part};
use pointman_extension::v1::ThreadRef;
use pointman_extension::{Extension, Handle, Job};
use serde_json::{json, Value};

const VIA: &str = concat!("pointman-elevenlabs/", env!("CARGO_PKG_VERSION"));
/// A quick call (credits, voices) that takes longer than this has hung.
const QUICK: Duration = Duration::from_secs(120);
/// How long health keeps what it read of the account.
const ACCOUNT_KEPT: Duration = Duration::from_secs(300);
const STT_MODEL: &str = "scribe_v1";
const AUDIO: [&str; 9] = ["mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "pcm", "ulaw"];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn state(h: &Handle) -> PathBuf {
    h.state_dir()
}

fn machine(h: &Handle) -> String {
    h.machine()
}

/// A folder of the extension's own to run the CLI in, outside any job (health, credits).
fn scratch(h: &Handle) -> PathBuf {
    let s = state(h);
    let base = if s.is_absolute() { s } else { std::env::temp_dir().join("pointman-elevenlabs") };
    base.join("scratch")
}

/// Homebrew's two folders, where a node's PATH may not reach. POINTMAN_ELEVENLABS_PLACES (a PATH-style list)
/// replaces them, for tests that need a machine without one.
fn places() -> Vec<PathBuf> {
    match std::env::var_os("POINTMAN_ELEVENLABS_PLACES") {
        Some(list) => std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()).collect(),
        None => vec!["/opt/homebrew/bin/elevenlabs".into(), "/usr/local/bin/elevenlabs".into()],
    }
}

/// ElevenLabs' newest release on GitHub; POINTMAN_ELEVENLABS_RELEASES for a test's fake one.
fn releases() -> String {
    std::env::var("POINTMAN_ELEVENLABS_RELEASES").unwrap_or_else(|_| install::RELEASES.to_string())
}

/// Where the CLI sends its requests, key and all: the `base_url` setting (an EU-residency account's API), else
/// ElevenLabs' own. Pinned in each call's environment, whatever a `.env` says.
fn base_url(h: &Handle) -> String {
    let set = h.settings()["base_url"].as_str().map(|u| u.trim().trim_end_matches('/').to_string());
    set.filter(|u| !u.is_empty()).unwrap_or_else(|| cli::BASE_URL.to_string())
}

fn find(h: &Handle) -> Option<Found> {
    let s = state(h);
    let own = std::env::current_exe().ok();
    cli::find(
        h.settings()["cli"].as_str().unwrap_or_default(),
        std::env::var_os("PATH").as_deref(),
        &places(),
        s.is_absolute().then_some(s.as_path()),
        own.as_deref(),
    )
}

const ADD: &str = "add one with `/elevenlabs add` in any thread: the app's form seals it in the vault, so it never \
                   goes in a post";

/// The key a call uses: the `account` setting's vault item, else the first `elevenlabs.` one by label.
struct Key {
    name: String,
    value: String,
}

/// The key in what the node gives: by its own pattern (`sk_…`, or an older 32-hex key) wherever it sits, since the
/// node hands over the vault's sealed record as it is (`{"kind":"env","vars":{"elevenlabs.main":"sk_…"}}`), and a
/// pasted `ELEVENLABS_API_KEY=` line or quotes are around it too. Anything else as pasted, without those.
fn key_of(pasted: &str) -> String {
    if let Some(key) = sk_key(pasted).or_else(|| hex_key(pasted)) {
        return key.to_string();
    }
    let v = pasted.trim();
    let v = v.rsplit_once('=').map_or(v, |(_, k)| k);
    v.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string()
}

/// `sk_` and 20 to 200 letters and digits, not inside a longer word (the credential form's own pattern).
fn sk_key(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(at) = s[from..].find("sk_").map(|i| i + from) {
        let len = b[at + 3..].iter().take_while(|c| c.is_ascii_alphanumeric()).count();
        if (at == 0 || !b[at - 1].is_ascii_alphanumeric()) && (20..=200).contains(&len) {
            return Some(&s[at..at + 3 + len]);
        }
        from = at + 3;
    }
    None
}

/// 32 lowercase hex digits on their own: an older ElevenLabs key.
fn hex_key(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut at = 0;
    while at < b.len() {
        let len = b[at..].iter().take_while(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')).count();
        let alone = (at == 0 || !b[at - 1].is_ascii_alphanumeric())
            && b.get(at + len).is_none_or(|c| !c.is_ascii_alphanumeric());
        if len == 32 && alone {
            return Some(&s[at..at + len]);
        }
        at += len.max(1);
    }
    None
}

#[derive(Debug)]
enum NoKey {
    None(String),
    Other(String),
}

impl std::fmt::Display for NoKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoKey::None(why) | NoKey::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for NoKey {}

fn key(h: &Handle) -> Result<Key, NoKey> {
    let mut names = h
        .secrets("elevenlabs.")
        .map_err(|e| NoKey::Other(format!("couldn't ask the node for the vault's ElevenLabs keys: {e:#}")))?;
    names.sort();
    let wanted = h.settings()["account"].as_str().map(str::trim).unwrap_or_default().to_string();
    let name = if wanted.is_empty() {
        names.first().cloned().ok_or_else(|| NoKey::None(format!("No ElevenLabs key on {} yet: {ADD}.", machine(h))))?
    } else {
        let name = format!("elevenlabs.{}", wanted.trim_start_matches("elevenlabs."));
        if !names.contains(&name) {
            let have = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            return Err(NoKey::None(format!(
                "The `account` setting names {name}, which {} hasn't got (it has {have}): {ADD}, or change the setting.",
                machine(h)
            )));
        }
        name
    };
    let value = h.secret(&name).map_err(|e| NoKey::Other(format!("couldn't open {name}: {e:#}")))?;
    let value = key_of(&value);
    if value.is_empty() {
        return Err(NoKey::None(format!("{name} is empty: {ADD}.")));
    }
    Ok(Key { name, value })
}

/// The account's plan and credits, from `elevenlabs user subscription get`.
#[derive(Debug, Clone)]
struct Account {
    tier: String,
    used: i64,
    limit: i64,
    resets: Option<i64>,
}

impl Account {
    fn left(&self) -> i64 {
        (self.limit - self.used).max(0)
    }
}

fn account(cli: &Path, key: &Key, cwd: &Path, base: &str) -> anyhow::Result<Account> {
    let args = vec!["user".into(), "subscription".into(), "get".into(), "--format=json".into()];
    let call = Call::new(cli, cwd, args).key(Some(&key.value)).via(VIA).base_url(base).timeout(QUICK);
    let ran = cli::run(&call, &|| false, &mut |_| {})?;
    if !ran.ok() {
        return Err(failure(&ran, key));
    }
    let v = ran.json().context("ElevenLabs' answer about the account wasn't JSON")?;
    Ok(Account {
        tier: v["tier"].as_str().unwrap_or("unknown").to_string(),
        used: v["character_count"].as_i64().context("ElevenLabs gave no credits used")?,
        limit: v["character_limit"].as_i64().context("ElevenLabs gave no credit limit")?,
        resets: v["next_character_count_reset_unix"].as_i64(),
    })
}

/// A failed call's error, saying which key was refused when one was.
fn failure(ran: &Ran, key: &Key) -> anyhow::Error {
    let e = cli::error_of(ran);
    if e.unauthorized() {
        return anyhow!("{e}. ElevenLabs refused the key in {}: {ADD}.", key.name);
    }
    anyhow!(e)
}

/// Paid calls in flight, and how many have started: a call's credits are its own only when no other paid call
/// ran beside it.
static PAID_NOW: AtomicUsize = AtomicUsize::new(0);
static PAID_STARTED: AtomicU64 = AtomicU64::new(0);

/// How long a paid call's unchanged balance is waited on, read again after each: 21 s in all.
const CHARGE_WAITS: [Duration; 3] = [Duration::from_secs(3), Duration::from_secs(6), Duration::from_secs(12)];

/// What a paid call cost, from the account's credits before and after it.
#[derive(Debug, Clone, Default)]
struct Credits {
    used: Option<i64>,
    left: Option<i64>,
    /// Another of this machine's paid calls ran beside it, so the difference is theirs too.
    shared: bool,
    /// ElevenLabs hadn't counted it yet when last read.
    pending: bool,
}

impl Credits {
    fn json(&self) -> Value {
        json!({"used": self.used, "left": self.left, "shared": self.shared, "pending": self.pending})
    }

    fn line(&self) -> String {
        match (self.used, self.left) {
            (_, Some(left)) if self.pending => format!(
                "ElevenLabs hadn't counted its credits 20 s after it finished ({} left then): elevenlabs.usage shows \
                 them once it has.",
                words::thousands(left)
            ),
            (Some(used), Some(left)) if self.shared => format!(
                "The account used {} credits while it ran, with other ElevenLabs jobs going at the same time; {} left.",
                words::thousands(used),
                words::thousands(left)
            ),
            (Some(used), Some(left)) => {
                format!("Used {} credits; {} left.", words::thousands(used), words::thousands(left))
            }
            _ => "The credits it used couldn't be read.".to_string(),
        }
    }
}

/// Everything a job needs to call the CLI.
struct Ready {
    cli: PathBuf,
    key: Key,
    work: PathBuf,
    base: String,
}

/// What health last read of the account: when, with which key, and what.
type KeptAccount = Option<(Instant, String, Result<Account, String>)>;

struct Ext {
    h: Handle,
    /// Held while the CLI is installed; the last install's error.
    installing: Mutex<Option<String>>,
    account: Mutex<KeptAccount>,
}

impl Ext {
    /// The CLI, installed first if the machine hasn't got one.
    fn cli(&self) -> anyhow::Result<PathBuf> {
        if let Some(found) = find(&self.h) {
            return Ok(found.path);
        }
        let mut last = lock(&self.installing);
        if let Some(found) = find(&self.h) {
            return Ok(found.path);
        }
        let s = state(&self.h);
        if !s.is_absolute() {
            bail!("ElevenLabs' CLI isn't on this machine, and the node gave no state folder to install it in");
        }
        self.h.log("ElevenLabs' CLI isn't on this machine: installing ElevenLabs' release build");
        match install::install(&s, &releases()) {
            Ok((path, version)) => {
                self.h.log(format!("installed ElevenLabs' CLI {version} at {}", path.display()));
                *last = None;
                Ok(path)
            }
            Err(e) => {
                let why = format!("couldn't install ElevenLabs' CLI: {e:#}");
                self.h.log(&why);
                *last = Some(why.clone());
                bail!(why)
            }
        }
    }

    fn ready(&self, job: &Job) -> anyhow::Result<Ready> {
        let key = key(&self.h)?;
        let cli = self.cli()?;
        std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
        Ok(Ready { cli, key, work: job.work.clone(), base: base_url(&self.h) })
    }

    /// A call for a job: cancellable, its stage and the time so far as progress.
    fn call(&self, job: &Job, r: &Ready, args: Vec<String>, stage: &str) -> anyhow::Result<Ran> {
        self.call_with(job, &r.cli, Some(&r.key), &r.work, args, stage)
    }

    fn call_with(
        &self,
        job: &Job,
        cli: &Path,
        key: Option<&Key>,
        cwd: &Path,
        args: Vec<String>,
        stage: &str,
    ) -> anyhow::Result<Ran> {
        job.progress(None, stage);
        let base = base_url(&self.h);
        let call = Call::new(cli, cwd, args).key(key.map(|k| k.value.as_str())).via(VIA).base_url(&base);
        let ran = cli::run(&call, &|| job.cancelled(), &mut |so_far| {
            job.progress_detail(
                None,
                format!("{stage}: {} so far", words::took(so_far)).as_str(),
                json!({"elapsed_seconds": so_far.as_secs()}),
            )
        })?;
        if !ran.ok() {
            return Err(match key {
                Some(key) => failure(&ran, key),
                None => anyhow!(cli::error_of(&ran)),
            });
        }
        Ok(ran)
    }

    /// `f`, a paid call, with the credits it used read before and after. ElevenLabs can count a call's credits a
    /// few seconds after it answers (a 20 s isolate's showed none at once), so for a call that always costs
    /// (`charged`), an unchanged balance is read again a few times before saying it's not counted yet.
    fn paid<T>(
        &self,
        job: &Job,
        r: &Ready,
        charged: bool,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<(T, Credits)> {
        let before = match account(&r.cli, &r.key, &r.work, &r.base) {
            Ok(a) => Some(a),
            Err(e) if e.to_string().contains("refused the key") => return Err(e),
            Err(e) => {
                self.h.log(format!("couldn't read the credits before a call: {e:#}"));
                None
            }
        };
        let others = PAID_NOW.fetch_add(1, Ordering::SeqCst);
        let started = PAID_STARTED.fetch_add(1, Ordering::SeqCst) + 1;
        let done = f();
        let shared = others > 0 || PAID_STARTED.load(Ordering::SeqCst) != started;
        PAID_NOW.fetch_sub(1, Ordering::SeqCst);
        let made = done?;
        let read = || {
            account(&r.cli, &r.key, &r.work, &r.base)
                .map_err(|e| self.h.log(format!("couldn't read the credits after a call: {e:#}")))
                .ok()
        };
        let mut after = read();
        let unchanged =
            |after: &Option<Account>| before.as_ref().zip(after.as_ref()).is_some_and(|(b, a)| a.used == b.used);
        if charged {
            for wait in CHARGE_WAITS {
                if !unchanged(&after) || job.cancelled() {
                    break;
                }
                job.progress(None, "waiting for ElevenLabs to count the credits it used");
                std::thread::sleep(wait);
                after = read();
            }
        }
        let pending = charged && unchanged(&after);
        let credits = Credits {
            used: before.as_ref().zip(after.as_ref()).map(|(b, a)| a.used - b.used).filter(|_| !pending),
            left: after.as_ref().map(Account::left),
            shared,
            pending,
        };
        Ok((made, credits))
    }

    /// A command's `file`: a path in the folders this node lets the extension use (a board file the node
    /// fetched is in its state folder).
    fn file(&self, job: &Job) -> anyhow::Result<PathBuf> {
        let given = job.params["file"].as_str().map(str::trim).filter(|f| !f.is_empty()).context("file is missing")?;
        if given.contains("://") {
            bail!("{given} wasn't fetched by this node: give a board file (pointman://asset/<id>) or a path on it");
        }
        let path = self.h.allowed(given)?;
        if !path.is_file() {
            bail!("{} isn't a file", path.display());
        }
        Ok(path)
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn text_param(job: &Job, key: &str) -> Option<String> {
    job.params[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn isolate(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let file = x.file(job)?;
    let r = x.ready(job)?;
    let name = name_of(&file);
    let out = r.work.join(format!("{}-isolated.mp3", words::stem(&name)));
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let stage = format!("isolating the voice in {name} ({}) on ElevenLabs", words::size(size));
    let args = vec![
        "audio-isolation".into(),
        "convert".into(),
        format!("--audio={}", file.display()),
        format!("--output={}", out.display()),
        "--format=json".into(),
    ];
    let started = Instant::now();
    let (_, credits) = x.paid(job, &r, true, || x.call(job, &r, args, &stage))?;
    if std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("ElevenLabs gave no audio for {name}");
    }
    job.progress(1.0, "posting the isolated audio");
    job.output(&out, "audio", Role::Main, json!({"file": name, "credits": credits.used}));
    Ok(json!({
        "text": format!("Isolated the voice in {name} in {}: {}. {}", words::took(started.elapsed()), name_of(&out), credits.line()),
        "file": out,
        "credits": credits.json(),
    }))
}

/// A voice by its id (20 or more letters and digits) or its name, as `voices search` finds it: exactly, else the
/// one name that starts with it.
fn voice_id(x: &Ext, job: &Job, r: &Ready, voice: &str) -> anyhow::Result<(String, String)> {
    if voice.len() >= 20 && voice.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok((voice.to_string(), voice.to_string()));
    }
    let args = vec![
        "voices".into(),
        "search".into(),
        format!("--search={voice}"),
        "--page-size=100".into(),
        "--format=json".into(),
    ];
    let ran = x.call(job, r, args, &format!("finding the voice {voice}"))?;
    let found = ran.json().context("ElevenLabs' voices weren't JSON")?;
    let voices: Vec<(String, String)> = found["voices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| Some((v["name"].as_str()?.to_string(), v["voice_id"].as_str()?.to_string())))
        .collect();
    let want = voice.to_lowercase();
    if let Some((name, id)) = voices.iter().find(|(n, _)| n.to_lowercase() == want) {
        return Ok((id.clone(), name.clone()));
    }
    let starting: Vec<&(String, String)> = voices.iter().filter(|(n, _)| n.to_lowercase().starts_with(&want)).collect();
    match starting.as_slice() {
        [(name, id)] => Ok((id.clone(), name.clone())),
        [] => bail!("No voice called {voice}: elevenlabs.voices lists them, by name and id"),
        many => bail!(
            "{voice} could be any of {}: give the name in full, or its id",
            many.iter().map(|(n, id)| format!("{n} ({id})")).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn tts(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let text = text_param(job, "text").context("text is missing")?;
    let voice = text_param(job, "voice").context("voice is missing: a name or an id (elevenlabs.voices)")?;
    let r = x.ready(job)?;
    let (id, name) = voice_id(x, job, &r, &voice)?;
    let first: String = text.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    let out = r.work.join(format!("{}-{}.mp3", words::slug(&name, 24), words::slug(&first, 40)));
    let mut args = vec![
        "text-to-speech".into(),
        "convert".into(),
        format!("--voice-id={id}"),
        format!("--text={text}"),
        format!("--output={}", out.display()),
        "--format=json".into(),
    ];
    if let Some(model) = text_param(job, "model") {
        args.push(format!("--model-id={model}"));
    }
    let stage = format!("speaking {} characters as {name}", text.chars().count());
    let (_, credits) = x.paid(job, &r, true, || x.call(job, &r, args, &stage))?;
    if std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("ElevenLabs gave no audio");
    }
    job.output(&out, "audio", Role::Main, json!({"file": "tts", "voice": name, "credits": credits.used}));
    Ok(json!({
        "text": format!("Spoke {} characters as {name}: {}. {}", text.chars().count(), name_of(&out), credits.line()),
        "voice": {"name": name, "id": id},
        "file": out,
        "credits": credits.json(),
    }))
}

fn stt(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let file = x.file(job)?;
    let r = x.ready(job)?;
    let name = name_of(&file);
    let model = text_param(job, "model").unwrap_or_else(|| STT_MODEL.to_string());
    let mut args = vec![
        "speech-to-text".into(),
        "convert".into(),
        format!("--file={}", file.display()),
        format!("--model-id={model}"),
        "--format=json".into(),
    ];
    if let Some(language) = text_param(job, "language") {
        args.push(format!("--language-code={language}"));
    }
    let stage = format!("transcribing {name} on ElevenLabs");
    let (ran, credits) = x.paid(job, &r, true, || x.call(job, &r, args, &stage))?;
    let got = ran.json().context("ElevenLabs' transcript wasn't JSON")?;
    let text = got["text"].as_str().unwrap_or_default().trim().to_string();
    let count = got["words"].as_array().map_or(0, |w| w.iter().filter(|w| w["type"] != "spacing").count());
    let stem = words::stem(&name);
    let txt = r.work.join(format!("{stem}-transcript.txt"));
    let full = r.work.join(format!("{stem}-transcript.json"));
    std::fs::write(&txt, format!("{text}\n"))?;
    std::fs::write(&full, serde_json::to_vec_pretty(&got)?)?;
    let meta = json!({"file": name, "credits": credits.used});
    job.output(&txt, "document", Role::Main, meta.clone());
    job.output(&full, "document", Role::Main, meta);
    let language = got["language_code"].as_str().unwrap_or("unknown").to_string();
    Ok(json!({
        "text": format!(
            "Transcribed {name}: {count} words, language {language}. {}\n\n{}",
            credits.line(),
            cli::clip(&text, 1500)
        ),
        "transcript": cli::clip(&text, 20_000),
        "language": language,
        "words": count,
        "files": [txt, full],
        "credits": credits.json(),
    }))
}

fn align(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let file = x.file(job)?;
    let text = text_param(job, "text").context("text is missing: what's said in the file")?;
    let r = x.ready(job)?;
    let name = name_of(&file);
    let args = vec![
        "forced-alignment".into(),
        "create".into(),
        format!("--file={}", file.display()),
        format!("--text={text}"),
        "--format=json".into(),
    ];
    let stage = format!("aligning {} words to {name}", text.split_whitespace().count());
    let (ran, credits) = x.paid(job, &r, true, || x.call(job, &r, args, &stage))?;
    let got = ran.json().context("ElevenLabs' alignment wasn't JSON")?;
    let aligned = got["words"].as_array().cloned().unwrap_or_default();
    let out = r.work.join(format!("{}-alignment.json", words::stem(&name)));
    std::fs::write(&out, serde_json::to_vec_pretty(&got)?)?;
    job.output(&out, "document", Role::Main, json!({"file": name, "credits": credits.used}));
    let first: Vec<String> = aligned
        .iter()
        .take(8)
        .map(|w| {
            format!(
                "{} {:.2}–{:.2}",
                w["text"].as_str().unwrap_or("?"),
                w["start"].as_f64().unwrap_or(0.0),
                w["end"].as_f64().unwrap_or(0.0)
            )
        })
        .collect();
    let loss = got["loss"].as_f64();
    Ok(json!({
        "text": format!(
            "Aligned {} words to {name}{}: {}… {}",
            aligned.len(),
            loss.map(|l| format!(" (loss {l:.3})")).unwrap_or_default(),
            first.join(", "),
            credits.line()
        ),
        "words": aligned.len(),
        "loss": loss,
        "first": aligned.iter().take(20).cloned().collect::<Vec<_>>(),
        "file": out,
        "credits": credits.json(),
    }))
}

fn voices(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let r = x.ready(job)?;
    let mut args = vec!["voices".into(), "search".into(), "--page-size=100".into(), "--format=json".into()];
    if let Some(search) = text_param(job, "search") {
        args.push(format!("--search={search}"));
    }
    let ran = x.call(job, &r, args, "listing voices")?;
    let got = ran.json().context("ElevenLabs' voices weren't JSON")?;
    let list: Vec<Value> = got["voices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(
            |v| json!({"name": v["name"], "voice_id": v["voice_id"], "category": v["category"], "labels": v["labels"]}),
        )
        .collect();
    let mut lines =
        vec![format!("{} voices{}:", list.len(), if got["has_more"] == true { " (the first 100)" } else { "" })];
    if !list.is_empty() {
        lines.push(String::new());
        lines.push("| Voice | Id | Kind |".into());
        lines.push("|---|---|---|".into());
        for v in &list {
            lines.push(format!(
                "| {} | `{}` | {} |",
                v["name"].as_str().unwrap_or("?").replace('|', "/"),
                v["voice_id"].as_str().unwrap_or("?"),
                v["category"].as_str().unwrap_or("")
            ));
        }
    }
    Ok(json!({"text": lines.join("\n"), "voices": list, "has_more": got["has_more"] == true}))
}

fn usage(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let r = x.ready(job)?;
    job.progress(None, "reading the account's credits");
    let a = account(&r.cli, &r.key, &r.work, &r.base)?;
    let resets = a
        .resets
        .map(|t| format!(" They reset on {} (in {} days).", words::day(t), ((t - now()).max(0) + 43_199) / 86_400));
    Ok(json!({
        "text": format!(
            "ElevenLabs ({}, {} plan): {} of {} credits used this period, {} left.{}",
            r.key.name,
            a.tier,
            words::thousands(a.used),
            words::thousands(a.limit),
            words::thousands(a.left()),
            resets.unwrap_or_default()
        ),
        "key": r.key.name,
        "tier": a.tier,
        "used": a.used,
        "limit": a.limit,
        "left": a.left(),
        "resets_unix": a.resets,
    }))
}

/// `elevenlabs.run`: any CLI command, a dry run unless `dry_run` is false.
fn run(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let given: Vec<String> = match &job.params["args"] {
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().map(String::from).context("args are words (strings)"))
            .collect::<Result<_, _>>()?,
        Value::String(s) => s.split_whitespace().map(String::from).collect(),
        _ => bail!("args is missing: the words after `elevenlabs`, as [\"voices\", \"search\"]"),
    };
    let dry = job.params["dry_run"].as_bool().unwrap_or(true);
    let confirm = job.params["confirm"].as_bool().unwrap_or(false);
    let file =
        if job.params["file"].as_str().is_some_and(|f| !f.trim().is_empty()) { Some(x.file(job)?) } else { None };
    let mut argv = Vec::new();
    for a in given {
        if a.contains("{file}") {
            let f = file.as_ref().context("args say {file}, but no file was given")?;
            argv.push(a.replace("{file}", &f.to_string_lossy()));
        } else {
            argv.push(a);
        }
    }
    let checked = args::check(&argv).map_err(|e| anyhow!(e))?;
    if let (false, Some(word), false) = (dry, &checked.destructive, confirm) {
        bail!(
            "`{word}` deletes, removes, disables or clones, so a real run needs confirm: true too. Nothing was sent. \
             Check it first with dry_run: true."
        );
    }
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    for p in args::paths(&argv) {
        let at = if Path::new(&p).is_absolute() || p.starts_with('~') { PathBuf::from(&p) } else { job.work.join(&p) };
        x.h.allowed(&at).map_err(|_| anyhow!("{p} is outside the folders this node lets the extension use"))?;
    }
    if dry && !argv.iter().any(|a| a == "--dry-run") {
        argv.push("--dry-run".into());
    }
    let shown = format!("elevenlabs {}", argv.join(" "));
    let before: Vec<PathBuf> = files_in(&job.work);
    let (ran, credits) = if dry {
        // a dry run sends nothing, so it needs no key, but it shows the request with one when there is
        let key = key(&x.h).ok();
        let cli = x.cli()?;
        (x.call_with(job, &cli, key.as_ref(), &job.work, argv, "a dry run")?, None)
    } else {
        let r = x.ready(job)?;
        let stage = format!("running {}", cli::clip(&shown, 80));
        let (ran, credits) = x.paid(job, &r, false, || x.call(job, &r, argv, &stage))?;
        (ran, Some(credits))
    };
    let made: Vec<PathBuf> = files_in(&job.work).into_iter().filter(|f| !before.contains(f)).collect();
    for f in &made {
        let ext = f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let kind = if AUDIO.contains(&ext.as_str()) { "audio" } else { "document" };
        job.output(f, kind, Role::Main, json!({"file": "run"}));
    }
    let out = String::from_utf8_lossy(&ran.stdout).trim().to_string();
    let parsed = ran.json();
    let mut text = vec![if dry {
        format!("Dry run of `{}`: nothing was sent.", cli::clip(&shown, 300))
    } else {
        format!("Ran `{}`.", cli::clip(&shown, 300))
    }];
    if let Some(c) = &credits {
        text.push(c.line());
    }
    if !made.is_empty() {
        text.push(format!("Made {}.", made.iter().map(|f| name_of(f)).collect::<Vec<_>>().join(", ")));
    }
    if !out.is_empty() {
        text.push(format!("```\n{}\n```", cli::clip(&out, 3000)));
    }
    Ok(json!({
        "text": text.join("\n\n"),
        "dry_run": dry,
        "args": shown,
        "output": parsed.unwrap_or_else(|| json!(cli::clip(&out, 100_000))),
        "stderr": cli::clip(ran.stderr.trim(), 4000),
        "files": made,
        "credits": credits.map(|c| c.json()),
    }))
}

/// The files directly in `dir`.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    files
}

// ------------------------------------------------------------------ elevenlabs.live

/// ffmpeg's or yt-dlp's path: the setting's, else the PATH's, else Homebrew's or NixOS's folders (a node's PATH
/// may reach none of them).
fn tool(h: &Handle, setting: &str, name: &str) -> Option<PathBuf> {
    let set = h.settings()[setting].as_str().map(str::trim).unwrap_or_default().to_string();
    if !set.is_empty() {
        return Some(PathBuf::from(set));
    }
    let on_path: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    on_path
        .into_iter()
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/run/current-system/sw/bin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_absolute() && p.is_file())
}

/// A number param: its default when not given, refused outside `min..=max`.
fn number(job: &Job, key: &str, default: f64, min: f64, max: f64) -> anyhow::Result<f64> {
    match &job.params[key] {
        Value::Null => Ok(default),
        v => {
            let n = v.as_f64().with_context(|| format!("{key} is a number"))?;
            if !(min..=max).contains(&n) {
                bail!("{key} is from {min} to {max}, not {n}");
            }
            Ok(n)
        }
    }
}

/// A program's output, killed when it takes longer than `timeout` or the job is cancelled.
fn output_of(mut cmd: Command, timeout: Duration, job: &Job) -> anyhow::Result<(bool, String, String)> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut out = child.stdout.take().context("no stdout")?;
    let mut err = child.stderr.take().context("no stderr")?;
    let o = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let e = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if job.cancelled() || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            job.check()?;
            bail!("it took longer than {}", words::took(timeout));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok((status.success(), o.join().unwrap_or_default(), e.join().unwrap_or_default()))
}

/// What ffmpeg reads: its inputs (a stream's picture and sound can be two), and how.
#[derive(Debug, Clone)]
struct Input {
    urls: Vec<String>,
    /// yt-dlp says whether it's live; a file isn't, and is read at its own pace (`-re`).
    live: Option<bool>,
    title: Option<String>,
    whitelist: &'static str,
}

/// A page's stream, as yt-dlp finds it: a picture small enough for stills with its sound, else the sound alone.
fn page_input(ytdlp: &Path, url: &str, job: &Job) -> anyhow::Result<Input> {
    let mut cmd = Command::new(ytdlp);
    cmd.args(["--ignore-config", "--no-warnings", "--no-playlist", "-f"])
        .arg("best[height<=720][acodec!=none][vcodec!=none]/best[acodec!=none]/bestaudio")
        .args(["--print", "is_live", "--print", "title", "--print", "urls", "--"])
        .arg(url);
    let (ok, out, err) = output_of(cmd, Duration::from_secs(90), job).context("asking yt-dlp for the stream")?;
    if !ok {
        bail!("yt-dlp couldn't find a stream at {url}: {}", cli::clip(err.trim(), 400));
    }
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let live = lines.next().map(|l| l == "True");
    let title = lines.next().map(String::from);
    let urls: Vec<String> = lines.filter(|l| l.starts_with("http")).map(String::from).collect();
    if urls.is_empty() {
        bail!("yt-dlp found no stream's address at {url}");
    }
    Ok(Input { urls, live, title, whitelist: live::PROTOCOLS })
}

/// ffmpeg's arguments: the sound as 16 kHz mono PCM on stdout, and when `stills` says, a still every so many
/// seconds into that folder, each written whole before it's named (`atomic_writing`).
fn ffmpeg_args(input: &Input, stills: Option<(&Path, f64)>) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error"].map(String::from).to_vec();
    for url in &input.urls {
        a.extend(["-protocol_whitelist".into(), input.whitelist.into()]);
        if input.live == Some(false) {
            a.push("-re".into());
        }
        if url.contains(".m3u8") && input.live != Some(false) {
            a.extend(["-live_start_index".into(), "-1".into()]);
        }
        a.extend(["-i".into(), url.clone()]);
    }
    // two inputs: yt-dlp gives the picture first, then the sound
    let sound = if input.urls.len() > 1 { "1:a:0" } else { "0:a:0" };
    a.extend(["-map", sound, "-vn", "-ac", "1", "-ar", "16000", "-f", "s16le", "pipe:1"].map(String::from));
    if let Some((dir, every)) = stills {
        a.extend(["-map".into(), "0:v:0?".into(), "-an".into(), "-vf".into()]);
        a.push(format!("fps=1/{every},scale='min(1280,iw)':-2"));
        a.extend(["-q:v", "4", "-f", "image2", "-atomic_writing", "1"].map(String::from));
        a.push(dir.join("still-%05d.jpg").to_string_lossy().into_owned());
    }
    a
}

/// A piece of the stream's sound, for speech to text: where it starts (in samples from the start) and when each
/// sample reached this machine.
struct Chunk {
    start: u64,
    samples: Vec<i16>,
    arrivals: live::Arrivals,
}

/// What the listener hands the transcriber, in order: a chunk to hear, or a line to post as it is (the stream
/// dropped, or is back).
enum Said {
    Chunk(Chunk),
    Line(String),
}

/// How long a stream may be silent (no sound at all, not a pause) before it's taken for a drop, and how often a
/// dropped stream is tried again.
const STALL: Duration = Duration::from_secs(30);
const RECONNECT: Duration = Duration::from_secs(10);

/// [`RECONNECT`], or POINTMAN_ELEVENLABS_RETRY_MS for a test that can't wait 10 s a try.
fn reconnect() -> Duration {
    std::env::var("POINTMAN_ELEVENLABS_RETRY_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(RECONNECT)
}

/// Wait `d`, or less when the job is cancelled or its time is up: true when it waited the whole time.
fn wait_for(job: &Job, deadline: Instant, d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if job.cancelled() || Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// What a listen has done so far.
#[derive(Default)]
struct Heard {
    posts: usize,
    words: usize,
    quiet: usize,
    stills: usize,
    drops: usize,
    lag: live::Lags,
    stt: live::Lags,
    errors: Vec<String>,
    /// ElevenLabs refused the key: nothing more is sent.
    refused: Option<String>,
}

impl Heard {
    fn error(&mut self, e: String) {
        if self.errors.len() < 20 {
            self.errors.push(e);
        }
    }
}

/// A post of the extension's own in `thread`, tried twice.
fn board_post(h: &Handle, thread: i64, text: &str) -> anyhow::Result<String> {
    let ask = BoardPostRequest {
        thread: ThreadRef { id: format!("thr_{thread}"), ..Default::default() }.into(),
        text: text.to_string(),
        ..Default::default()
    };
    match h.request(&ask) {
        Ok(a) => Ok(a.post.id.clone()),
        Err(_) => {
            std::thread::sleep(Duration::from_secs(2));
            Ok(h.request(&ask)?.post.id.clone())
        }
    }
}

struct Ear {
    h: Handle,
    cli: PathBuf,
    key: Key,
    base: String,
    work: PathBuf,
    thread: i64,
    model: String,
    language: Option<String>,
    transcript: PathBuf,
    heard: Arc<Mutex<Heard>>,
    stop: Arc<AtomicBool>,
}

impl Ear {
    /// Each chunk to ElevenLabs' speech to text, and its words posted as one line in the thread.
    fn transcribe(&self, said: mpsc::Receiver<Said>) {
        let mut n = 0;
        for said in said {
            if self.stop.load(Ordering::SeqCst) || lock(&self.heard).refused.is_some() {
                continue;
            }
            let chunk = match said {
                Said::Chunk(c) => c,
                Said::Line(line) => {
                    if let Err(e) = board_post(&self.h, self.thread, &line) {
                        lock(&self.heard).error(format!("couldn't post {line:?}: {e:#}"));
                    }
                    continue;
                }
            };
            n += 1;
            if let Err(e) = self.one(n, &chunk) {
                let e = format!("{e:#}");
                self.h.log(format!("live: chunk {n}: {e}"));
                let mut heard = lock(&self.heard);
                if e.contains("refused the key") {
                    heard.refused = Some(e.clone());
                }
                heard.error(e);
            }
        }
    }

    fn one(&self, n: usize, chunk: &Chunk) -> anyhow::Result<()> {
        let pcm = self.work.join(format!("chunk-{n:05}.pcm"));
        let bytes: Vec<u8> = chunk.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&pcm, bytes)?;
        let mut args = vec![
            "speech-to-text".into(),
            "convert".into(),
            format!("--file={}", pcm.display()),
            "--file-format=pcm_s16le_16".into(),
            format!("--model-id={}", self.model),
            "--tag-audio-events=false".into(),
            "--timestamps-granularity=word".into(),
            "--format=json".into(),
        ];
        if let Some(language) = &self.language {
            args.push(format!("--language-code={language}"));
        }
        let asked = Instant::now();
        let call = Call::new(&self.cli, &self.work, args)
            .key(Some(&self.key.value))
            .via(VIA)
            .base_url(&self.base)
            .timeout(Duration::from_secs(120));
        let ran = cli::run(&call, &|| false, &mut |_| {});
        let _ = std::fs::remove_file(&pcm);
        let ran = ran?;
        if !ran.ok() {
            return Err(failure(&ran, &self.key));
        }
        let stt = asked.elapsed().as_secs_f64();
        let got = ran.json().context("ElevenLabs' transcript wasn't JSON")?;
        let (said, text) = live::words_of(&got);
        let sample = |s: f64| chunk.start + (s.max(0.0) * live::RATE as f64) as u64;
        let (Some(first), Some(last)) = (said.first(), said.last()) else {
            let mut heard = lock(&self.heard);
            heard.quiet += 1;
            heard.stt.add(stt);
            return Ok(());
        };
        let first_at = chunk.arrivals.at(sample(first.start)).unwrap_or_else(|| live::unix(SystemTime::now()));
        let post = board_post(&self.h, self.thread, &live::line(first_at, &text))?;
        let posted = live::unix(SystemTime::now());
        let at = chunk.start as f64 / live::RATE as f64;
        let row = json!({
            "t_utc": live::iso(first_at),
            "start_s": ((at + first.start) * 100.0).round() / 100.0,
            "end_s": ((at + last.end) * 100.0).round() / 100.0,
            "text": text,
            "post": post,
        });
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.transcript)?;
        writeln!(f, "{row}")?;
        let mut heard = lock(&self.heard);
        heard.posts += 1;
        heard.words += said.len();
        heard.stt.add(stt);
        for w in &said {
            if let Some(arrived) = chunk.arrivals.at(sample(w.start)) {
                heard.lag.add(posted - arrived);
            }
        }
        Ok(())
    }
}

/// Each still ffmpeg writes, posted to the thread as a file.
fn post_stills(
    h: Handle,
    thread: i64,
    asset: String,
    title: &Mutex<String>,
    stills: mpsc::Receiver<(PathBuf, f64)>,
    heard: Arc<Mutex<Heard>>,
) {
    for (file, at) in stills {
        let name = format!("still-{}.jpg", live::clock(at).replace(':', ""));
        let title = lock(title).clone();
        let text = format!("[{} UTC] still", live::clock(at));
        let body = json!({
            "project": {"slug": "elevenlabs", "name": "ElevenLabs"},
            "asset": {"path": format!("{asset}/{}", words::stem(&name)), "title": format!("{title} · {} UTC", live::clock(at)), "kind": "image"},
            "caption": text,
            "files": [{"role": "image", "file": file, "name": name, "mime": "image/jpeg"}],
            "by": "elevenlabs",
            "post": {"thread_id": thread, "text": text},
        });
        let publish = FilesPublishRequest { body: to_struct(&body).into(), ..Default::default() };
        match h.request_timeout(&publish, Duration::from_secs(120)) {
            Ok(_) => lock(&heard).stills += 1,
            Err(e) => {
                h.log(format!("live: couldn't post {}: {e:#}", file.display()));
                lock(&heard).error(format!("a still didn't post: {e:#}"));
            }
        }
    }
}

/// The stills in `dir` not yet seen, oldest first, with when each was written.
fn new_stills(dir: &Path, seen: &mut std::collections::HashSet<PathBuf>) -> Vec<(PathBuf, f64)> {
    let mut found: Vec<(PathBuf, f64)> = files_in(dir)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jpg") && !seen.contains(p))
        .map(|p| {
            let at = std::fs::metadata(&p).and_then(|m| m.modified()).map(live::unix).unwrap_or(0.0);
            (p, at)
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    for (p, _) in &found {
        seen.insert(p.clone());
    }
    found
}

/// `elevenlabs.live`: hear a stream, and post what's said to a thread every so often, with a still now and then.
fn live(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let url =
        text_param(job, "url").context("url is missing: a YouTube live URL, or anything yt-dlp or ffmpeg reads")?;
    let source = live::source(&url).map_err(|e| anyhow!(e))?;
    let thread =
        live::thread_id(&job.params["thread"]).context("thread is missing: the thread to post in (thr_123, or 123)")?;
    let every = number(job, "every_seconds", 15.0, 5.0, 120.0)?;
    let stills_every = number(job, "stills_every_seconds", 60.0, 0.0, 3600.0)?;
    let stills_every = (stills_every > 0.0).then(|| stills_every.max(10.0));
    let minutes = number(job, "minutes", 120.0, 0.1, 480.0)?;
    let model = text_param(job, "model").unwrap_or_else(|| STT_MODEL.to_string());
    let language = text_param(job, "language");
    let ffmpeg = tool(&x.h, "ffmpeg", "ffmpeg")
        .context("ffmpeg isn't on this machine: install it (brew install ffmpeg), or set the ffmpeg setting")?;
    let ytdlp = match &source {
        live::Source::Page(_) => Some(tool(&x.h, "yt_dlp", "yt-dlp").context(
            "yt-dlp isn't on this machine, and it's needed for a page's stream: install it (brew install yt-dlp), \
             set the yt_dlp setting, or give the stream's own address (an .m3u8)",
        )?),
        _ => None,
    };
    let r = x.ready(job)?;
    let resolve = || -> anyhow::Result<Input> {
        match &source {
            live::Source::Page(u) => {
                job.progress(None, "finding the stream with yt-dlp");
                page_input(ytdlp.as_deref().context("no yt-dlp")?, u, job)
            }
            live::Source::Stream(u) => {
                Ok(Input { urls: vec![u.clone()], live: None, title: None, whitelist: live::PROTOCOLS })
            }
            live::Source::File(p) => {
                let path = x.h.allowed(p)?;
                if !path.is_file() {
                    bail!("{} isn't a file", path.display());
                }
                Ok(Input {
                    urls: vec![path.to_string_lossy().into_owned()],
                    live: Some(false),
                    title: None,
                    whitelist: "file",
                })
            }
        }
    };
    // a page's stream may not have started yet (a scheduled live): it's waited for, in the loop below; a file
    // that isn't there fails at once
    let first = match &source {
        live::Source::Page(_) => None,
        _ => Some(resolve()?),
    };
    let title = Arc::new(Mutex::new(url.clone()));
    let transcript = r.work.join("transcript.jsonl");
    let heard = Arc::new(Mutex::new(Heard::default()));
    let stop = Arc::new(AtomicBool::new(false));

    let ((stopped, seconds), credits) = x.paid(job, &r, true, || {
        let (chunks, chunks_rx) = mpsc::channel::<Said>();
        let ear = Ear {
            h: x.h.clone(),
            cli: r.cli.clone(),
            key: Key { name: r.key.name.clone(), value: r.key.value.clone() },
            base: r.base.clone(),
            work: r.work.clone(),
            thread,
            model: model.clone(),
            language: language.clone(),
            transcript: transcript.clone(),
            heard: heard.clone(),
            stop: stop.clone(),
        };
        let ear = std::thread::spawn(move || ear.transcribe(chunks_rx));
        let (stills, stills_rx) = mpsc::channel::<(PathBuf, f64)>();
        let poster = {
            let (h, heard) = (x.h.clone(), heard.clone());
            let asset = format!("live/{}", words::slug(&job.id, 40));
            let title = title.clone();
            std::thread::spawn(move || post_stills(h, thread, asset, &title, stills_rx, heard))
        };
        let dir = r.work.join("stills");
        std::fs::create_dir_all(&dir)?;
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs_f64(minutes * 60.0);
        let chunk_len = (every * live::RATE as f64) as usize;
        let mut pcm: Vec<i16> = Vec::new();
        let mut odd: Option<u8> = None;
        let mut total: u64 = 0;
        let mut pcm_start: u64 = 0;
        let mut arrivals = live::Arrivals::default();
        let mut was_live = first.as_ref().is_some_and(|f| f.live == Some(true));
        let mut input = first;
        // posted once when the stream hasn't started yet, and cleared by its first sound
        let mut waiting = false;
        let mut with_stills = stills_every;
        let mut last_progress = Instant::now();
        // since when the stream has been gone, and whether it ever came
        let mut dropped: Option<f64> = None;
        let stop_why = |job: &Job| -> Option<String> {
            if job.cancelled() {
                return Some("it was cancelled".into());
            }
            if Instant::now() >= deadline {
                return Some(format!("its {minutes} minutes were up"));
            }
            lock(&heard).refused.clone()
        };
        let stopped: String = loop {
            if let Some(why) = stop_why(job) {
                break why;
            }
            let inp = match input.take() {
                Some(i) => i,
                None => match resolve() {
                    // a live page that isn't live now (between a Zoom stream's drop and its restart, or after it):
                    // not back yet, rather than its recording read from the start
                    Ok(i) if was_live && i.live != Some(true) => {
                        wait_for(job, deadline, reconnect());
                        continue;
                    }
                    Ok(i) => {
                        was_live |= i.live == Some(true);
                        if let Some(t) = &i.title {
                            *lock(&title) = t.clone();
                        }
                        i
                    }
                    Err(e) => {
                        x.h.log(format!("live: the stream isn't there yet: {e:#}"));
                        if total == 0 && !waiting {
                            waiting = true;
                            let now = live::unix(SystemTime::now());
                            let _ = chunks.send(Said::Line(format!(
                                "[{} UTC] Waiting for the stream to start.",
                                live::clock(now)
                            )));
                        }
                        wait_for(job, deadline, reconnect());
                        continue;
                    }
                },
            };
            let mut child = Command::new(&ffmpeg)
                .args(ffmpeg_args(&inp, with_stills.map(|s| (dir.as_path(), s))))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("starting {}", ffmpeg.display()))?;
            let mut out = child.stdout.take().context("ffmpeg gave no stdout")?;
            let mut err = child.stderr.take().context("ffmpeg gave no stderr")?;
            let tail = std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                s
            });
            let (bytes, bytes_rx) = mpsc::channel::<(Vec<u8>, SystemTime)>();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 6400];
                loop {
                    match out.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if bytes.send((buf[..n].to_vec(), SystemTime::now())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let heard_before = total;
            let mut last_sound = Instant::now();
            let why: Option<String> = loop {
                if let Some(why) = stop_why(job) {
                    break Some(why);
                }
                match bytes_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok((b, at)) => {
                        last_sound = Instant::now();
                        if std::mem::take(&mut waiting) {
                            let now = live::unix(SystemTime::now());
                            let _ =
                                chunks.send(Said::Line(format!("[{} UTC] The stream has started.", live::clock(now))));
                        }
                        if let Some(since) = dropped.take() {
                            let now = live::unix(SystemTime::now());
                            let gone = words::took(Duration::from_secs_f64((now - since).max(0.0)));
                            let _ = chunks.send(Said::Line(format!(
                                "[{} UTC] The stream is back (it was gone {gone}).",
                                live::clock(now)
                            )));
                        }
                        let mut b = b;
                        if let Some(o) = odd.take() {
                            b.insert(0, o);
                        }
                        if b.len() % 2 == 1 {
                            odd = b.pop();
                        }
                        pcm.extend(b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)));
                        total += (b.len() / 2) as u64;
                        arrivals.mark(total, live::unix(at));
                        if pcm.len() >= chunk_len {
                            let cut = live::quiet_cut(&pcm);
                            let samples: Vec<i16> = pcm.drain(..cut).collect();
                            let _ = chunks.send(Said::Chunk(Chunk {
                                start: pcm_start,
                                samples,
                                arrivals: arrivals.clone(),
                            }));
                            pcm_start += cut as u64;
                            arrivals.forget_before(pcm_start);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if last_sound.elapsed() > STALL => break None,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                }
                for s in new_stills(&dir, &mut seen) {
                    let _ = stills.send(s);
                }
                if last_progress.elapsed() >= Duration::from_secs(5) {
                    last_progress = Instant::now();
                    let h = lock(&heard);
                    let lag = h.lag.median().map(|l| format!(", lag {l:.1} s")).unwrap_or_default();
                    let heard_s = total / live::RATE as u64;
                    job.progress_detail(
                        None,
                        format!(
                            "listening: {} heard, {} posts, {} stills{lag}",
                            words::took(Duration::from_secs(heard_s)),
                            h.posts,
                            h.stills
                        )
                        .as_str(),
                        json!({"posts": h.posts, "stills": h.stills, "heard_seconds": heard_s}),
                    );
                }
            };
            let _ = child.kill();
            let _ = child.wait();
            let said = tail.join().unwrap_or_default();
            odd = None;
            if let Some(why) = why {
                break why;
            }
            // ffmpeg stopped by itself, or went quiet: a picture-less stream, the end of a file, or a drop
            if total == heard_before && with_stills.is_some() && said.contains("does not contain any stream") {
                with_stills = None;
                lock(&heard).error("the stream has no picture, so there are no stills".into());
                input = Some(inp);
                continue;
            }
            if inp.live == Some(false) {
                break "the stream ended".into();
            }
            // never heard yet (an address that isn't up): still waiting for it to start, not a drop
            if total == 0 {
                if !waiting {
                    waiting = true;
                    let now = live::unix(SystemTime::now());
                    let _ =
                        chunks.send(Said::Line(format!("[{} UTC] Waiting for the stream to start.", live::clock(now))));
                }
                wait_for(job, deadline, reconnect());
                continue;
            }
            // what was heard before the drop is posted first, then the drop
            if !pcm.is_empty() {
                let samples = std::mem::take(&mut pcm);
                let n = samples.len() as u64;
                if n >= live::RATE as u64 {
                    let _ = chunks.send(Said::Chunk(Chunk { start: pcm_start, samples, arrivals: arrivals.clone() }));
                }
                pcm_start += n;
                arrivals.forget_before(pcm_start);
            }
            let said = cli::clip(said.trim(), 300);
            x.h.log(format!("live: the stream dropped (ffmpeg: {said}); trying again every 10 s"));
            if dropped.is_none() {
                let now = live::unix(SystemTime::now());
                dropped = Some(now);
                lock(&heard).drops += 1;
                let _ = chunks.send(Said::Line(format!(
                    "[{} UTC] The stream dropped: trying again every 10 s.",
                    live::clock(now)
                )));
            }
            wait_for(job, deadline, reconnect());
        };
        // the rest of the sound, and the last stills
        if pcm.len() >= live::RATE && !job.cancelled() {
            let _ = chunks.send(Said::Chunk(Chunk {
                start: pcm_start,
                samples: std::mem::take(&mut pcm),
                arrivals: arrivals.clone(),
            }));
        }
        if job.cancelled() {
            stop.store(true, Ordering::SeqCst);
        }
        for s in new_stills(&dir, &mut seen) {
            let _ = stills.send(s);
        }
        drop(chunks);
        drop(stills);
        let _ = ear.join();
        let _ = poster.join();
        Ok((stopped, total / live::RATE as u64))
    })?;

    let h = lock(&heard);
    let lag = match (h.lag.median(), h.lag.worst()) {
        (Some(m), Some(w)) => {
            format!("lag median {m:.1} s, worst {w:.1} s (a word reaching {} to its post)", machine(&x.h))
        }
        _ => "no words, so no lag".into(),
    };
    let title = lock(&title).clone();
    let summary = format!(
        "Stopped listening to {title}: {stopped}. Heard {}: {} posts, {} words, {} stills{}; {lag}. {}",
        words::took(Duration::from_secs(seconds)),
        h.posts,
        h.words,
        h.stills,
        if h.drops > 0 { format!(", {} drops", h.drops) } else { String::new() },
        credits.line()
    );
    if let Err(e) = board_post(&x.h, thread, &summary) {
        x.h.log(format!("live: couldn't post the summary: {e:#}"));
    }
    if transcript.is_file() {
        job.output(&transcript, "document", Role::Main, json!({"file": "transcript", "credits": credits.used}));
    }
    job.check()?;
    Ok(json!({
        "text": summary,
        "stopped": stopped,
        "heard_seconds": seconds,
        "posts": h.posts,
        "words": h.words,
        "quiet_chunks": h.quiet,
        "stills": h.stills,
        "drops": h.drops,
        "lag": h.lag.json(),
        "stt": h.stt.json(),
        "errors": h.errors,
        "transcript": transcript,
        "credits": credits.json(),
    }))
}

/// Health's two parts: the CLI (its version and where it came from) and the key (with the account's credits).
fn health(x: &Ext) -> Vec<Part> {
    let h = &x.h;
    let scratch = scratch(h);
    let found = find(h);
    let cli_is =
        |state: State, needs: Vec<Action>, why: String| health_part("cli", "cli", "ElevenLabs CLI", state, needs, why);
    let cli_part = match &found {
        Some(f) => match cli::version(&f.path, &scratch) {
            Ok(v) => Part { version: Some(v.clone()), ..cli_is(State::Ok, vec![], format!("{v}, {}", f.origin())) },
            Err(e) => cli_is(State::Failing, vec![], format!("{} doesn't run: {e:#}", f.origin())),
        },
        None => {
            let why = match lock_try(&x.installing) {
                None => "not on this machine yet: installing ElevenLabs' release build".to_string(),
                Some(Some(e)) => e,
                Some(None) => "not on this machine: the extension installs ElevenLabs' release build".to_string(),
            };
            cli_is(State::Failing, vec![Action::Update], why)
        }
    };
    let part = |state: State, needs: Vec<Action>, why: String| {
        health_part("key", "account", "ElevenLabs key", state, needs, why)
    };
    let key_part = match key(h) {
        Err(NoKey::None(why)) => part(State::Failing, vec![Action::Auth], why),
        Err(NoKey::Other(why)) => part(State::Unspecified, vec![], why),
        Ok(k) => match &found {
            None => part(State::Unspecified, vec![], format!("{}: waiting for the CLI to read the account", k.name)),
            Some(f) => {
                let mut kept = lock(&x.account);
                let fresh = kept.as_ref().filter(|(at, name, _)| at.elapsed() < ACCOUNT_KEPT && *name == k.name);
                let read = match fresh {
                    Some((_, _, read)) => read.clone(),
                    None => {
                        let read = account(&f.path, &k, &scratch, &base_url(h)).map_err(|e| format!("{e:#}"));
                        *kept = Some((Instant::now(), k.name.clone(), read.clone()));
                        read
                    }
                };
                match read {
                    Ok(a) => part(
                        State::Ok,
                        vec![],
                        format!(
                            "{}: {} plan, {} of {} credits left",
                            k.name,
                            a.tier,
                            words::thousands(a.left()),
                            words::thousands(a.limit)
                        ),
                    ),
                    Err(why) if why.contains("refused the key") => part(State::Failing, vec![Action::Auth], why),
                    Err(why) => part(State::Unspecified, vec![], format!("couldn't read the account: {why}")),
                }
            }
        },
    };
    vec![cli_part, key_part]
}

fn health_part(id: &str, kind: &str, name: &str, state: State, needs: Vec<Action>, why: String) -> Part {
    Part {
        id: id.into(),
        kind: kind.into(),
        name: Some(name.into()),
        state: state.into(),
        needs: needs.into_iter().map(Into::into).collect(),
        why: Some(why),
        ..Default::default()
    }
}

/// The install's last error, or None while one is going.
fn lock_try(m: &Mutex<Option<String>>) -> Option<Option<String>> {
    match m.try_lock() {
        Ok(g) => Some(g.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn fix(x: &Ext, fix: &HealthFixRequest) -> anyhow::Result<HealthFixResponse> {
    let message = match (fix.part.as_str(), fix.action.as_known()) {
        ("key", Some(Action::Auth)) => {
            *lock(&x.account) = None;
            "Add an ElevenLabs API key with /elevenlabs add in any thread: the app opens a form, so it never goes in a \
             post. It's on elevenlabs.io: Developers › API Keys."
                .to_string()
        }
        ("cli", Some(Action::Update)) => match find(&x.h) {
            Some(f) if f.from != cli::From::Installed => {
                format!("The CLI in use is {}: update it there (Homebrew's: brew upgrade elevenlabs).", f.origin())
            }
            _ => {
                let s = state(&x.h);
                let _one = lock(&x.installing);
                let (path, version) = install::install(&s, &releases()).context("installing ElevenLabs' CLI")?;
                format!("Installed ElevenLabs' CLI {version} at {}.", path.display())
            }
        },
        _ => "Nothing to fix from here.".to_string(),
    };
    Ok(HealthFixResponse { message, ..Default::default() })
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let x = Arc::new(Ext { h: ext.handle(), installing: Mutex::new(None), account: Mutex::new(None) });

    // a machine without the CLI gets ElevenLabs' own build, in the state folder, as soon as the extension starts
    let xs = x.clone();
    ext.on_start(move |_: &Handle| {
        if find(&xs.h).is_none() {
            let xs = xs.clone();
            std::thread::spawn(move || {
                let _ = xs.cli();
            });
        }
    });

    type Cmd = fn(&Ext, &Job) -> anyhow::Result<Value>;
    let commands: [(&str, Cmd); 8] = [
        ("elevenlabs.isolate", isolate),
        ("elevenlabs.tts", tts),
        ("elevenlabs.stt", stt),
        ("elevenlabs.align", align),
        ("elevenlabs.voices", voices),
        ("elevenlabs.usage", usage),
        ("elevenlabs.run", run),
        ("elevenlabs.live", live),
    ];
    for (kind, f) in commands {
        let xc = x.clone();
        ext.command(kind, move |job: &Job| f(&xc, job));
    }

    // `/elevenlabs add` posted (an agent, an older app): the apps open the key's form for it
    ext.command("elevenlabs.add", |_: &Job| {
        Ok(json!({"text": "An ElevenLabs API key is added in the Pointman app, so it never goes in a post: type \
                           `/elevenlabs add` there and paste it. It's on elevenlabs.io: Developers › API Keys."}))
    });

    let xh = x.clone();
    ext.on(move |_: HealthCheckRequest| Ok(HealthCheckResponse { parts: health(&xh), ..Default::default() }));
    let xf = x.clone();
    ext.on(move |req: HealthFixRequest| fix(&xf, &req));

    ext.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_key_is_only_the_key() {
        assert_eq!(key_of("  sk_abc123\n"), "sk_abc123");
        assert_eq!(key_of("ELEVENLABS_API_KEY=\"sk_abc123\""), "sk_abc123");
    }

    #[test]
    fn the_key_comes_out_of_the_vaults_sealed_record() {
        let key = format!("sk_{}", "a1B2c3D4".repeat(6));
        let sealed = format!(r#"{{"kind":"env","vars":{{"elevenlabs.main":"{key}"}}}}"#);
        assert_eq!(key_of(&sealed), key);
        assert_eq!(key_of(&format!("ELEVENLABS_API_KEY={key}\n")), key);
        let old = "0123456789abcdef0123456789abcdef";
        assert_eq!(key_of(&format!(r#"{{"kind":"env","vars":{{"elevenlabs.old":"{old}"}}}}"#)), old);
        assert_eq!(sk_key("task_0123456789abcdefghij"), None, "not inside a longer word");
        assert_eq!(hex_key(&format!("{old}0")), None, "33 hex digits aren't a key");
    }

    #[test]
    fn credits_say_when_they_were_shared() {
        let c = Credits { used: Some(1000), left: Some(99_000), shared: false, pending: false };
        assert_eq!(c.line(), "Used 1,000 credits; 99,000 left.");
        assert!(Credits { shared: true, ..c }.line().starts_with("The account used 1,000 credits while it ran"));
        assert_eq!(Credits::default().line(), "The credits it used couldn't be read.");
        let late = Credits { used: None, left: Some(99_000), shared: false, pending: true };
        assert!(late.line().starts_with("ElevenLabs hadn't counted its credits 20 s after it finished (99,000 left"));
    }

    #[test]
    fn errors_name_the_refused_key() {
        let ran = Ran {
            code: Some(1),
            stdout: vec![],
            stderr: r#"{"error": {"code": 401, "message": "Invalid API key"}}"#.into(),
        };
        let key = Key { name: "elevenlabs.main".into(), value: "sk_x".into() };
        let e = failure(&ran, &key).to_string();
        assert!(e.contains("refused the key in elevenlabs.main") && e.contains("/elevenlabs add"), "{e}");
    }
}
