//! The About tab: whether a newer Picard is out, and the links it opens.
//!
//! The window has no network or opener access, so both happen here. The check
//! runs `/usr/bin/curl` rather than pulling an HTTP and TLS stack into the
//! app, and only when the About tab asks for it. Links are named, not passed
//! as URLs, so the window can open these pages and nothing else.

use std::process::Command;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/velvet-tiger/picard/releases/latest";
const CURL_TIMEOUT: Duration = Duration::from_secs(20);
/// Curl's own limit, kept under `CURL_TIMEOUT` so curl reports the timeout.
const CURL_MAX_TIME_SECS: &str = "15";
/// A release body with a few assets is a few KB; anything near this is wrong.
const RESPONSE_CAP: usize = 1024 * 1024;
/// Separates the body from the status code that `--write-out` appends.
const STATUS_MARKER: &str = "\nPICARD-HTTP-STATUS:";

/// A page the About tab may open. Must match `Link` in src/lib/api.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Website,
    Issues,
    LatestRelease,
}

impl Link {
    pub fn url(self) -> &'static str {
        match self {
            Link::Website => "https://velvettiger.com.au",
            Link::Issues => "https://github.com/velvet-tiger/picard/issues",
            Link::LatestRelease => "https://github.com/velvet-tiger/picard/releases/latest",
        }
    }
}

/// Opens `link` in the user's default browser.
pub fn open(link: Link) -> Result<(), String> {
    let url = link.url();
    Command::new("/usr/bin/open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open {url}: {e}"))
}

/// The running version beside the latest published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: String,
    /// True when `latest` is newer than `current`.
    pub newer: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
}

/// Asks GitHub for the latest release and compares it with `current`.
/// Blocks for up to `CURL_TIMEOUT`, so call it off the async runtime.
pub fn check(current: &Version) -> Result<UpdateCheck, String> {
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--location",
        "--max-time",
        CURL_MAX_TIME_SECS,
        "--header",
        "Accept: application/vnd.github+json",
        "--user-agent",
        &format!("Picard/{current}"),
        "--write-out",
        &format!("{STATUS_MARKER}%{{http_code}}"),
        LATEST_RELEASE_API,
    ]);
    let out = crate::proc::run(cmd, CURL_TIMEOUT, RESPONSE_CAP)
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.success() {
        log::warn!("update check failed: {}", out.failure_summary(CURL_TIMEOUT));
        return Err(curl_failure(&out.stderr, out.timed_out));
    }
    if out.truncated {
        return Err("GitHub's reply was too large to read".to_string());
    }
    let (status, body) = split_status(&out.stdout)?;
    interpret(status, body, current)
}

fn curl_failure(stderr: &str, timed_out: bool) -> String {
    if timed_out {
        return "GitHub did not answer in time".to_string();
    }
    let detail = stderr.trim().trim_start_matches("curl: ");
    if detail.is_empty() {
        "the request to GitHub failed".to_string()
    } else {
        format!("the request to GitHub failed ({detail})")
    }
}

/// Splits curl's output into the HTTP status and the body before it.
fn split_status(stdout: &str) -> Result<(u16, &str), String> {
    let (body, status) = stdout
        .rsplit_once(STATUS_MARKER)
        .ok_or("curl's output had no HTTP status")?;
    let status = status
        .trim()
        .parse()
        .map_err(|_| format!("curl reported an unreadable HTTP status {status:?}"))?;
    Ok((status, body))
}

