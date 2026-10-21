//! The links the About tab opens.
//!
//! The window has no opener access, so opening happens here. Links are named,
//! not passed as URLs, so the window can open these pages and nothing else.
//! Update checks live in `updater`.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// `open` prints nothing on success and a line or two on failure.
const OPEN_OUTPUT_CAP: usize = 4 * 1024;

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

/// Opens `link` in the user's default browser. `open` hands the URL to
/// Launch Services and exits at once. Waiting for it reaps the process and
/// reports a failure, so call it off the main thread. The time limit covers
/// a stalled Launch Services.
pub fn open(link: Link) -> Result<(), String> {
    let url = link.url();
    let mut cmd = Command::new("/usr/bin/open");
    cmd.arg(url);
    let out = crate::proc::run(cmd, OPEN_TIMEOUT, OPEN_OUTPUT_CAP)
        .map_err(|e| format!("could not open {url}: {e}"))?;
    if out.success() {
        Ok(())
    } else {
        Err(format!(
            "could not open {url}: {}",
            out.failure_summary(OPEN_TIMEOUT)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_deserialize_from_the_names_the_window_sends() {
        let link: Link = serde_json::from_str("\"latest_release\"").unwrap();
        assert_eq!(link, Link::LatestRelease);
        assert_eq!(Link::Website.url(), "https://velvettiger.com.au");
        assert!(serde_json::from_str::<Link>("\"https://evil.example\"").is_err());
    }
}
