//! npx keeps Boubou where no `boubou` command reaches it, and npm goes on running an old copy.
//! So a copy that npx started writes the commands it suggests as npx commands, and a copy from
//! npm shows when a newer version is available during startup.

use crate::tui;
use reqwest::blocking::Client;
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::Path,
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// How npm put this copy here, as its launcher found: `npx` for one npx fetched, `global` for
/// `npm install --global`. `None` for a binary run directly, as a source build is.
fn npm() -> Option<String> {
    std::env::var("BOUBOU_NPM")
        .ok()
        .filter(|npm| npm == "npx" || npm == "global")
}

/// The command that brings the version under `tag` to a copy npm put here in the `npm` way.
fn update(npm: &str, tag: &str) -> String {
    match npm {
        "npx" => format!("npx boubou@{tag}"),
        _ => format!("npm install --global boubou@{tag}"),
    }
}

/// The npm tag a version is published under: `beta` for 0.1.0-beta.9, `latest` for a release.
fn channel(version: &str) -> &str {
    version
        .split_once('-')
        .and_then(|(_, pre)| pre.split('.').next())
        .unwrap_or("latest")
}

/// What starts Boubou from a shell: `boubou`, or the npx command for a copy npx started.
pub fn program() -> &'static str {
    static PROGRAM: OnceLock<String> = OnceLock::new();
    PROGRAM.get_or_init(|| match npm().as_deref() {
        Some("npx") => update("npx", channel(VERSION)),
        _ => "boubou".into(),
    })
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Part<'a> {
    Number(u64),
    Word(&'a str),
}

/// A version's place in semver's order: its three numbers, a release above its prereleases,
/// then the prerelease's parts with numbers below words. `None` for text that is not a version,
/// so whatever passes is also safe to print.
fn order(version: &str) -> Option<([u64; 3], bool, Vec<Part<'_>>)> {
    let (numbers, pre) = match version.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (version, None),
    };
    let numbers: Vec<u64> = numbers
        .split('.')
        .map(|number| number.parse().ok())
        .collect::<Option<_>>()?;
    let parts = pre
        .into_iter()
        .flat_map(|pre| pre.split('.'))
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                .then(|| part.parse().map_or(Part::Word(part), Part::Number))
        })
        .collect::<Option<_>>()?;
    Some((numbers.try_into().ok()?, pre.is_none(), parts))
}

/// The line to print when `tags`, a registry's answer, holds a version newer than `running`.
fn newer(tags: &str, running: &str, npm: &str) -> Option<String> {
    let tags: HashMap<String, String> = serde_json::from_str(tags).ok()?;
    // Between equal versions the later tag wins: this copy's own channel, as its hints name it.
    let (tag, version) = ["latest", channel(running)]
        .into_iter()
        .filter_map(|tag| Some((tag, tags.get(tag)?)))
        .max_by_key(|(_, version)| order(version))?;
    (order(version) > order(running)).then(|| {
        format!(
            "Boubou {version} is out (this is {running}). Run: {}",
            update(npm, tag)
        )
    })
}

/// Asks `registry` for Boubou's tags and keeps what reads as an answer in `file`. The request
/// names the package and nothing about this copy or computer.
fn ask(client: &Client, registry: &str, file: &Path) -> Option<String> {
    let tags = client
        .get(format!(
            "{}/-/package/boubou/dist-tags",
            registry.trim_end_matches('/')
        ))
        .send()
        .and_then(|response| response.error_for_status()?.text())
        .ok()?;
    // A Wi-Fi sign-in page is not an answer.
    serde_json::from_str::<HashMap<String, String>>(&tags).ok()?;
    let _ = fs::create_dir_all(file.parent()?).and_then(|()| fs::write(file, &tags));
    Some(tags)
}

fn remember(tags: &str, npm: &str) {
    *NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = newer(tags, VERSION, npm);
}

/// The latest available upgrade notice, for startup screens that redraw while checking.
pub fn message() -> Option<String> {
    NOTICE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Hands a late result to the active screen once, without writing from the network thread.
pub fn take_notice() -> Option<String> {
    NOTICE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Looks for a newer version without holding anything up; startup screens read [`message`].
///
/// Only a copy from npm looks, as only its update command is known. In the background and at
/// most once a day it asks the registry npm uses, npm's own unless npm names another, and keeps
/// the answer in the cache for the starts in between. It stays silent in CI, away from a
/// terminal and when `BOUBOU_NO_UPDATE_CHECK` is set.
pub fn check(cache: &Path) {
    let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let Some(npm) = npm() else { return };
    if set("BOUBOU_NO_UPDATE_CHECK") || set("CI") || !tui::interactive(&io::stderr()) {
        return;
    }
    let file = cache.join("npm-tags.json");
    // The last answer stands until the registry gives another.
    remember(&fs::read_to_string(&file).unwrap_or_default(), &npm);
    // The file's age is the time since the registry answered.
    if file
        .metadata()
        .and_then(|file| file.modified())
        .is_ok_and(|answered| answered.elapsed().is_ok_and(|age| age < DAY))
    {
        return;
    }
    let registry = std::env::var("npm_config_registry")
        .unwrap_or_else(|_| "https://registry.npmjs.org".into());
    let _ = thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let client = Client::builder().timeout(Duration::from_secs(3)).build();
            if let Some(tags) = client
                .ok()
                .and_then(|client| ask(&client, &registry, &file))
            {
                remember(&tags, &npm);
            }
        });
}