/// Turns GitHub's reply into an `UpdateCheck`, or a message the tab can show.
fn interpret(status: u16, body: &str, current: &Version) -> Result<UpdateCheck, String> {
    match status {
        200 => {}
        // GitHub answers 403 or 429 once an address uses up its hourly limit.
        403 | 429 => return Err("GitHub's rate limit was reached. Try again later.".into()),
        404 => return Err("GitHub has no published release of Picard".into()),
        other => return Err(format!("GitHub answered with HTTP {other}")),
    }
    let release: Release =
        serde_json::from_str(body).map_err(|e| format!("could not read GitHub's reply: {e}"))?;
    let tag = release.tag_name.as_str();
    let latest = Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .map_err(|e| format!("the latest release tag {tag:?} is not a version: {e}"))?;
    Ok(UpdateCheck {
        current: current.to_string(),
        latest: latest.to_string(),
        newer: latest > *current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn reply(tag: &str) -> String {
        format!(r#"{{"tag_name":"{tag}","html_url":"https://example.invalid","assets":[]}}"#)
    }

    #[test]
    fn a_newer_release_is_reported() {
        let check = interpret(200, &reply("v0.2.0"), &v("0.1.0")).unwrap();
        assert_eq!(
            check,
            UpdateCheck {
                current: "0.1.0".into(),
                latest: "0.2.0".into(),
                newer: true,
            }
        );
    }

    #[test]
    fn the_same_or_an_older_release_is_not_newer() {
        assert!(!interpret(200, &reply("v0.1.0"), &v("0.1.0")).unwrap().newer);
        assert!(!interpret(200, &reply("v0.1.0"), &v("0.2.0")).unwrap().newer);
    }

    #[test]
    fn a_pre_release_build_sees_its_final_release_as_newer() {
        assert!(
            interpret(200, &reply("v1.0.0"), &v("1.0.0-rc.1"))
                .unwrap()
                .newer
        );
    }

    #[test]
    fn a_tag_without_the_v_prefix_is_read() {
        assert_eq!(
            interpret(200, &reply("0.3.1"), &v("0.1.0")).unwrap().latest,
            "0.3.1"
        );
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_an_error() {
        let err = interpret(200, &reply("nightly"), &v("0.1.0")).unwrap_err();
        assert!(err.contains("\"nightly\" is not a version"), "{err}");
    }

    #[test]
    fn http_errors_become_readable_messages() {
        let current = v("0.1.0");
        assert!(
            interpret(403, "{}", &current)
                .unwrap_err()
                .contains("rate limit")
        );
        assert!(
            interpret(429, "{}", &current)
                .unwrap_err()
                .contains("rate limit")
        );
        assert!(
            interpret(404, "{}", &current)
                .unwrap_err()
                .contains("no published release")
        );
        assert_eq!(
            interpret(502, "", &current).unwrap_err(),
            "GitHub answered with HTTP 502"
        );
    }

    #[test]
    fn a_body_that_is_not_a_release_is_an_error() {
        let err = interpret(200, "<html>", &v("0.1.0")).unwrap_err();
        assert!(err.starts_with("could not read GitHub's reply"), "{err}");
    }

    #[test]
    fn the_status_is_split_from_the_end_of_the_output() {
        let out = format!("{}{STATUS_MARKER}200", reply("v0.1.0"));
        assert_eq!(split_status(&out).unwrap(), (200, reply("v0.1.0").as_str()));
        assert!(split_status("no marker").is_err());
        assert!(split_status(&format!("{{}}{STATUS_MARKER}abc")).is_err());
    }

    #[test]
    fn curl_failures_name_the_cause() {
        assert_eq!(curl_failure("", true), "GitHub did not answer in time");
        assert_eq!(
            curl_failure("curl: (6) Could not resolve host: api.github.com\n", false),
            "the request to GitHub failed ((6) Could not resolve host: api.github.com)"
        );
        assert_eq!(curl_failure("  ", false), "the request to GitHub failed");
    }

    #[test]
    fn links_deserialize_from_the_names_the_window_sends() {
        let link: Link = serde_json::from_str("\"latest_release\"").unwrap();
        assert_eq!(link, Link::LatestRelease);
        assert_eq!(Link::Website.url(), "https://velvettiger.com.au");
        assert!(serde_json::from_str::<Link>("\"https://evil.example\"").is_err());
    }
}
