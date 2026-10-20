# elevenlabs

A Pointman extension for [ElevenLabs](https://elevenlabs.io): voice isolation, text to speech, transcripts, word
timings, voices and credits, from any thread or agent, on the machines that have it. It's a thin wrapper of
ElevenLabs' official CLI ([elevenlabs/cli](https://github.com/elevenlabs/cli), `elevenlabs <service> <verb>`):
each command is one CLI call, and `elevenlabs.run` passes anything else through.

One Rust binary, `bin/elevenlabs`, on the [pointman-extension](https://github.com/pointman-ai/pointman-extension)
crate, for macOS and Linux (Apple silicon, Intel and ARM).

## Commands

| Command | Slash | What it does |
|---|---|---|
| `elevenlabs.isolate {file}` | `/elevenlabs isolate` | Isolates speech from background noise (`audio-isolation convert`). The clean audio is posted back as `<name>-isolated.mp3` |
| `elevenlabs.tts {text, voice, model?}` | `/elevenlabs tts` | Text to speech (`text-to-speech convert`). `voice` is a name (or the start of one) or an id. The audio is posted back |
| `elevenlabs.stt {file, model?, language?}` | `/elevenlabs stt` | A transcript (`speech-to-text convert`, `scribe_v1` unless `model` says), posted back as `.txt` and `.json` (word timings) |
| `elevenlabs.align {file, text}` | `/elevenlabs align` | Each word's start and end (`forced-alignment create`), posted back as `.json` |
| `elevenlabs.voices {search?}` | `/elevenlabs voices` | The voices the key can use: names, ids and kinds |
| `elevenlabs.usage` | `/elevenlabs usage` | The plan, credits used and left this period, and when they reset |
| `elevenlabs.run {args, dry_run, confirm?, file?}` | `/elevenlabs run` | Any CLI command (`args` are the words after `elevenlabs`). A dry run unless `dry_run: false` |
| `elevenlabs.add` | `/elevenlabs add` | Opens the app's form for an API key |

- **`file`** is a board file (`pointman://asset/<id>`, which the node fetches) or a path in the folders the
  node lets extensions use.
- **Long jobs** run in the background, each on a thread of its own, so an 18-minute isolate holds up nobody.
  Each says what it's doing and for how long so far, and cancelling one stops the CLI at once.
- **Credits:** each paid command reads the account's credits before and after, and says what it used and
  what's left. When another of this machine's ElevenLabs jobs ran at the same time, it says so, since the
  difference is then theirs too. Use from elsewhere with the same key (the CLI on a laptop) isn't seen.

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

## Settings

In `~/.config/pointman/extensions/elevenlabs/config.toml` (or `pointman config set
node.extensions.elevenlabs.<key> <value>`):

| Setting | Default | |
|---|---|---|
| `cli` | none | The `elevenlabs` to run, when it isn't on the PATH or in Homebrew's folders |
| `account` | the first | The label of the vault's key to use (`main` for `elevenlabs.main`), never the key itself |

## Guidance for agents

[SKILLS.md](SKILLS.md), the `[behavior]` beside `[run]`: which command for what, credits first, dry runs first,
never a key in a post. Every agent run on a node with the extension gets it.

## Development

- `cargo test`: the commands under the SDK's stand-in node, with a fake `elevenlabs` (a shell script that logs
  each call and answers as the CLI does) and a fake GitHub release for the installer. Nothing reaches
  ElevenLabs or GitHub.
- `cargo test -- --ignored`: the real installer, from ElevenLabs' newest release (online).
- `tests/live.rs`: the real ElevenLabs, with a key given to that one command by `pointman secrets run`. It
  spends credits.
- **Release:** bump `version` in extension.toml and Cargo.toml, then push the tag (`git tag v0.1.1 && git push
  origin v0.1.1`). The release workflow builds the four platforms and attaches
  `elevenlabs-<version>-<platform>.tar.gz` for each, with SKILLS.md.

MIT licensed.
