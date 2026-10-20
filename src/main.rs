//! The extension's process: the node starts it (`bin/elevenlabs`, from the extension's folder) and talks to it
//! over stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! Each command runs ElevenLabs' CLI on a thread of its own (no lane), so an 18-minute isolate holds up nobody,
//! and can be cancelled: the CLI is killed. Its key comes from the vault for each call (`elevenlabs.<label>`),
//! into that call's environment only. A paid call reads the account's credits before and after, and says what it
//! used. What it makes goes back as the command's outputs, posted where it was asked.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use elevenlabs::cli::{self, Call, Found, Ran};
use elevenlabs::{args, install, words};
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
    PathBuf::from(h.node()["state_dir"].as_str().unwrap_or_default())
}

fn machine(h: &Handle) -> String {
    h.node()["machine"].as_str().filter(|m| !m.is_empty()).unwrap_or("this machine").to_string()
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

/// The key as pasted, without what's often pasted around it (`ELEVENLABS_API_KEY=`, quotes, spaces).
fn key_of(pasted: &str) -> String {
    let v = pasted.trim();
    let v = v.rsplit_once('=').map_or(v, |(_, k)| k);
    v.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string()
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
    let listed = h
        .request("secret.list", json!({"prefix": "elevenlabs."}))
        .map_err(|e| NoKey::Other(format!("couldn't ask the node for the vault's ElevenLabs keys: {e:#}")))?;
    let mut names: Vec<String> =
        listed["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
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
    job.output(&out, "audio", "main", json!({"file": name, "credits": credits.used}));
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
    job.output(&out, "audio", "main", json!({"file": "tts", "voice": name, "credits": credits.used}));
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
    job.output(&txt, "document", "main", meta.clone());
    job.output(&full, "document", "main", meta);
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
    job.output(&out, "document", "main", json!({"file": name, "credits": credits.used}));
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
        job.output(f, kind, "main", json!({"file": "run"}));
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

/// Health's two parts: the CLI (its version and where it came from) and the key (with the account's credits).
fn health(x: &Ext) -> Value {
    let h = &x.h;
    let scratch = scratch(h);
    let found = find(h);
    let cli_part = match &found {
        Some(f) => match cli::version(&f.path, &scratch) {
            Ok(v) => {
                json!({"id": "cli", "kind": "cli", "name": "ElevenLabs CLI", "version": v, "state": "ok", "needs": [],
                            "why": format!("{v}, {}", f.origin())})
            }
            Err(e) => json!({"id": "cli", "kind": "cli", "name": "ElevenLabs CLI", "state": "failing", "needs": [],
                             "why": format!("{} doesn't run: {e:#}", f.origin())}),
        },
        None => {
            let why = match lock_try(&x.installing) {
                None => "not on this machine yet: installing ElevenLabs' release build".to_string(),
                Some(Some(e)) => e,
                Some(None) => "not on this machine: the extension installs ElevenLabs' release build".to_string(),
            };
            json!({"id": "cli", "kind": "cli", "name": "ElevenLabs CLI", "state": "failing", "needs": ["update"], "why": why})
        }
    };
    let part = |state: &str, needs: Vec<&str>, why: String| json!({"id": "key", "kind": "account", "name": "ElevenLabs key", "state": state, "needs": needs, "why": why});
    let key_part = match key(h) {
        Err(NoKey::None(why)) => part("failing", vec!["auth"], why),
        Err(NoKey::Other(why)) => part("unknown", vec![], why),
        Ok(k) => match &found {
            None => part("unknown", vec![], format!("{}: waiting for the CLI to read the account", k.name)),
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
                        "ok",
                        vec![],
                        format!(
                            "{}: {} plan, {} of {} credits left",
                            k.name,
                            a.tier,
                            words::thousands(a.left()),
                            words::thousands(a.limit)
                        ),
                    ),
                    Err(why) if why.contains("refused the key") => part("failing", vec!["auth"], why),
                    Err(why) => part("unknown", vec![], format!("couldn't read the account: {why}")),
                }
            }
        },
    };
    json!([cli_part, key_part])
}

/// The install's last error, or None while one is going.
fn lock_try(m: &Mutex<Option<String>>) -> Option<Option<String>> {
    match m.try_lock() {
        Ok(g) => Some(g.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn fix(x: &Ext, params: &Value) -> anyhow::Result<Value> {
    let message = match (params["part"].as_str(), params["action"].as_str()) {
        (Some("key"), Some("auth")) => {
            *lock(&x.account) = None;
            "Add an ElevenLabs API key with /elevenlabs add in any thread: the app opens a form, so it never goes in a \
             post. It's on elevenlabs.io: Developers › API Keys."
                .to_string()
        }
        (Some("cli"), Some("update")) => match find(&x.h) {
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
    Ok(json!({"message": message}))
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
    let commands: [(&str, Cmd); 7] = [
        ("elevenlabs.isolate", isolate),
        ("elevenlabs.tts", tts),
        ("elevenlabs.stt", stt),
        ("elevenlabs.align", align),
        ("elevenlabs.voices", voices),
        ("elevenlabs.usage", usage),
        ("elevenlabs.run", run),
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
    ext.method("health.check", move |_| Ok(health(&xh)));
    let xf = x.clone();
    ext.method("health.fix", move |params: Value| fix(&xf, &params));

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
