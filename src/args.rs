//! What `elevenlabs.run` passes through to the CLI, checked first. Refused outright: anything that would show,
//! send elsewhere or replace the key (`--api-key`, `--xi-api-key`, `--base-url`, `--debug`, which dumps the
//! request), signing in (`auth`), speaking aloud on the machine (`say`), moving the API's region for everyone on
//! it (`residency`), and API keys (`api-keys`: a new one would land in a post). Anything that deletes, removes,
//! disables or clones needs `confirm`, and every path it names must be in the extension's folders.

/// Flags never passed on.
const REFUSED_FLAGS: [(&str, &str); 4] = [
    ("--api-key", "the key comes from the vault, never on the command line"),
    ("--xi-api-key", "the key comes from the vault, never on the command line"),
    ("--base-url", "it would send the key somewhere other than ElevenLabs"),
    ("--debug", "it dumps the request, key and all"),
];

/// Commands never run, by any of their words.
const REFUSED_WORDS: [(&str, &str); 5] = [
    ("auth", "the key comes from the vault for each call: add one with /elevenlabs add"),
    ("say", "it plays audio aloud on this machine: use elevenlabs.tts, which posts the audio"),
    ("residency", "it moves ElevenLabs' region for every call on this machine"),
    ("api-keys", "an API key made or shown here would land in a post: manage them on elevenlabs.io"),
    ("api_keys", "an API key made or shown here would land in a post: manage them on elevenlabs.io"),
];

/// Words that delete, remove, disable or clone: with them, a real run needs `confirm`.
const DESTRUCTIVE: [&str; 6] = ["delete", "remove", "disable", "clone", "ivc", "pvc"];

/// The args, checked: the word that makes it need `confirm`, if one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub destructive: Option<String>,
}

/// The flag of `-x`, `--xyz` or `--xyz=value`.
fn flag(arg: &str) -> Option<&str> {
    arg.starts_with('-').then(|| arg.split_once('=').map_or(arg, |(f, _)| f))
}

pub fn check(args: &[String]) -> Result<Checked, String> {
    if args.iter().all(|a| a.trim().is_empty()) {
        return Err("args is empty: give the words after `elevenlabs`, as [\"voices\", \"search\"]".into());
    }
    for arg in args {
        if let Some(f) = flag(arg) {
            if let Some((name, why)) = REFUSED_FLAGS.iter().find(|(name, _)| *name == f) {
                return Err(format!("{name} isn't passed on: {why}"));
            }
        }
    }
    let words: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).map(|a| a.to_lowercase()).collect();
    if let Some((name, why)) = REFUSED_WORDS.iter().find(|(name, _)| words.iter().any(|w| w == name)) {
        return Err(format!("`{name}` isn't run here: {why}"));
    }
    let destructive = words
        .iter()
        .find(|w| w.split(['-', '_', '.', ' ']).any(|part| DESTRUCTIVE.contains(&part)) || w.contains("delete"))
        .cloned();
    Ok(Checked { destructive })
}

/// The paths args name, for the caller to check against the folders it may use: the file of every `@path` (the
/// CLI uploads that file's contents), and any value starting `/`, `~` or `./`, or with a `..` in it. `\@` is the
/// CLI's literal @. A plain name (`out.mp3`) is the scratch folder's, where the CLI runs.
pub fn paths(args: &[String]) -> Vec<String> {
    args.iter()
        .filter_map(|a| match a.strip_prefix("--") {
            Some(rest) => rest.split_once('=').map(|(_, v)| v),
            None if a.starts_with('-') && a.len() > 1 => None,
            None => Some(a.as_str()),
        })
        .filter_map(|v| {
            let (v, at) = match v.strip_prefix('@') {
                Some(file) => (file, true),
                None => (v, false),
            };
            let pathy = at
                || v.starts_with('/')
                || v.starts_with('~')
                || v.starts_with("./")
                || v.split(['/', '\\']).any(|part| part == "..");
            pathy.then(|| v.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_key_stays_ours() {
        for bad in [
            &["user", "get", "--api-key=sk_x"][..],
            &["user", "get", "--xi-api-key", "x"],
            &["--base-url=http://x", "user", "get"],
            &["user", "get", "--debug"],
        ] {
            assert!(check(&a(bad)).is_err(), "{bad:?}");
        }
        assert!(check(&a(&["auth", "login"])).unwrap_err().contains("/elevenlabs add"));
        assert!(check(&a(&["say", "hello"])).is_err());
        assert!(check(&a(&["service-accounts", "api-keys", "create"])).is_err());
        assert!(check(&a(&[])).is_err());
    }

    #[test]
    fn deleting_and_cloning_are_named() {
        let d = |w: &[&str]| check(&a(w)).unwrap().destructive;
        assert_eq!(d(&["voices", "delete", "abc"]).as_deref(), Some("delete"));
        assert_eq!(d(&["voices", "ivc", "create", "--name=x"]).as_deref(), Some("ivc"));
        assert_eq!(d(&["voices", "pvc", "samples", "create"]).as_deref(), Some("pvc"));
        assert_eq!(d(&["agents", "knowledge-base", "documents", "bulk_delete"]).as_deref(), Some("bulk_delete"));
        assert_eq!(
            d(&["voices", "--format", "json", "delete", "x"]).as_deref(),
            Some("delete"),
            "words after flags count"
        );
        assert_eq!(d(&["voices", "collections", "remove_voice"]).as_deref(), Some("remove_voice"));
        assert_eq!(d(&["voices", "search", "--search=delete me"]), None, "a flag's own value isn't a word");
        assert_eq!(d(&["user", "subscription", "get"]), None);
    }

    #[test]
    fn paths_are_found_for_checking() {
        let p = paths(&a(&[
            "audio-isolation",
            "convert",
            "--audio=@/etc/passwd",
            "-o",
            "/tmp/x.mp3",
            "--text=\\@hi",
            "./a",
            "--json",
            "@~/.ssh/id",
        ]));
        assert_eq!(p, ["/etc/passwd", "/tmp/x.mp3", "./a", "~/.ssh/id"]);
        assert_eq!(paths(&a(&["--audio=@a.wav", "--file", "sub/../../x"])), ["a.wav", "sub/../../x"]);
        assert!(paths(&a(&["text-to-speech", "convert", "--text=a/b", "-o", "out.mp3"])).is_empty());
    }
}
