---
name: elevenlabs
description: ElevenLabs through the elevenlabs extension: voice isolation, text to speech, transcripts, live streams heard into a channel, word timings, voices and credits, with the vault's key.
---

# ElevenLabs

ElevenLabs runs through the `elevenlabs` extension on the machine that has it: ask with `create_job`
(or `/elevenlabs <verb>` in a channel). Its key comes from the vault for each call. You never need it, and
you never run `elevenlabs auth` or write a `.env`.

| For | Command | Gives |
|---|---|---|
| Clean up a voice track (noise, room) | `elevenlabs.isolate {file}` | the isolated audio, posted back |
| Speech from text | `elevenlabs.tts {text, voice, model?}` | audio, posted back |
| A transcript | `elevenlabs.stt {file, model?, language?}` | `.txt` and `.json` (word timings), posted back |
| Word timings for a script you have | `elevenlabs.align {file, text}` | `.json`, posted back |
| Hear a live stream (a YouTube live) | `elevenlabs.live {url, channel, every_seconds?: 15, stills_every_seconds?: 60, language?, minutes?: 120}` | `[HH:MM:SS UTC] words` posts in `channel`, stills, `transcript.jsonl` |
| Which voices there are | `elevenlabs.voices {search?}` | names and ids |
| Credits used and left | `elevenlabs.usage` | the plan, used, left, reset day |
| Anything else in the CLI | `elevenlabs.run {args, dry_run, confirm?, file?}` | its output |

- **`file`** is a board file (`pointman://asset/<id>`: the node fetches it) or a path in the
  extension's folders. Long files are fine: commands run in the background, report progress, and can be
  cancelled.
- **Credits first.** Before a long job (isolating or transcribing many minutes), run `elevenlabs.usage`.
  Each paid command says what it used and what's left.
- **`live` runs until `minutes`** or until it's cancelled, and spends credits all the while (batch speech to
  text on each chunk): give `minutes` for a test, and cancel it when the stream is over. Posts lag the speaker
  by about `every_seconds` plus a few seconds. A dropped stream is tried again every 10 s, with a line saying so; 2 min with no words gets a `2 min, no words.` line, and a chunk it couldn't transcribe gets a `Couldn't hear` line, so quiet and faults look different.
- **`voice`** is a name from `elevenlabs.voices` (or the start of one) or a voice id.
- **`run` is a dry run unless `dry_run: false`.** Do the dry run first and read the request it shows.
  Anything that deletes, removes, disables or clones also needs `confirm: true`. Ask your person first.
  `args` are the words after `elevenlabs` (`["voices", "get", "--voice-id=…"]`). Put `{file}` in args
  where a `file` param goes. A file it writes into its folder (`-o out.mp3`) is posted back.
  `elevenlabs <service> --schema` (through `run`) lists what a service takes.
- **Never paste a key** into a post, an arg or a file. With no key on the machine, ask your person to run
  `/elevenlabs add` in the app. Its form seals the key in the vault.
