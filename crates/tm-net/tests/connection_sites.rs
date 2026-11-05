//! Every connection the miner makes goes through `tm-net`.
//!
//! The scan reads the production source of every other workspace crate (its
//! `src` tree, without `#[cfg(test)]` items) and fails when it opens a socket,
//! builds an HTTP client, or resolves a name itself. `examples/` and `tests/`
//! are not production code and are not scanned.

use std::fs;
use std::path::{Path, PathBuf};

/// Ways to reach the network that bypass `NetworkPolicy`.
const FORBIDDEN: &[&str] = &[
    "reqwest::Client::new",
    "reqwest::Client::builder",
    "Client::builder()",
    "TcpStream::connect",
    "connect_async",
    "lookup_host",
];

/// Constructors that fall back to a direct connection, which the application
/// must never call: it builds everything from the configured policy.
const DIRECT_CONSTRUCTORS: &[&str] = &[
    "TwitchClient::new(",
    "TwitchClient::with_client",
    "TwitchAuthClient::new(",
    "DiscordClient::new(",
    "PubSubClient::default(",
    "PubSubClientSettings::default(",
    "NetworkPolicy::direct(",
];

/// The application crate, whose wiring must use the policy-taking constructors.
const APPLICATION_CRATE: &str = "tm-app";

/// The only places allowed to match [`FORBIDDEN`]: branches that exist only
/// when no proxy is configured and cannot move into `tm-net`.
const ALLOWLIST: &[(&str, &str, &str)] = &[(
    "tm-twitch/src/client.rs",
    "lookup_host",
    "the no-proxy playback resolver validates resolved addresses; it is not installed when a proxy is configured",
)];

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    file: String,
    line: usize,
    token: &'static str,
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Index of the last line of the item or statement that begins at `start`.
/// The code is rustfmt-formatted, so a multi-line item closes on a line at its
/// own indentation.
fn item_end(lines: &[&str], start: usize) -> usize {
    let first = lines[start].trim_end();
    if first.ends_with(';') || first.ends_with(',') {
        return start;
    }
    let item_indent = indent(lines[start]);
    (start + 1..lines.len())
        .find(|&index| {
            indent(lines[index]) == item_indent
                && lines[index].trim_start().starts_with(['}', ')', ']'])
        })
        .unwrap_or(lines.len() - 1)
}

/// The source lines that are not test-only code or comments, with their
/// 1-based line numbers.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut kept = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() == "#[cfg(test)]" {
            let mut item = index + 1;
            while item < lines.len() && lines[item].trim_start().starts_with("#[") {
                item += 1;
            }
            if item >= lines.len() {
                break;
            }
            index = item_end(&lines, item) + 1;
            continue;
        }
        if !lines[index].trim_start().starts_with("//") {
            kept.push((index + 1, lines[index]));
        }
        index += 1;
    }
    kept
}

fn contains_token(line: &str, token: &str) -> bool {
    line.match_indices(token).any(|(at, _)| {
        line[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !(before.is_alphanumeric() || before == '_'))
    })
}

fn scan(source: &str, file: &str, tokens: &[&'static str]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line, text) in production_lines(source) {
        for token in tokens {
            if contains_token(text, token) {
                findings.push(Finding {
                    file: file.to_string(),
                    line,
                    token,
                });
            }
        }
    }
    findings
}

fn production_sources() -> Vec<(String, String, String)> {
    let mut sources = Vec::new();
    let mut crate_dirs = fs::read_dir(crates_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    crate_dirs.sort();
    for crate_dir in crate_dirs {
        let name = crate_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if name == "tm-net" || !crate_dir.join("src").is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&crate_dir.join("src"), &mut files);
        for path in files {
            let relative = path
                .strip_prefix(&crate_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            sources.push((
                name.clone(),
                format!("{name}/{relative}"),
                fs::read_to_string(&path).unwrap(),
            ));
        }
    }
    sources
}

#[test]
fn only_tm_net_opens_connections() {
    let sources = production_sources();
    assert!(
        sources.iter().any(|(name, _, _)| name == "tm-twitch"),
        "the scan found no workspace sources"
    );
    let mut violations = Vec::new();
    let mut used = vec![false; ALLOWLIST.len()];
    for (_, file, source) in &sources {
        for finding in scan(source, file, FORBIDDEN) {
            let allowed = ALLOWLIST
                .iter()
                .position(|(path, token, _)| *path == finding.file && *token == finding.token);
            match allowed {
                Some(position) => used[position] = true,
                None => violations.push(finding),
            }
        }
    }
    assert!(
        violations.is_empty(),
        "connections must go through tm_net::NetworkPolicy: {violations:#?}"
    );
    for (entry, used) in ALLOWLIST.iter().zip(used) {
        assert!(used, "stale allowlist entry: {entry:?}");
    }
}

#[test]
fn the_application_never_calls_a_constructor_that_connects_directly() {
    let violations = production_sources()
        .iter()
        .filter(|(name, _, _)| name == APPLICATION_CRATE)
        .flat_map(|(_, file, source)| scan(source, file, DIRECT_CONSTRUCTORS))
        .collect::<Vec<_>>();
    assert!(
        violations.is_empty(),
        "build clients from the NetworkPolicy built at startup: {violations:#?}"
    );
}

#[test]
fn the_allowlist_entries_explain_themselves() {
    for (file, token, reason) in ALLOWLIST {
        assert!(FORBIDDEN.contains(token), "{file}: {token}");
        assert!(reason.len() > 20, "{file}: {token} needs a reason");
    }
}

#[test]
fn the_scan_drops_test_only_code_and_keeps_the_rest() {
    let source = "\
use tokio::net::TcpStream;
#[cfg(test)]
use reqwest::Client as TestClient;
fn connect() {
    // TcpStream::connect in a comment
    #[cfg(test)]
    if cfg!(debug_assertions) {
        let _ = TcpStream::connect(\"a\");
    }
    let _ = TcpStream::connect(\"b\");
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    fn helper() {
        let _ = reqwest::Client::new();
    }
}
fn after_tests() {
    let _ = connect_async(\"c\");
}
";
    let findings = scan(source, "sample.rs", FORBIDDEN);
    assert_eq!(
        findings,
        vec![
            Finding {
                file: String::from("sample.rs"),
                line: 10,
                token: "TcpStream::connect",
            },
            Finding {
                file: String::from("sample.rs"),
                line: 20,
                token: "connect_async",
            },
        ]
    );
}

#[test]
fn tokens_match_only_at_a_word_boundary() {
    assert!(contains_token(
        "let a = Client::builder();",
        "Client::builder()"
    ));
    assert!(contains_token(
        "reqwest::Client::builder()",
        "Client::builder()"
    ));
    assert!(!contains_token(
        "let a = MyClient::builder();",
        "Client::builder()"
    ));
    assert!(contains_token(
        "connect_async_with_config(x)",
        "connect_async"
    ));
    assert!(contains_token(
        "DiscordClient::new(t)",
        "DiscordClient::new("
    ));
    assert!(!contains_token(
        "MyDiscordClient::new(t)",
        "DiscordClient::new("
    ));
}
