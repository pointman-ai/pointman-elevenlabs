//! extension.toml within what core's manifest check takes (Pointman's src/messageboard/extensions/
//! manifest.py): core refuses a release whose manifest breaks one.

#[test]
fn the_manifest_keeps_to_cores_limits() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/extension.toml")).unwrap();
    let m: toml::Table = toml::from_str(&text).unwrap();
    let chars = |v: &toml::Value| v.as_str().unwrap().chars().count();
    let ext = &m["extension"];
    assert_eq!(ext["version"].as_str(), Some(env!("CARGO_PKG_VERSION")), "extension.toml's version is Cargo's");
    assert!((1..=60).contains(&chars(&ext["name"])) && (1..=200).contains(&chars(&ext["summary"])));
    let forms: Vec<&str> = m["credentials"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    for c in m["commands"].as_array().unwrap() {
        let kind = c["kind"].as_str().unwrap();
        assert!(kind.starts_with("elevenlabs."), "{kind}");
        assert!((1..=60).contains(&chars(&c["title"])), "{kind}'s title is {} characters", chars(&c["title"]));
        assert!(chars(&c["summary"]) <= 120, "{kind}'s summary is {} characters", chars(&c["summary"]));
        let slash = c["slash"].as_str().unwrap();
        let ok = slash.len() <= 24
            && slash.starts_with(|ch: char| ch.is_ascii_lowercase())
            && slash.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        assert!(ok, "{kind}'s slash {slash}");
        assert_eq!(c["params"]["type"].as_str(), Some("object"), "{kind}");
        if let Some(form) = c.get("form") {
            assert!(forms.contains(&form.as_str().unwrap()), "{kind}'s form");
        }
    }
    let secrets = m["permissions"]["secrets"].as_array().unwrap();
    assert_eq!(secrets[0].as_str(), Some("elevenlabs.*"), "the keys' vault items are the extension's own");
    let skill = m["behavior"]["skill"].as_str().unwrap();
    let guidance = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/", "SKILLS.md")).unwrap();
    assert_eq!(skill, "SKILLS.md");
    assert!(guidance.starts_with("---\n"), "the skill's frontmatter first");
}

/// The release packs the skill beside the binary, so a node has the guidance from the build alone.
#[test]
fn the_release_packs_the_skill() {
    let release =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/release.yml")).unwrap();
    assert!(release.contains("cp extension.toml SKILLS.md README.md LICENSE pkg/"), "{release}");
    assert!(release.contains("-C pkg extension.toml SKILLS.md bin README.md LICENSE"), "{release}");
}
