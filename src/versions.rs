//! versions.rs — Talk to kernel.ubuntu.com/mainline: list versions, resolve
//! the generic-flavour amd64 .deb set for a version, and fetch checksums.

use anyhow::{bail, Context, Result};
use regex::Regex;
use scraper::{Html, Selector};
use std::collections::HashMap;

pub const MAINLINE_BASE: &str = "https://kernel.ubuntu.com/mainline/";

#[derive(Debug, Clone)]
pub struct KernelVersion {
    /// e.g. "7.1.3", or "7.2-rc1" when `is_rc`
    pub version: String,
    /// e.g. "https://kernel.ubuntu.com/mainline/v7.1.3/"
    pub url: String,
    /// True for `-rcN` directories. Filtered out of the Browse list by
    /// default — see the RC toggle in ui.rs — since these are pre-release
    /// and not what most users installing a mainline kernel want.
    pub is_rc: bool,
}

/// One .deb belonging to a kernel version.
#[derive(Debug, Clone)]
pub struct KernelDeb {
    pub filename: String,
    pub url: String,
    /// SHA256 from the CHECKSUMS file, if published.
    pub sha256: Option<String>,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("kernel-pop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// List stable versions from the mainline index, newest first.
/// Release candidates and the daily builds are skipped.
pub async fn fetch_versions() -> Result<Vec<KernelVersion>> {
    let html = client()?
        .get(MAINLINE_BASE)
        .send()
        .await
        .context("Failed to reach kernel.ubuntu.com")?
        .text()
        .await?;

    let versions = parse_versions(&html);
    if versions.is_empty() {
        bail!("No kernel versions found in the mainline index");
    }
    Ok(versions)
}

/// Leading dotted-numeric part of a version, as sortable components. An
/// RC's "-rcN" suffix isn't parseable as a version component and would
/// otherwise sort that entry as if every part after the first dot were 0.
fn version_sort_key(version: &str) -> Vec<u32> {
    version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .split('.')
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Parse the mainline index page into versions, newest first, de-duplicated.
fn parse_versions(html: &str) -> Vec<KernelVersion> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a[href]").unwrap();
    // Stable directories look like "v7.1.3/"; daily builds etc. are skipped
    // by not matching either pattern below.
    let ver_re = Regex::new(r"^v(\d+\.\d+(?:\.\d+)?)/$").unwrap();
    // Release-candidate directories look like "v7.2-rc1/". Captured
    // separately and tagged `is_rc` — see the RC toggle in ui.rs — rather
    // than skipped outright, since there's now a way to opt into seeing them.
    let rc_re = Regex::new(r"^v(\d+\.\d+(?:\.\d+)?-rc\d+)/$").unwrap();

    let mut versions: Vec<KernelVersion> = document
        .select(&selector)
        .filter_map(|el| {
            let href = el.value().attr("href")?;
            if let Some(caps) = ver_re.captures(href) {
                let version = caps[1].to_string();
                let url = format!("{}v{}/", MAINLINE_BASE, version);
                return Some(KernelVersion { version, url, is_rc: false });
            }
            let caps = rc_re.captures(href)?;
            let version = caps[1].to_string();
            let url = format!("{}v{}/", MAINLINE_BASE, version);
            Some(KernelVersion { version, url, is_rc: true })
        })
        .collect();

    versions.sort_by(|a, b| version_sort_key(&b.version).cmp(&version_sort_key(&a.version)));
    versions.dedup_by(|a, b| a.version == b.version);
    versions
}

/// Collect .deb links from one index page. Hrefs may be plain filenames or
/// prefixed with a subdirectory (e.g. "amd64/linux-image-…"), so URLs are
/// resolved against the page they came from.
fn debs_from_page(page_url: &str, html: &str) -> Vec<(String, String)> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a[href]").unwrap();
    let mut out = vec![];
    for el in document.select(&selector) {
        let Some(href) = el.value().attr("href") else { continue };
        if !href.ends_with(".deb") || href.starts_with("..") {
            continue;
        }
        let filename = href.rsplit('/').next().unwrap_or(href).to_string();
        let url = format!("{}{}", page_url, href.trim_start_matches("./"));
        out.push((filename, url));
    }
    out
}

/// True for the four generic-flavour amd64 packages this app installs:
/// linux-headers-*_all.deb, linux-headers-*-generic_*_amd64.deb,
/// linux-image-unsigned-*-generic_*_amd64.deb, linux-modules-*-generic_*_amd64.deb.
fn wanted_deb(filename: &str) -> bool {
    let generic_amd64 = filename.contains("-generic") && filename.ends_with("_amd64.deb");
    let headers_all = filename.starts_with("linux-headers-") && filename.ends_with("_all.deb");
    if headers_all {
        return true;
    }
    if !generic_amd64 {
        return false;
    }
    filename.starts_with("linux-image-unsigned-")
        || filename.starts_with("linux-image-")
        || filename.starts_with("linux-modules-")
        || filename.starts_with("linux-headers-")
}

/// Parse a mainline CHECKSUMS file: plain `hash  path` lines from sha1sum
/// and sha256sum runs. Only 64-char (SHA256) hashes are kept, keyed by the
/// path's basename so both flat and amd64/-prefixed layouts match.
fn parse_checksums(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(path)) = (parts.next(), parts.next()) else { continue };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let base = path.trim_start_matches('*').rsplit('/').next().unwrap_or(path);
        map.insert(base.to_string(), hash.to_lowercase());
    }
    map
}

