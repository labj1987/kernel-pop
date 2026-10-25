//! kernelorg.rs — Talk to kernel.org: list the current releases from
//! releases.json (mainline, stable, longterm) and work out where a
//! release's tarball and signature live on cdn.kernel.org.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const RELEASES_URL: &str = "https://www.kernel.org/releases.json";
pub const CDN_BASE: &str = "https://cdn.kernel.org/pub/linux/kernel/";

/// Which kernel.org series a release belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Moniker {
    Mainline,
    Stable,
    Longterm,
}

impl Moniker {
    pub fn label(self) -> &'static str {
        match self {
            Moniker::Mainline => "mainline",
            Moniker::Stable => "stable",
            Moniker::Longterm => "longterm",
        }
    }
}

/// One release listed in releases.json.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub moniker: Moniker,
    /// As kernel.org names it: "7.3", "7.2.12", "7.4-rc1"
    pub version: String,
    pub is_eol: bool,
    /// "2026-10-21"
    pub released: String,
    /// The tarball (.tar.xz on cdn.kernel.org; a release candidate's is a
    /// .tar.gz from git.kernel.org).
    pub source: String,
    /// Its detached signature (.tar.sign). Release candidates have none.
    pub pgp: Option<String>,
}

#[derive(Deserialize)]
struct ReleasesJson {
    releases: Vec<ReleaseJson>,
}

#[derive(Deserialize)]
struct ReleaseJson {
    moniker: String,
    version: String,
    #[serde(default)]
    iseol: bool,
    released: Option<ReleasedJson>,
    source: Option<String>,
    pgp: Option<String>,
}

#[derive(Deserialize)]
struct ReleasedJson {
    isodate: Option<String>,
}

/// Parse releases.json. Entries that are not mainline, stable or longterm
/// (linux-next) or that have no tarball are left out; the file's order
/// (mainline, then stable, then longterm newest first) is kept.
pub fn parse_releases(json: &str) -> Result<Vec<Release>> {
    let parsed: ReleasesJson = serde_json::from_str(json).context("releases.json is not in the expected format")?;
    let releases: Vec<Release> = parsed
        .releases
        .into_iter()
        .filter_map(|r| {
            let moniker = match r.moniker.as_str() {
                "mainline" => Moniker::Mainline,
                "stable" => Moniker::Stable,
                "longterm" => Moniker::Longterm,
                _ => return None,
            };
            let source = r.source.filter(|s| s.starts_with("https://"))?;
            Some(Release {
                moniker,
                version: r.version,
                is_eol: r.iseol,
                released: r.released.and_then(|d| d.isodate).unwrap_or_default(),
                source,
                pgp: r.pgp.filter(|s| s.starts_with("https://")),
            })
        })
        .collect();
    if releases.is_empty() {
        bail!("releases.json lists no kernel releases");
    }
    Ok(releases)
}