/// Prints the line about a newer version, once the command is over and the screen is free.
pub fn notice() {
    if let Some(line) = take_notice() {
        // The terminal may already be gone; printing must not panic on the way out.
        let _ = writeln!(io::stderr(), "\n{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn hints_name_the_channel_this_version_is_published_under() {
        assert_eq!(channel("0.1.0-beta.9"), "beta");
        assert_eq!(channel("0.2.0-rc.1"), "rc");
        assert_eq!(channel("1.0.0"), "latest");
        // Tests run the binary's code directly, as a source build does.
        assert_eq!(program(), "boubou");
    }

    #[test]
    fn versions_compare_as_semver_orders_them() {
        let ascending = [
            "0.1.0-beta.2",
            "0.1.0-beta.9",
            "0.1.0-beta.10",
            "0.1.0-beta.10.1",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1-beta.1",
            "0.2.0",
            "1.0.0",
        ];
        for pair in ascending.windows(2) {
            assert!(order(pair[0]) < order(pair[1]), "{pair:?}");
            assert!(order(pair[0]).is_some());
        }
        for text in [
            "",
            "1.0",
            "1.0.0.0",
            "v1.0.0",
            "1.0.0-",
            "1.0.0-beta..1",
            "1.0.0-\x1b[2J",
            "1.0.0\n",
        ] {
            assert!(order(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn a_newer_version_is_announced_with_the_command_that_fetches_it() {
        // Until a stable version exists, a beta is both tags: the line names the copy's own.
        let tags = r#"{"latest":"0.1.0-beta.10","beta":"0.1.0-beta.10"}"#;
        assert_eq!(
            newer(tags, "0.1.0-beta.9", "npx").unwrap(),
            "Boubou 0.1.0-beta.10 is out (this is 0.1.0-beta.9). Run: npx boubou@beta"
        );
        assert_eq!(
            newer(tags, "0.1.0-beta.3", "global").unwrap(),
            "Boubou 0.1.0-beta.10 is out (this is 0.1.0-beta.3). Run: npm install --global boubou@beta"
        );
        // The first stable version reaches people on the last beta.
        assert_eq!(
            newer(
                r#"{"latest":"0.1.0","beta":"0.1.0-beta.12"}"#,
                "0.1.0-beta.12",
                "npx"
            )
            .unwrap(),
            "Boubou 0.1.0 is out (this is 0.1.0-beta.12). Run: npx boubou@latest"
        );
        // A later beta reaches people on a beta, not people on the stable version.
        let tags = r#"{"latest":"0.1.0","beta":"0.2.0-beta.2"}"#;
        assert!(
            newer(tags, "0.2.0-beta.1", "npx")
                .unwrap()
                .ends_with("npx boubou@beta")
        );
        assert_eq!(newer(tags, "0.1.0", "npx"), None);
    }

    /// A registry that answers one request with `body` and hands back what it was sent.
    fn registry(status: &str, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/", listener.local_addr().unwrap());
        let head = format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        let served = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            stream
                .write_all(format!("{head}{body}").as_bytes())
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        (address, served)
    }

    #[test]
    fn the_registry_is_asked_for_the_tags_alone_and_its_answer_is_kept() {
        let dir = std::env::temp_dir().join(format!("boubou-update-{}", std::process::id()));
        let file = dir.join("cache/npm-tags.json");
        let client = Client::builder().no_proxy().build().unwrap();
        let tags = r#"{"latest":"99.0.0","beta":"99.0.0-beta.1"}"#;
        let (address, served) = registry("200 OK", tags);
        assert_eq!(ask(&client, &address, &file).as_deref(), Some(tags));
        assert_eq!(fs::read_to_string(&file).unwrap(), tags);
        let request = served.join().unwrap().to_lowercase();
        assert!(
            request.starts_with("get /-/package/boubou/dist-tags http/1.1\r\n"),
            "{request}"
        );
        // Nothing about this copy travels: no version, no user agent, no cookie.
        let headers: Vec<_> = request
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, _)| name)
            .collect();
        assert_eq!(headers, ["accept", "host"], "{request}");
        assert!(!request.contains(&VERSION.to_lowercase()), "{request}");
        // An answer that is not tags, a refusal and a registry that is not there leave the last answer alone.
        let (address, served) = registry("200 OK", "<html>Sign in to this network</html>");
        assert_eq!(ask(&client, &address, &file), None);
        served.join().unwrap();
        let (address, served) = registry("503 Service Unavailable", tags);
        assert_eq!(ask(&client, &address, &file), None);
        served.join().unwrap();
        assert_eq!(ask(&client, &address, &file), None);
        assert_eq!(fs::read_to_string(&file).unwrap(), tags);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_is_announced_for_the_same_an_older_or_an_unreadable_answer() {
        for tags in [
            r#"{"latest":"0.1.0-beta.9","beta":"0.1.0-beta.9"}"#,
            r#"{"latest":"0.1.0-beta.8","beta":"0.1.0-beta.8"}"#,
            r#"{"latest":"9.0.0-\u001b[2J"}"#,
            r#"{"latest":9}"#,
            r#"{"next":"9.0.0"}"#,
            "<html>Sign in to this network</html>",
            "",
        ] {
            assert_eq!(newer(tags, "0.1.0-beta.9", "npx"), None, "{tags}");
        }
    }
}