/// Resolve the .deb set for one version. Tries the version page first
/// (older flat layout), then the amd64/ subdirectory (newer layout).
pub async fn fetch_deb_list(ver: &KernelVersion) -> Result<Vec<KernelDeb>> {
    let client = client()?;

    let top_html = client
        .get(&ver.url)
        .send()
        .await
        .context("Failed to fetch kernel version page")?
        .text()
        .await?;

    let mut found = debs_from_page(&ver.url, &top_html);

    if !found.iter().any(|(f, _)| wanted_deb(f)) {
        let amd64_url = format!("{}amd64/", ver.url);
        if let Ok(resp) = client.get(&amd64_url).send().await {
            if resp.status().is_success() {
                let html = resp.text().await?;
                found.extend(debs_from_page(&amd64_url, &html));
            }
        }
    }

    // Checksums are optional — a missing CHECKSUMS file is not fatal.
    let mut sums = HashMap::new();
    let checksums_url = format!("{}CHECKSUMS", ver.url);
    if let Ok(resp) = client.get(&checksums_url).send().await {
        if resp.status().is_success() {
            if let Ok(text) = resp.text().await {
                sums = parse_checksums(&text);
            }
        }
    }

    let mut debs: Vec<KernelDeb> = vec![];
    for (filename, url) in found {
        if !wanted_deb(&filename) {
            continue;
        }
        if debs.iter().any(|d: &KernelDeb| d.filename == filename) {
            continue;
        }
        let sha256 = sums.get(&filename).cloned();
        debs.push(KernelDeb { filename, url, sha256 });
    }

    if debs.is_empty() {
        bail!(
            "No generic amd64 packages found for v{} — the build may have \
             failed for this architecture (check {} in a browser)",
            ver.version, ver.url
        );
    }
    debs.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(debs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_HTML: &str = r#"<html><body>
<a href="?C=N;O=D">Name</a>
<a href="/">Parent</a>
<a href="v5.4.9/">v5.4.9/</a>
<a href="v6.10/">v6.10/</a>
<a href="v6.9.12/">v6.9.12/</a>
<a href="v6.11-rc2/">v6.11-rc2/</a>
<a href="v6.10/">v6.10/</a>
<a href="daily/">daily/</a>
<a href="v6.9.12/amd64/">not a version dir</a>
</body></html>"#;

    #[test]
    fn parse_versions_sorts_numerically_dedups_and_tags_rc() {
        let v = parse_versions(INDEX_HTML);
        let names: Vec<_> = v.iter().map(|k| k.version.as_str()).collect();
        assert_eq!(names, ["6.11-rc2", "6.10", "6.9.12", "5.4.9"]);
        assert!(v[0].is_rc);
        assert!(v[1..].iter().all(|k| !k.is_rc));
        assert_eq!(v[1].url, "https://kernel.ubuntu.com/mainline/v6.10/");
    }

    #[test]
    fn parse_versions_empty_page() {
        assert!(parse_versions("<html></html>").is_empty());
    }

    #[test]
    fn sort_key_ignores_rc_suffix() {
        assert_eq!(version_sort_key("6.11-rc2"), vec![6, 11]);
        assert_eq!(version_sort_key("6.9.12"), vec![6, 9, 12]);
        assert!(version_sort_key("6.10") > version_sort_key("6.9.12"));
    }

    #[test]
    fn checksums_keep_only_sha256_keyed_by_basename() {
        let sha256 = "A".repeat(64);
        let sha1 = "b".repeat(40);
        let text = format!(
            "{sha1}  linux-a.deb\n{sha256}  amd64/linux-image-unsigned-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb\n{sha256} *linux-b.deb\nnot a line\n\n"
        );
        let m = parse_checksums(&text);
        assert_eq!(m.len(), 2);
        assert!(!m.contains_key("linux-a.deb"));
        assert_eq!(
            m["linux-image-unsigned-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb"],
            "a".repeat(64)
        );
        assert!(m.contains_key("linux-b.deb"));
    }

    #[test]
    fn wanted_deb_selects_generic_amd64_set() {
        assert!(wanted_deb("linux-image-unsigned-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb"));
        assert!(wanted_deb("linux-modules-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb"));
        assert!(wanted_deb("linux-headers-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb"));
        assert!(wanted_deb("linux-headers-6.9.12-060912_6.9.12-060912.202407_all.deb"));
        assert!(!wanted_deb("linux-image-unsigned-6.9.12-060912-lowlatency_6.9.12-060912.202407_amd64.deb"));
        assert!(!wanted_deb("linux-modules-6.9.12-060912-generic_6.9.12-060912.202407_arm64.deb"));
        assert!(!wanted_deb("linux-buildinfo-6.9.12-060912-generic_6.9.12-060912.202407_amd64.deb"));
        assert!(!wanted_deb("CHECKSUMS"));
    }

    #[test]
    fn debs_from_page_resolves_urls_and_skips_parent_links() {
        let html = r#"<a href="../">up</a>
<a href="linux-modules-1-generic_1_amd64.deb">a</a>
<a href="./linux-headers-1_1_all.deb">b</a>
<a href="amd64/linux-image-1-generic_1_amd64.deb">c</a>
<a href="../linux-evil.deb">d</a>
<a href="README">e</a>"#;
        let out = debs_from_page("https://example.test/v1/", html);
        assert_eq!(
            out,
            vec![
                ("linux-modules-1-generic_1_amd64.deb".into(), "https://example.test/v1/linux-modules-1-generic_1_amd64.deb".into()),
                ("linux-headers-1_1_all.deb".into(), "https://example.test/v1/linux-headers-1_1_all.deb".into()),
                ("linux-image-1-generic_1_amd64.deb".into(), "https://example.test/v1/amd64/linux-image-1-generic_1_amd64.deb".into()),
            ]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_fetch_includes_rc_entries_with_working_deb_layout() {
        let versions = fetch_versions().await.expect("live fetch failed");
        assert!(!versions.is_empty());
        let stable = versions.iter().filter(|v| !v.is_rc).count();
        let rcs: Vec<_> = versions.iter().filter(|v| v.is_rc).collect();
        println!("stable: {stable}, rc: {}", rcs.len());
        assert!(stable > 0);
        assert!(!rcs.is_empty(), "expected at least one -rcN directory live");
        for rc in rcs.iter().take(1) {
            println!("checking RC {} deb layout at {}", rc.version, rc.url);
            let debs = fetch_deb_list(rc).await.expect("RC deb layout differs from stable");
            assert!(!debs.is_empty());
            for d in &debs {
                println!("  {} (sha256: {})", d.filename, d.sha256.is_some());
            }
        }
    }
}
