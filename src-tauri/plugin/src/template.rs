//! Folder and file-name templates, e.g. `{YYYY}/{MMMM}` → `2026/October`.
//!
//! Tokens: {YYYY} {YY} {MM} {M} {MMMM} {MMM} {DD} {D} {HH} {mm} {ss}
//!         {make} {camera} {original} {seq}

use std::path::PathBuf;

use chrono::{Datelike, NaiveDateTime, Timelike};

pub struct Ctx<'a> {
    pub date: NaiveDateTime,
    pub make: Option<&'a str>,
    pub model: Option<&'a str>,
    /// Original file name without extension.
    pub original: &'a str,
    pub seq: u32,
}

pub const TOKENS: &[&str] = &[
    "YYYY", "YY", "MMMM", "MMM", "MM", "M", "DD", "D", "HH", "mm", "ss", "make", "camera", "original", "seq",
];

fn token(name: &str, c: &Ctx) -> Option<String> {
    let d = &c.date;
    Some(match name {
        "YYYY" => format!("{:04}", d.year()),
        "YY" => format!("{:02}", d.year() % 100),
        "MMMM" => d.format("%B").to_string(),
        "MMM" => d.format("%b").to_string(),
        "MM" => format!("{:02}", d.month()),
        "M" => d.month().to_string(),
        "DD" => format!("{:02}", d.day()),
        "D" => d.day().to_string(),
        "HH" => format!("{:02}", d.hour()),
        "mm" => format!("{:02}", d.minute()),
        "ss" => format!("{:02}", d.second()),
        "make" => c.make.unwrap_or("Unknown").to_string(),
        "camera" => c.model.unwrap_or("Unknown").to_string(),
        "original" => c.original.to_string(),
        "seq" => format!("{:04}", c.seq),
        _ => return None,
    })
}

/// Expand tokens. Unknown `{tokens}` are kept literally so typos are visible.
pub fn expand(tpl: &str, c: &Ctx) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match token(name, c) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Make a single path component safe on every OS.
pub fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    // Windows rejects trailing dots/spaces.
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    match trimmed {
        "" | "." | ".." => String::new(),
        t => t.to_string(),
    }
}

/// Relative folder for a file, e.g. `2026/October`. Empty template = flat.
pub fn folder(tpl: &str, c: &Ctx) -> PathBuf {
    expand(tpl, c)
        .split(['/', '\\'])
        .map(sanitize_component)
        .filter(|s| !s.is_empty())
        .collect()
}

/// File stem (no extension). Falls back to the original name when the
/// template is blank or expands to nothing usable.
pub fn file_stem(tpl: &str, c: &Ctx) -> String {
    let tpl = if tpl.trim().is_empty() { "{original}" } else { tpl };
    let s = sanitize_component(&expand(tpl, c).replace(['/', '\\'], "-"));
    if s.is_empty() {
        sanitize_component(c.original)
    } else {
        s
    }
}

/// `name.ext` → `name_1.ext`, `name_2.ext`, …
pub fn with_suffix(stem: &str, ext: &str, n: u32) -> String {
    let base = if n == 0 { stem.to_string() } else { format!("{stem}_{n}") };
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// Path of a file relative to the destination root, before collision suffixes.
pub fn plan(folder_tpl: &str, name_tpl: &str, c: &Ctx, ext: &str) -> PathBuf {
    folder(folder_tpl, c).join(with_suffix(&file_stem(name_tpl, c), ext, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::parse_exif_datetime;

    fn ctx() -> Ctx<'static> {
        Ctx {
            date: parse_exif_datetime("2026:10:05 07:08:09").unwrap(),
            make: Some("OM Digital Solutions"),
            model: Some("OM-1 Mark II"),
            original: "P8200112",
            seq: 7,
        }
    }

    #[test]
    fn year_month_name() {
        assert_eq!(folder("{YYYY}/{MMMM}", &ctx()), PathBuf::from("2026/October"));
        assert_eq!(folder("{YYYY}/{YYYY}-{MM}-{DD}", &ctx()), PathBuf::from("2026/2026-10-05"));
        assert_eq!(folder("", &ctx()), PathBuf::new());
    }

    #[test]
    fn names() {
        assert_eq!(file_stem("{original}", &ctx()), "P8200112");
        assert_eq!(file_stem("{YYYY}{MM}{DD}_{HH}{mm}{ss}", &ctx()), "20261005_070809");
        assert_eq!(file_stem("{YYYY}-{MM}-{DD}_{original}_{seq}", &ctx()), "2026-10-05_P8200112_0007");
        assert_eq!(file_stem("", &ctx()), "P8200112");
        assert_eq!(file_stem("{camera}/{bogus}", &ctx()), "OM-1 Mark II-{bogus}");
        assert_eq!(with_suffix("a", "ORF", 0), "a.ORF");
        assert_eq!(with_suffix("a", "ORF", 2), "a_2.ORF");
    }

    #[test]
    fn plans_full_relative_path() {
        assert_eq!(plan("{YYYY}/{MMMM}", "{original}", &ctx(), "ORF"), PathBuf::from("2026/October/P8200112.ORF"));
    }

    #[test]
    fn no_traversal() {
        assert_eq!(folder("../{YYYY}/./../x", &ctx()), PathBuf::from("2026/x"));
        assert_eq!(sanitize_component("a:b*c?. "), "a-b-c-");
        assert_eq!(file_stem("..", &ctx()), "P8200112");
    }
}
