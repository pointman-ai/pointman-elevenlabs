# elevenlabs

A Pointman extension for [ElevenLabs](https://elevenlabs.io): voice isolation, text to speech, transcripts, word
timings, voices and credits, from any thread or agent, on the machines that have it. It's a thin wrapper of
ElevenLabs' official CLI ([elevenlabs/cli](https://github.com/elevenlabs/cli), `elevenlabs <service> <verb>`):
each command is one CLI call, and `elevenlabs.run` passes anything else through.

One Rust binary, `bin/elevenlabs`, on the [pointman-extension](https://github.com/pointman-ai/pointman-extension)
crate (the node's typed protocol: each method's params and result are its messages), for macOS and Linux
(Apple silicon, Intel and ARM).

## Commands

| Command | Slash | What it does |
|---|---|---|
| `elevenlabs.isolate {file}` | `/elevenlabs isolate` | Isolates speech from background noise (`audio-isolation convert`). The clean audio is posted back as `<name>-isolated.mp3` |
| `elevenlabs.tts {text, voice, model?}` | `/elevenlabs tts` | Text to speech (`text-to-speech convert`). `voice` is a name (or the start of one) or an id. The audio is posted back |
| `elevenlabs.stt {file, model?, language?}` | `/elevenlabs stt` | A transcript (`speech-to-text convert`, `scribe_v1` unless `model` says), posted back as `.txt` and `.json` (word timings) |
| `elevenlabs.align {file, text}` | `/elevenlabs align` | Each word's start and end (`forced-alignment create`), posted back as `.json` |
| `elevenlabs.voices {search?}` | `/elevenlabs voices` | The voices the key can use: names, ids and kinds |
| `elevenlabs.live {url, thread, every_seconds?, stills_every_seconds?, language?, model?, minutes?}` | `/elevenlabs live` | Listens to a live stream (a YouTube live URL, or anything yt-dlp or ffmpeg reads) and posts what's said to `thread` as `[HH:MM:SS UTC] words`, every 15 s, with a still every 60 s. Writes `transcript.jsonl` |
| `elevenlabs.usage` | `/elevenlabs usage` | The plan, credits used and left this period, and when they reset |
| `elevenlabs.run {args, dry_run, confirm?, file?}` | `/elevenlabs run` | Any CLI command (`args` are the words after `elevenlabs`). A dry run unless `dry_run: false` |
| `elevenlabs.add` | `/elevenlabs add` | Opens the app's form for an API key |

- **`file`** is a board file (`pointman://asset/<id>`, which the node fetches) or a path in the folders the
  node lets extensions use.
- **Long jobs** run in the background, each on a thread of its own, so an 18-minute isolate holds up nobody.
  Each says what it's doing and for how long so far, and cancelling one stops the CLI at once.
- **Credits:** each paid command reads the account's credits before and after, and says what it used and
  what's left. ElevenLabs can count a call a few seconds after it answers, so an unchanged balance is read again
  (after 3, 6 and 12 s) before it says the credits aren't counted yet. It never says 0. When another of this
  machine's ElevenLabs jobs ran at the same time, it says so, since the difference is then theirs too. Use from
  elsewhere with the same key (the CLI on a laptop) isn't seen.

### `elevenlabs.run`

- **A dry run by default:** `--dry-run` is added unless `dry_run` is `false`, so it shows the request without
  sending it. A dry run needs no key.
- **Deleting, removing, disabling or cloning** (any of its words: `delete`, `remove`, `disable`, `clone`, `ivc`,
  `pvc`) needs `confirm: true` too, for a real run.
- **Never passed on:** `--api-key`, `--xi-api-key`, `--base-url` (it would send the key elsewhere), `--debug`
  (it dumps the request), `auth`, `say` (it plays aloud on the machine), `residency` (it moves every call's
  region) and `api-keys` (a new key would land in a post).
- **Paths:** every path in `args` (an `@file` the CLI uploads, `/…`, `~/…`, `./…`, anything with `..`) must be
  in the extension's folders. `{file}` in `args` is the `file` param's path. A file the CLI writes into the
  command's folder (`-o rain.mp3`) is posted back.

## The key

An ElevenLabs API key (elevenlabs.io: Developers › API Keys), added with `/elevenlabs add` in any thread: the
app's form seals it in the vault as `elevenlabs.<label>`, so it never goes in a post. The extension may open
only `elevenlabs.*` items.

- **Each call** gets it as `ELEVENLABS_API_KEY` in that call's environment, and nowhere else: never on the
  command line, in a file or a `.env`, or in the log, and the extension never runs `elevenlabs auth login`.
  Whatever the CLI says back has the key taken out.
- **A folder of its own:** the CLI reads a `.env` in its working folder and the folders above it, so each call
  runs in the command's own work folder, inside the extension's state folder, with only the node's `PATH`,
  `HOME`, `USER`, `LANG`, `LC_ALL` and `TMPDIR` besides.
- **Where it goes is pinned:** each call sets `ELEVENLABS_BASE_URL` to the `base_url` setting (https only), and
  every proxy (`ELEVENLABS_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and the lowercase ones) and TLS override
  (`ELEVENLABS_INSECURE`, `ELEVENLABS_CA_BUNDLE`) empty. A `.env` anywhere above can't send the key elsewhere,
  whatever a later CLI reads from one. `tests/real_cli.rs` checks it with the real CLI.
- **Which key:** the first by label, or the one the `account` setting names.
- **Health:** with no key, the `key` part is failing and needs `auth`. A key ElevenLabs refuses is the same,
  naming it. With one, it says the plan and the credits left.

## The CLI

The `cli` setting's, else `elevenlabs` on the node's PATH, in `/opt/homebrew/bin` or in `/usr/local/bin`
(Homebrew's: `brew install elevenlabs/tap/elevenlabs`). On a machine with none, the extension installs
ElevenLabs' newest release for its platform into its state folder (`cli/<version>/elevenlabs`):
`elevenlabs-cli-<target>.tar.gz` (the musl build on Linux, so NixOS runs it), checked against the sha256
ElevenLabs publishes beside it, and refused if it doesn't match. Health's `cli` part says its version and where
it came from. Its `update` fix installs the newest again, for the extension's own copy only.

### `elevenlabs.live`

- **Chunks, not realtime:** ElevenLabs' CLI has batch speech to text only, so ffmpeg turns the stream into 16 kHz
  mono PCM and each `every_seconds` of it goes to `speech-to-text convert` (`--file-format=pcm_s16le_16`). Each
  chunk is cut at its quietest moment in its last 3 s, so a cut falls in a pause rather than through a word.
- **Posts** come from the extension's robot account (`permissions.post`, `board.post`): one line per chunk with
  words, the clock being when its first word reached the machine. An `@` in what's said gets a zero-width space,
  so it wakes nobody. Stills go up as files (`files.publish`) every `stills_every_seconds` (0 for none).
- **`transcript.jsonl`:** one line a post, `{t_utc, start_s, end_s, text, post}`; `start_s` and `end_s` count
  the stream's sound heard since the job started. It's posted back when the job ends.
- **Drops:** when ffmpeg stops, or there's no sound at all for 30 s, it posts a line saying the stream dropped,
  tries again every 10 s (asking yt-dlp afresh, and waiting while a live page isn't live), and posts a line when
  it's back. The transcript goes on in the same file. It stops at `minutes` (120 unless given), when cancelled,
  or when a file ends, and posts a summary: posts, stills, drops, lag and credits.
- **Lag** is measured for every word: from its sound reaching the machine to its post being made. The stream's
  own delay (YouTube's, usually several seconds) comes before that and isn't in it.
- **Where streams come from:** `https`, `http`, `rtmp(s)`, `srt` and `rtsp` addresses, or a file in the
  extension's folders. A page (YouTube, Twitch) goes through yt-dlp; a playlist or media address straight to
  ffmpeg, limited to network protocols (`-protocol_whitelist`), so a playlist can't point it at a local file.

## Settings

In `~/.config/pointman/extensions/elevenlabs/config.toml` (or `pointman config set
node.extensions.elevenlabs.<key> <value>`):

| Setting | Default | |
|---|---|---|
| `cli` | none | The `elevenlabs` to run, when it isn't on the PATH or in Homebrew's folders |
| `base_url` | `https://api.elevenlabs.io` | Where every call goes: change it only for an account whose API lives elsewhere (EU residency) |
| `ffmpeg` | the PATH's, else Homebrew's | The ffmpeg `elevenlabs.live` reads streams with |
| `yt_dlp` | the PATH's, else Homebrew's | The yt-dlp `elevenlabs.live` finds a page's stream with |
| `account` | the first | The label of the vault's key to use (`main` for `elevenlabs.main`), never the key itself |

## Guidance for agents

[SKILLS.md](SKILLS.md), the `[behavior]` beside `[run]`: which command for what, credits first, dry runs first,
never a key in a post. Every agent run on a node with the extension gets it.

## Development

- `cargo test`: the commands under the SDK's stand-in node, with a fake `elevenlabs` (a shell script that logs
  each call and answers as the CLI does) and a fake GitHub release for the installer, and the real CLI against
  two local servers (`tests/real_cli.rs`; skipped where there's no CLI, and the test workflow downloads one).
  Nothing reaches ElevenLabs or GitHub.
- `cargo test -- --ignored`: the real installer, from ElevenLabs' newest release (online).
- `tests/live.rs`: the real ElevenLabs, with a key given to that one command by `pointman secrets run`. It
  spends credits.
- **Release:** bump `version` in extension.toml and Cargo.toml, then push the tag (`git tag v0.1.1 && git push
  origin v0.1.1`). The release workflow builds the four platforms and attaches
  `elevenlabs-<version>-<platform>.tar.gz` for each, with SKILLS.md.

MIT licensed.