pub fn client() -> Result<reqwest::Client> {
    // connect_timeout + read_timeout instead of a total request timeout:
    // a total timeout would kill a slow-but-healthy download; read_timeout
    // only fires when the stream actually stalls.
    Ok(reqwest::Client::builder()
        .user_agent(concat!("kernel-pop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// Fetch and parse releases.json.
pub async fn fetch_releases() -> Result<Vec<Release>> {
    let json = client()?
        .get(RELEASES_URL)
        .send()
        .await
        .context("Failed to reach www.kernel.org")?
        .error_for_status()
        .context("www.kernel.org did not return releases.json")?
        .text()
        .await?;
    parse_releases(&json)
}

/// The cdn.kernel.org URL of the signature for a tarball of kernel.org
/// release `name` ("7.2.12", "7.3"): pub/linux/kernel/v7.x/linux-7.2.12.tar.sign.
/// One signature covers the .tar.xz and the .tar.gz alike, since it is made
/// over the uncompressed tar.
pub fn signature_url(name: &str, major: u32) -> String {
    format!("{CDN_BASE}v{major}.x/linux-{name}.tar.sign")
}

/// Fetch kernel.org's `.tar.sign` for a source tree of `version` (as read
/// from its Makefile) into `dest_dir`; returns its path.
pub async fn fetch_signature(version: &crate::source::SourceVersion, dest_dir: std::path::PathBuf) -> Result<std::path::PathBuf> {
    let name = version.release_name();
    let url = signature_url(&name, version.version);
    let file = format!("linux-{name}.tar.sign");
    let mut paths = crate::download::download_files(
        vec![crate::download::Remote { url: url.clone(), filename: file }],
        dest_dir,
        |_| {},
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
    .with_context(|| format!("No signature could be fetched for Linux {name} from {url}"))?;
    Ok(paths.remove(0))
}

/// The last path segment of a URL.
pub fn url_file_name(url: &str) -> Option<&str> {
    let name = url.rsplit('/').next()?;
    (!name.is_empty() && name != "." && name != "..").then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES: &str = r#"{"latest_stable":{"version":"7.3"},"releases":[
{"iseol":false,"version":"7.3","moniker":"mainline","source":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.tar.xz","pgp":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.tar.sign","released":{"timestamp":1792365298,"isodate":"2026-10-18"},"gitweb":"https://git.kernel.org/torvalds/h/v7.3","changelog":null,"diffview":"https://git.kernel.org/torvalds/ds/v7.3/v7.2","patch":{"full":"https://cdn.kernel.org/pub/linux/kernel/v7.x/patch-7.3.xz","incremental":null}},
{"iseol":false,"version":"7.2.12","moniker":"stable","source":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.12.tar.xz","pgp":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.12.tar.sign","released":{"timestamp":1792600000,"isodate":"2026-10-21"}},
{"iseol":true,"version":"6.6.160","moniker":"longterm","source":"https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.6.160.tar.xz","pgp":"https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.6.160.tar.sign","released":{"timestamp":1792600000,"isodate":"2026-10-21"}},
{"iseol":false,"version":"7.4-rc1","moniker":"mainline","source":"https://git.kernel.org/torvalds/t/linux-7.4-rc1.tar.gz","pgp":null,"released":{"timestamp":1,"isodate":"2026-11-01"}},
{"iseol":false,"version":"next-20261023","moniker":"linux-next","source":null,"pgp":null,"released":{"timestamp":1,"isodate":"2026-10-23"}}
]}"#;

    #[test]
    fn releases_are_parsed_and_linux_next_is_left_out() {
        let r = parse_releases(RELEASES).unwrap();
        let names: Vec<_> = r.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(names, ["7.3", "7.2.12", "6.6.160", "7.4-rc1"]);
        assert_eq!(r[1].moniker, Moniker::Stable);
        assert_eq!(r[1].released, "2026-10-21");
        assert_eq!(r[1].pgp.as_deref(), Some("https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.12.tar.sign"));
        assert!(r[2].is_eol && r[2].moniker == Moniker::Longterm);
        assert_eq!(r[3].pgp, None);
    }

    #[test]
    fn bad_or_empty_json_is_an_error() {
        assert!(parse_releases("not json").is_err());
        assert!(parse_releases(r#"{"releases":[]}"#).is_err());
        assert!(parse_releases(r#"{"releases":[{"moniker":"stable","version":"1","source":"http://insecure/x.tar.xz"}]}"#).is_err());
    }

    #[test]
    fn signature_urls() {
        assert_eq!(signature_url("7.2.12", 7), "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.12.tar.sign");
        assert_eq!(signature_url("7.3", 7), "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.tar.sign");
        assert_eq!(url_file_name("https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.tar.xz"), Some("linux-7.3.tar.xz"));
        assert_eq!(url_file_name("https://x/"), None);
        assert_eq!(url_file_name("https://x/.."), None);
    }

    /// A dropped tarball without its .tar.sign: the signature is fetched by
    /// the Makefile version and verifies. KERNEL_POP_LIVE_TARBALL=<.tar.xz>.
    #[tokio::test]
    #[ignore]
    async fn live_missing_signature_is_fetched_and_verifies() {
        let path = std::path::PathBuf::from(std::env::var("KERNEL_POP_LIVE_TARBALL").expect("set KERNEL_POP_LIVE_TARBALL"));
        let t = crate::source::inspect(&path).unwrap();
        let dir = std::env::temp_dir().join(format!("kernel-pop-live-sig-{}", std::process::id()));
        let sig = fetch_signature(&t.version, dir.clone()).await.unwrap();
        println!("fetched {}", sig.display());
        let signer = crate::verify::verify_tarball(&t, &sig, &|l| println!("{l}")).unwrap();
        println!("signed by {}", signer.uid);
        std::fs::remove_dir_all(&dir).unwrap();
        // A version kernel.org never released has no signature to fetch.
        let fake = crate::source::SourceVersion { version: 7, patchlevel: 2, sublevel: 999, extraversion: String::new() };
        assert!(fetch_signature(&fake, dir.clone()).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore]
    async fn live_releases_json_lists_stable_with_signature() {
        let r = fetch_releases().await.expect("live releases.json");
        let stable = r.iter().find(|x| x.moniker == Moniker::Stable).expect("a stable release");
        assert!(stable.source.ends_with(".tar.xz") && stable.pgp.is_some(), "{stable:?}");
    }
}
