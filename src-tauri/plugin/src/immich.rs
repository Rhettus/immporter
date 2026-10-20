//! Ask an Immich server whether a photo is already in the library.
//!
//! Immich does not content-hash external-library files (their checksum is a
//! hash of the path), so the lookup is by file name, confirmed by exact size
//! and EXIF capture time — plus a real SHA-1 comparison for assets that were
//! uploaded (which do carry a content checksum). Only the `asset.read` API
//! permission is needed; nothing on the server is ever changed.

use std::time::Duration;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

pub const REQUIRED_PERMISSION: &str = "asset.read";
const KEYCHAIN_ACCOUNT: &str = "immich-api-key";
const NAMES_PER_REQUEST: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImmichSettings {
    pub enabled: bool,
    pub url: String,
    pub accept_invalid_certs: bool,
    /// Also require Immich's file path to match the planned folder structure.
    pub match_path: bool,
    /// Where the library root appears inside Immich, e.g. `/mnt/photos`.
    pub path_prefix: String,
}

/// `https://host/` or `https://host/api` → `https://host`.
pub fn normalize_url(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if u.ends_with("/api") {
        u.truncate(u.len() - 4);
    }
    u.trim_end_matches('/').to_string()
}

// ---- API key in the OS keychain --------------------------------------------

pub mod keychain {
    use super::*;
    use std::sync::OnceLock;

    /// The keychain service the key is kept under: the app's identifier, so
    /// the standalone importer (`app.sdcardimporter`) and an app embedding
    /// it each keep their own.
    static SERVICE: OnceLock<String> = OnceLock::new();

    pub fn set_service(service: &str) {
        let _ = SERVICE.set(service.to_string());
    }

    fn entry() -> Result<keyring::Entry, String> {
        let service = SERVICE.get().map(String::as_str).unwrap_or("app.sdcardimporter");
        keyring::Entry::new(service, KEYCHAIN_ACCOUNT).map_err(|e| e.to_string())
    }

    pub fn get() -> Option<String> {
        entry().ok()?.get_password().ok().filter(|k| !k.is_empty())
    }

    pub fn set(key: &str) -> Result<(), String> {
        entry()?.set_password(key).map_err(|e| e.to_string())
    }

    /// The key kept under another service (an app's identifier before a
    /// rename) moves under this one, unless this one has its own.
    pub fn migrate_from(old: &str) {
        if get().is_some() {
            return;
        }
        let Ok(e) = keyring::Entry::new(old, KEYCHAIN_ACCOUNT) else { return };
        if let Ok(key) = e.get_password() {
            if !key.is_empty() && set(&key).is_ok() {
                let _ = e.delete_credential();
            }
        }
    }

    pub fn delete() -> Result<(), String> {
        match entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---- API types -------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExifInfo {
    pub file_size_in_byte: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub id: String,
    pub original_file_name: String,
    #[serde(default)]
    pub original_path: String,
    pub library_id: Option<String>,
    pub checksum: Option<String>,
    pub local_date_time: Option<String>,
    pub exif_info: Option<ExifInfo>,
}

#[derive(Deserialize)]
struct SearchPage {
    items: Vec<Asset>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchPage,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub ok: bool,
    pub version: Option<String>,
    pub key_name: Option<String>,
    pub has_asset_read: bool,
    /// One asset's path, to suggest a path prefix.
    pub sample_path: Option<String>,
    pub error: Option<String>,
}

// ---- client ----------------------------------------------------------------

pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
}

fn describe(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "Immich did not respond in time".into()
    } else if e.is_connect() {
        "Cannot connect to Immich (check the URL and that the server is running)".into()
    } else {
        e.to_string()
    }
}

