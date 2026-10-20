//! ElevenLabs' CLI on a machine that hasn't got it: the newest release of github.com/elevenlabs/cli for this
//! platform (`elevenlabs-cli-<target>.tar.gz`), checked against the sha256 ElevenLabs publishes beside it, and
//! only its `elevenlabs` taken out, into the extension's state folder. No Homebrew, no sudo, nothing outside that
//! folder. Linux gets the musl build, static, so it runs on NixOS too.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const RELEASES: &str = "https://api.github.com/repos/elevenlabs/cli/releases/latest";
/// Its archives are about 7 MB; anything much bigger isn't one.
const ARCHIVE_MAX: u64 = 200 * 1024 * 1024;
const BINARY_MAX: u64 = 400 * 1024 * 1024;

/// This platform's build, as ElevenLabs' releases name it: `elevenlabs-cli-<this>.tar.gz`.
pub fn target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// The CLI this extension installed, if it has: `<state>/cli/current/elevenlabs`.
pub fn installed(state: &Path) -> Option<PathBuf> {
    let cli = state.join("cli").join("current").join("elevenlabs");
    cli.is_file().then_some(cli)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).user_agent("pointman-elevenlabs").build()
}

fn bytes(url: &str, max: u64) -> anyhow::Result<Vec<u8>> {
    let r = agent().get(url).call().with_context(|| format!("downloading {url}"))?;
    let mut out = Vec::new();
    r.into_reader().take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        bail!("{url} is bigger than it should be");
    }
    Ok(out)
}

/// The checksum `sums` gives for `name`: sha256sum's lines (`<hex>  <name>`, or `<hex> *<name>` in binary mode),
/// or a lone hash when the file is that archive's own.
pub fn checksum_for(sums: &str, name: &str, own: bool) -> Option<String> {
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let lines: Vec<&str> = sums.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let named = lines.iter().find_map(|l| {
        let (sum, file) = l.split_once(char::is_whitespace)?;
        (hex(sum) && file.trim().trim_start_matches('*') == name).then(|| sum.to_lowercase())
    });
    named.or_else(|| match lines.as_slice() {
        [only] if own && hex(only) => Some(only.to_lowercase()),
        _ => None,
    })
}

/// Install the newest release from `releases` (GitHub's latest-release JSON) into `<state>/cli/<version>`, and
/// point `<state>/cli/current` at it. Returns its path and version.
pub fn install(state: &Path, releases: &str) -> anyhow::Result<(PathBuf, String)> {
    let target = target().context("ElevenLabs' CLI has no build for this platform")?;
    let release: Value =
        agent().get(releases).call().context("asking GitHub for ElevenLabs' newest CLI release")?.into_json()?;
    let tag = release["tag_name"].as_str().context("ElevenLabs' newest CLI release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        bail!("ElevenLabs' newest CLI release has an odd tag: {tag}");
    }
    let assets = release["assets"].as_array().cloned().unwrap_or_default();
    let url_of = |name: &str| {
        assets.iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str().map(String::from))
    };
    let name = format!("elevenlabs-cli-{target}.tar.gz");
    let archive_url = url_of(&name).with_context(|| format!("ElevenLabs' CLI {version} has no {name}"))?;
    // its own .sha256, else the release's sha256.sum
    let (sums_url, own) = match (url_of(&format!("{name}.sha256")), url_of("sha256.sum")) {
        (Some(url), _) => (url, true),
        (None, Some(url)) => (url, false),
        (None, None) => bail!("ElevenLabs' CLI {version} publishes no sha256 for {name}: not installed"),
    };
    let sums = String::from_utf8(bytes(&sums_url, 1024 * 1024)?).context("its sha256 isn't text")?;
    let want = checksum_for(&sums, &name, own).with_context(|| format!("its sha256 file doesn't list {name}"))?;
    let archive = bytes(&archive_url, ARCHIVE_MAX)?;
    let got = format!("{:x}", Sha256::digest(&archive));
    if got != want {
        bail!("{name} ({version}) doesn't match its published sha256: not installed");
    }
    let binary = unpack(&archive, target).with_context(|| format!("unpacking {name}"))?;

    let cli = state.join("cli");
    let to = cli.join(&version);
    fs::create_dir_all(&to)?;
    let part = to.join(".elevenlabs.part");
    fs::write(&part, &binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&part, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&part, to.join("elevenlabs"))?;
    let current = cli.join("current");
    let link = cli.join(".current.new");
    let _ = fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&version, &link)?;
    fs::rename(&link, &current)?;
    Ok((current.join("elevenlabs"), version))
}

/// The archive's `elevenlabs-cli-<target>/elevenlabs`, and nothing else of it.
fn unpack(archive: &[u8], target: &str) -> anyhow::Result<Vec<u8>> {
    let want = Path::new(&format!("elevenlabs-cli-{target}")).join("elevenlabs");
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && *entry.path()? == *want {
            if entry.size() > BINARY_MAX {
                bail!("its elevenlabs is bigger than it should be");
            }
            let mut out = Vec::new();
            entry.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    bail!("it has no {}", want.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        let sums = "abc  x.tar.gz\ncb8f7959d8708891ab9789004ddac402a3cda4bacae7c46adc5d00e85887d4ae *elevenlabs-cli-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "elevenlabs-cli-aarch64-apple-darwin.tar.gz", false).as_deref(),
            Some("cb8f7959d8708891ab9789004ddac402a3cda4bacae7c46adc5d00e85887d4ae")
        );
        assert_eq!(checksum_for(sums, "elevenlabs-cli-x86_64-apple-darwin.tar.gz", false), None);
        let lone = "CB8F7959D8708891AB9789004DDAC402A3CDA4BACAE7C46ADC5D00E85887D4AE\n";
        assert!(checksum_for(lone, "a.tar.gz", true).is_some());
        assert_eq!(checksum_for(lone, "a.tar.gz", false), None, "a lone hash is only its own file's");
    }

    #[test]
    fn every_platform_has_its_build() {
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("linux", "aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_for("windows", "x86_64"), None);
        assert!(target().is_some());
    }
}

#[cfg(test)]
mod real {
    /// The real download from ElevenLabs' releases: `cargo test -- --ignored` (online).
    #[test]
    #[ignore]
    fn installs_the_newest_cli() {
        let dir = tempfile::tempdir().unwrap();
        let (cli, version) = super::install(dir.path(), super::RELEASES).unwrap();
        let out = std::process::Command::new(&cli).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(&version), "{out:?}");
        assert_eq!(super::installed(dir.path()), Some(cli));
    }
}