fn check_status(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response, String> {
    match resp.status().as_u16() {
        200..=299 => Ok(resp),
        401 => Err("Immich rejected the API key".into()),
        403 => Err(format!("The API key is missing the {REQUIRED_PERMISSION} permission")),
        404 => Err("Not an Immich server (API not found at this URL)".into()),
        code => Err(format!("Immich returned HTTP {code}")),
    }
}

impl Client {
    pub fn new(url: &str, api_key: &str, accept_invalid_certs: bool) -> Result<Self, String> {
        let base = normalize_url(url);
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err("The URL must start with http:// or https://".into());
        }
        let mut headers = reqwest::header::HeaderMap::new();
        let mut key = reqwest::header::HeaderValue::from_str(api_key.trim()).map_err(|_| "Invalid API key")?;
        key.set_sensitive(true);
        headers.insert("x-api-key", key);
        let http = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(15))
            .tls_danger_accept_invalid_certs(accept_invalid_certs)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http, base })
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        let resp = self.http.get(format!("{}/api{path}", self.base)).send().map_err(describe)?;
        check_status(resp)?.json().map_err(|e| e.to_string())
    }

    /// Check the server, its version and the key's permissions.
    pub fn test(&self) -> TestResult {
        let mut r = TestResult::default();
        if let Err(e) = self.get("/server/ping") {
            r.error = Some(e);
            return r;
        }
        match self.get("/server/version") {
            Ok(v) => {
                let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                r.version = Some(format!("{}.{}.{}", n("major"), n("minor"), n("patch")));
                if (n("major"), n("minor")) < (3, 2) {
                    r.error = Some("Immich 3.2 or newer is required".into());
                    return r;
                }
            }
            Err(e) => {
                r.error = Some(e);
                return r;
            }
        }
        match self.get("/api-keys/me") {
            Ok(k) => {
                r.key_name = k.get("name").and_then(|x| x.as_str()).map(String::from);
                let perms: Vec<&str> = k
                    .get("permissions")
                    .and_then(|p| p.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                r.has_asset_read = has_permission(&perms, REQUIRED_PERMISSION);
            }
            Err(e) => {
                r.error = Some(e);
                return r;
            }
        }
        if !r.has_asset_read {
            r.error = Some(format!("The API key is missing the {REQUIRED_PERMISSION} permission"));
            return r;
        }
        r.sample_path = self.sample_path();
        r.ok = true;
        r
    }

    fn search(&self, body: &serde_json::Value) -> Result<SearchPage, String> {
        let resp = self.http.post(format!("{}/api/search/metadata", self.base)).json(body).send().map_err(describe)?;
        let r: SearchResponse = check_status(resp)?.json().map_err(|e| format!("Unexpected search response: {e}"))?;
        Ok(r.assets)
    }

    fn sample_path(&self) -> Option<String> {
        let page = self.search(&serde_json::json!({ "size": 1 })).ok()?;
        page.items.into_iter().next().map(|a| a.original_path)
    }

    /// All assets whose original file name is one of `names`.
    pub fn search_names(&self, names: &[String]) -> Result<Vec<Asset>, String> {
        let mut out = Vec::new();
        for chunk in names.chunks(NAMES_PER_REQUEST) {
            let mut cursor: Option<String> = None;
            loop {
                let mut body = serde_json::json!({
                    "filter": { "originalFileName": { "in": chunk } },
                    "withExif": true,
                    "size": 1000,
                });
                if let Some(c) = &cursor {
                    body["cursor"] = serde_json::Value::String(c.clone());
                }
                let page = self.search(&body)?;
                out.extend(page.items);
                match page.next_cursor {
                    Some(c) if !c.is_empty() => cursor = Some(c),
                    _ => break,
                }
            }
        }
        Ok(out)
    }
}

pub fn has_permission(perms: &[&str], wanted: &str) -> bool {
    perms.iter().any(|p| *p == "all" || *p == wanted)
}

// ---- matching --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImmichMatch {
    pub asset_id: String,
    pub original_path: String,
    /// Confirmed by SHA-1 of the file contents (uploaded assets only).
    pub verified: bool,
}

pub struct MatchInput<'a> {
    pub card_name: &'a str,
    /// File name the naming template would produce, if different.
    pub planned_name: Option<&'a str>,
    pub size: u64,
    pub capture: Option<NaiveDateTime>,
    /// Planned path relative to the destination root, `/`-separated.
    pub planned_rel: Option<&'a str>,
    /// The naming template uses `{seq}`, so only the folder can be compared.
    pub name_unpredictable: bool,
    /// Names this file was actually imported under before, from the import
    /// history: (file name, path relative to the current destination root if
    /// it lies inside it). Covers template changes, `{seq}` and `_1` suffixes.
    pub history: &'a [(String, Option<String>)],
    /// Videos match on name + exact size only: their QuickTime timestamps are
    /// UTC in some cameras and local in others, so times can disagree.
    pub is_video: bool,
}

fn parse_local(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()
}

fn split_path(p: &str) -> (&str, &str) {
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

/// `IMG_1.JPG` matches `IMG_1.JPG` and `IMG_1_2.JPG` (collision suffix).
fn same_file_name(actual: &str, planned: &str) -> bool {
    if actual.eq_ignore_ascii_case(planned) {
        return true;
    }
    let (ps, pe) = planned.rsplit_once('.').unwrap_or((planned, ""));
    let (as_, ae) = actual.rsplit_once('.').unwrap_or((actual, ""));
    ae.eq_ignore_ascii_case(pe)
        && as_.len() > ps.len() + 1
        && as_[..ps.len()].eq_ignore_ascii_case(ps)
        && as_[ps.len()..].starts_with('_')
        && as_[ps.len() + 1..].chars().all(|c| c.is_ascii_digit())
}

/// Does Immich's path agree with where the importer would put the file?
pub fn path_matches(original_path: &str, planned_rel: &str, prefix: &str, name_unpredictable: bool) -> bool {
    let op = original_path.replace('\\', "/");
    let rel = planned_rel.replace('\\', "/");
    let (op_dir, op_file) = split_path(&op);
    let (rel_dir, rel_file) = split_path(&rel);
    if !name_unpredictable && !same_file_name(op_file, rel_file) {
        return false;
    }
    let prefix = prefix.trim().replace('\\', "/");
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        rel_dir.is_empty() || op_dir == rel_dir || op_dir.ends_with(&format!("/{rel_dir}"))
    } else if rel_dir.is_empty() {
        op_dir == prefix
    } else {
        op_dir == format!("{prefix}/{rel_dir}")
    }
}

/// Decide whether any candidate is this file. `sha1` lazily returns the
/// base64 SHA-1 of the card file; it is only called for uploaded assets.
pub fn match_file(
    input: &MatchInput,
    settings: &ImmichSettings,
    candidates: &[&Asset],
    sha1: &mut dyn FnMut() -> Option<String>,
) -> Option<ImmichMatch> {
    for a in candidates {
        let name = a.original_file_name.as_str();
        let history = input.history.iter().find(|(n, _)| n.eq_ignore_ascii_case(name));
        let name_ok = name.eq_ignore_ascii_case(input.card_name)
            || input.planned_name.is_some_and(|p| name.eq_ignore_ascii_case(p))
            || history.is_some();
        let size_ok = a.exif_info.as_ref().and_then(|e| e.file_size_in_byte) == Some(input.size);
        if !name_ok || !size_ok {
            continue;
        }
        if settings.match_path {
            // A name from the import history is checked against where that
            // copy actually went; otherwise against the planned path.
            let ok = match history {
                Some((_, Some(rel))) => path_matches(&a.original_path, rel, &settings.path_prefix, false),
                Some((_, None)) => false,
                None => match input.planned_rel {
                    Some(rel) => path_matches(&a.original_path, rel, &settings.path_prefix, input.name_unpredictable),
                    None => false,
                },
            };
            if !ok {
                continue;
            }
        }
        let uploaded = a.library_id.is_none() && a.checksum.as_deref().is_some_and(|c| !c.is_empty());
        if uploaded {
            if sha1().as_deref() == a.checksum.as_deref() {
                return Some(ImmichMatch { asset_id: a.id.clone(), original_path: a.original_path.clone(), verified: true });
            }
            continue;
        }
        let time_ok = input.is_video || match (input.capture, a.local_date_time.as_deref().and_then(parse_local)) {
            (Some(ours), Some(theirs)) => ours == theirs,
            // Without a date on either side, name + exact size decides.
            _ => true,
        };
        if time_ok {
            return Some(ImmichMatch { asset_id: a.id.clone(), original_path: a.original_path.clone(), verified: false });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::parse_exif_datetime;

    fn ext_asset(name: &str, path: &str, size: u64, local: &str) -> Asset {
        Asset {
            id: format!("id-{name}"),
            original_file_name: name.into(),
            original_path: path.into(),
            library_id: Some("lib".into()),
            checksum: Some("cGF0aA==".into()), // path hash; must be ignored
            local_date_time: Some(local.into()),
            exif_info: Some(ExifInfo { file_size_in_byte: Some(size) }),
        }
    }

    fn input<'a>(name: &'a str, size: u64, rel: Option<&'a str>) -> MatchInput<'a> {
        MatchInput {
            card_name: name,
            planned_name: None,
            size,
            capture: parse_exif_datetime("2026:10:05 06:50:16"),
            planned_rel: rel,
            name_unpredictable: false,
            history: &[],
            is_video: false,
        }
    }

    fn no_sha() -> impl FnMut() -> Option<String> {
        || panic!("SHA-1 must not be computed for external assets")
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize_url(" https://photos.example.com/ "), "https://photos.example.com");
        assert_eq!(normalize_url("http://nas:2283/api/"), "http://nas:2283");
        assert_eq!(normalize_url("http://nas:2283/api"), "http://nas:2283");
    }

    #[test]
    fn permissions() {
        assert!(has_permission(&["all"], "asset.read"));
        assert!(has_permission(&["album.read", "asset.read"], "asset.read"));
        assert!(!has_permission(&["asset.upload"], "asset.read"));
        assert!(!has_permission(&[], "asset.read"));
    }

    #[test]
    fn external_match_by_name_size_time() {
        let s = ImmichSettings::default();
        let a = ext_asset("IMG_2599.JPG", "/mnt/p/2026/October/IMG_2599.JPG", 100, "2026-10-05T06:50:16.000Z");
        let m = match_file(&input("IMG_2599.JPG", 100, None), &s, &[&a], &mut no_sha()).unwrap();
        assert_eq!(m.asset_id, "id-IMG_2599.JPG");
        assert!(!m.verified);
        // Size differs (e.g. an edited copy).
        assert!(match_file(&input("IMG_2599.JPG", 101, None), &s, &[&a], &mut no_sha()).is_none());
        // Different capture time (a different shot with a reused number).
        let b = ext_asset("IMG_2599.JPG", "/x", 100, "2019-01-01T00:00:00.000Z");
        assert!(match_file(&input("IMG_2599.JPG", 100, None), &s, &[&b], &mut no_sha()).is_none());
        // No EXIF date on the card file: name + size decide.
        let mut i = input("IMG_2599.JPG", 100, None);
        i.capture = None;
        assert!(match_file(&i, &s, &[&b], &mut no_sha()).is_some());
    }

    #[test]
    fn videos_ignore_capture_time() {
        let s = ImmichSettings::default();
        // Immich read the QuickTime time as UTC; the card says local.
        let a = ext_asset("P9214118.MOV", "/p/P9214118.MOV", 472_417_280, "2025-09-21T15:44:49Z");
        let mut i = input("P9214118.MOV", 472_417_280, None);
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_none());
        i.is_video = true;
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_some());
        i.size = 1;
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_none());
    }

    #[test]
    fn renamed_files_match_via_planned_name() {
        let s = ImmichSettings::default();
        let a = ext_asset("20261005_065016.JPG", "/p/20261005_065016.JPG", 100, "2026-10-05T06:50:16Z");
        let mut i = input("IMG_2599.JPG", 100, None);
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_none());
        i.planned_name = Some("20261005_065016.JPG");
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_some());
    }

    /// Imported as `OLD_NAME.ORF` with an earlier template; the template has
    /// since changed, so only the import history knows that name.
    #[test]
    fn template_change_matched_via_import_history() {
        let a = ext_asset("OLD_NAME.ORF", "/mnt/p/2026/March/OLD_NAME.ORF", 100, "2026-10-05T06:50:16Z");
        let mut i = input("P3270048.ORF", 100, Some("2026/March/NEW_NAME.ORF"));
        i.planned_name = Some("NEW_NAME.ORF");
        let s = ImmichSettings::default();
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_none());
        let history = vec![("OLD_NAME.ORF".to_string(), Some("2026/March/OLD_NAME.ORF".to_string()))];
        i.history = &history;
        assert!(match_file(&i, &s, &[&a], &mut no_sha()).is_some());
        // With the folder check on, the history path is what must match.
        let strict = ImmichSettings { match_path: true, path_prefix: "/mnt/p".into(), ..Default::default() };
        assert!(match_file(&i, &strict, &[&a], &mut no_sha()).is_some());
        let moved = ext_asset("OLD_NAME.ORF", "/mnt/p/2025/March/OLD_NAME.ORF", 100, "2026-10-05T06:50:16Z");
        assert!(match_file(&i, &strict, &[&moved], &mut no_sha()).is_none());
    }

    #[test]
    fn uploaded_assets_need_matching_sha1() {
        let s = ImmichSettings::default();
        let mut a = ext_asset("IMG_1.JPG", "upload/x/IMG_1.JPG", 100, "2026-10-05T06:50:16Z");
        a.library_id = None;
        a.checksum = Some("Zm9v".into());
        let i = input("IMG_1.JPG", 100, None);
        let m = match_file(&i, &s, &[&a], &mut || Some("Zm9v".into())).unwrap();
        assert!(m.verified);
        assert!(match_file(&i, &s, &[&a], &mut || Some("YmFy".into())).is_none());
    }

    #[test]
    fn folder_check() {
        let rel = "2026/October/IMG_2599.JPG";
        assert!(path_matches("/mnt/photos/2026/October/IMG_2599.JPG", rel, "/mnt/photos/", false));
        assert!(!path_matches("/mnt/photos/2026/September/IMG_2599.JPG", rel, "/mnt/photos", false));
        assert!(!path_matches("/other/2026/October/IMG_2599.JPG", rel, "/mnt/photos", false));
        // No prefix: suffix comparison.
        assert!(path_matches("/anything/2026/October/IMG_2599.JPG", rel, "", false));
        assert!(!path_matches("/anything/2026/Sept/IMG_2599.JPG", rel, "", false));
        // Collision suffix and {seq} naming.
        assert!(path_matches("/mnt/photos/2026/October/IMG_2599_1.JPG", rel, "/mnt/photos", false));
        assert!(!path_matches("/mnt/photos/2026/October/IMG_2599_x.JPG", rel, "/mnt/photos", false));
        assert!(path_matches("/mnt/photos/2026/October/2026-10-05_0042.JPG", "2026/October/2026-10-05_0001.JPG", "/mnt/photos", true));
        // Windows-style paths.
        assert!(path_matches("D:\\Photos\\2026\\October\\IMG_2599.JPG", rel, "D:\\Photos", false));

        let s = ImmichSettings { match_path: true, path_prefix: "/mnt/photos".into(), ..Default::default() };
        let good = ext_asset("IMG_2599.JPG", "/mnt/photos/2026/October/IMG_2599.JPG", 100, "2026-10-05T06:50:16Z");
        let wrong = ext_asset("IMG_2599.JPG", "/mnt/photos/2025/October/IMG_2599.JPG", 100, "2026-10-05T06:50:16Z");
        assert!(match_file(&input("IMG_2599.JPG", 100, Some(rel)), &s, &[&wrong], &mut no_sha()).is_none());
        assert!(match_file(&input("IMG_2599.JPG", 100, Some(rel)), &s, &[&wrong, &good], &mut no_sha()).is_some());
    }

    #[test]
    fn search_pages_and_connection_test() {
        let mut server = mockito::Server::new();
        let url = server.url();
        let p1 = server
            .mock("POST", "/api/search/metadata")
            .match_header("x-api-key", "secret")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "filter": { "originalFileName": { "in": ["A.JPG", "B.JPG"] } }, "withExif": true
            })))
            .match_request(|r| !r.utf8_lossy_body().unwrap().contains("cursor"))
            .with_body(r#"{"albums":{},"assets":{"items":[{"id":"1","originalFileName":"A.JPG","originalPath":"/p/A.JPG","libraryId":"l","checksum":"x","localDateTime":"2026-10-05T06:50:16.000Z","exifInfo":{"fileSizeInByte":5}}],"nextCursor":"c2"}}"#)
            .create();
        let p2 = server
            .mock("POST", "/api/search/metadata")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({ "cursor": "c2" })))
            .with_body(r#"{"albums":{},"assets":{"items":[{"id":"2","originalFileName":"B.JPG","originalPath":"/p/B.JPG","libraryId":null,"checksum":"y","localDateTime":null,"exifInfo":null}],"nextCursor":null}}"#)
            .create();
        let c = Client::new(&format!("{url}/api/"), "secret", false).unwrap();
        let assets = c.search_names(&["A.JPG".into(), "B.JPG".into()]).unwrap();
        assert_eq!(assets.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec!["1", "2"]);
        assert_eq!(assets[0].exif_info.as_ref().unwrap().file_size_in_byte, Some(5));
        p1.assert();
        p2.assert();

        server.mock("GET", "/api/server/ping").with_body(r#"{"res":"pong"}"#).create();
        server.mock("GET", "/api/server/version").with_body(r#"{"major":3,"minor":3,"patch":0}"#).create();
        let key = server
            .mock("GET", "/api/api-keys/me")
            .with_body(r#"{"name":"importer","permissions":["asset.upload"]}"#)
            .create();
        let r = c.test();
        assert!(!r.ok);
        assert_eq!(r.version.as_deref(), Some("3.3.0"));
        assert_eq!(r.key_name.as_deref(), Some("importer"));
        assert!(r.error.unwrap().contains("asset.read"));
        key.remove();
        server.mock("GET", "/api/api-keys/me").with_body(r#"{"name":"importer","permissions":["asset.read"]}"#).create();
        assert!(c.test().ok);

        // Bad key.
        let mut s2 = mockito::Server::new();
        s2.mock("GET", "/api/server/ping").with_status(401).create();
        let r = Client::new(&s2.url(), "nope", false).unwrap().test();
        assert!(r.error.unwrap().contains("rejected"));
    }
}
