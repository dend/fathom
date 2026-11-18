//! Formatting and file-type helpers shared by the UI.

use std::cmp::Ordering;

use crate::sigv4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
    Audio,
    Code,
    Text,
    Archive,
    File,
}

impl Kind {
    pub fn is_text(self) -> bool {
        matches!(self, Kind::Text | Kind::Code)
    }

    pub fn is_media(self) -> bool {
        matches!(self, Kind::Video | Kind::Audio)
    }
}

pub fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[dot + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn kind_of(name: &str) -> Kind {
    match extension_of(name).as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" | "heic" | "tif"
        | "tiff" => Kind::Image,
        "mp4" | "m4v" | "webm" | "mov" | "ogv" | "mkv" | "avi" => Kind::Video,
        "mp3" | "wav" | "ogg" | "oga" | "m4a" | "flac" | "aac" | "opus" => Kind::Audio,
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "json" | "html" | "htm" | "css" | "scss"
        | "xml" | "svg" | "yml" | "yaml" | "toml" | "rs" | "py" | "go" | "java" | "c" | "h"
        | "cpp" | "hpp" | "cs" | "rb" | "php" | "sh" | "bash" | "ps1" | "sql" | "lua" | "swift"
        | "kt" | "vue" | "svelte" | "map" | "graphql" | "proto" | "tf" | "dockerfile" => Kind::Code,
        "txt" | "md" | "markdown" | "csv" | "tsv" | "log" | "ini" | "conf" | "cfg" | "env"
        | "gitignore" | "srt" | "vtt" => Kind::Text,
        "zip" | "gz" | "tgz" | "tar" | "7z" | "rar" | "bz2" | "xz" | "zst" => Kind::Archive,
        _ => Kind::File,
    }
}

/// Whether the image decoder can show this file.
pub fn can_decode_image(name: &str) -> bool {
    matches!(
        extension_of(name).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
    )
}

pub fn type_label(name: &str) -> String {
    let extension = extension_of(name);
    let known = match extension.as_str() {
        "" => return "File".into(),
        "pdf" => "PDF document",
        "md" | "markdown" => "Markdown",
        "json" => "JSON",
        "csv" => "CSV",
        "txt" => "Text",
        "svg" => "SVG image",
        "html" | "htm" => "HTML",
        "css" => "Stylesheet",
        "js" | "mjs" => "JavaScript",
        "zip" => "ZIP archive",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    let upper = extension.to_ascii_uppercase();
    match kind_of(name) {
        Kind::Image => format!("{upper} image"),
        Kind::Video => format!("{upper} video"),
        Kind::Audio => format!("{upper} audio"),
        Kind::Archive => format!("{upper} archive"),
        _ => format!("{upper} file"),
    }
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if unit == 0 || value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.digits$} {}", UNITS[unit])
}

/// `1,234,567`
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn plural(count: usize, one: &str) -> String {
    let word = if count == 1 {
        one.to_string()
    } else {
        format!("{one}s")
    };
    format!("{} {word}", thousands(count as u64))
}

// ----- dates -------------------------------------------------------------------

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day, hour, minute, second) for a Unix timestamp.
pub fn civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let rem = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3_600, (rem % 3_600) / 60, rem % 60)
}

pub fn unix_from_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> i64 {
    days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
}

/// Parses the ISO 8601 timestamps S3 uses in listings: `2024-05-01T10:00:00.000Z`.
pub fn parse_iso8601(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    Some(unix_from_civil(
        number(0..4)?,
        number(5..7)?,
        number(8..10)?,
        number(11..13)?,
        number(14..16)?,
        number(17..19)?,
    ))
}

/// Parses HTTP dates as returned in `Last-Modified`: `Wed, 01 May 2024 10:00:00 GMT`.
pub fn parse_http_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace().skip(1);
    let day = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts.next()?.parse().ok()?;
    let mut time = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    Some(unix_from_civil(year, month, day, hour, minute, second))
}

/// `2024-05-01 10:00` in the user's local time zone.
pub fn date(unix: i64) -> String {
    let (year, month, day, hour, minute, _) = civil(unix + crate::system::local_offset(unix));
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

// ----- keys and names ------------------------------------------------------------

/// Last path segment of a key or prefix.
pub fn base_name(key: &str) -> &str {
    let trimmed = key.strip_suffix('/').unwrap_or(key);
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// Prefix containing a key or prefix: `a/b/c.txt` and `a/b/c/` both give `a/b/`.
pub fn parent_prefix(key: &str) -> &str {
    let trimmed = key.strip_suffix('/').unwrap_or(key);
    match trimmed.rfind('/') {
        Some(index) => &trimmed[..=index],
        None => "",
    }
}

/// Public URL of an object on a bucket's custom domain.
pub fn public_url(base: &str, key: &str) -> String {
    let path: Vec<String> = key
        .split('/')
        .map(|segment| sigv4::uri_encode(segment, true))
        .collect();
    format!("{}/{}", base.trim_end_matches('/'), path.join("/"))
}

/// Case-insensitive comparison that orders embedded numbers by value,
/// so `file2` sorts before `file10`.
pub fn compare_names(a: &str, b: &str) -> Ordering {
    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(&c) = chars.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        digits.push(c);
                        chars.next();
                    }
                    digits
                };
                let (x, y) = (take(&mut left), take(&mut right));
                let (xt, yt) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                let order = xt.len().cmp(&yt.len()).then_with(|| xt.cmp(yt));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                left.next();
                right.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_files() {
        assert_eq!(kind_of("photo.JPG"), Kind::Image);
        assert_eq!(kind_of("clip.webm"), Kind::Video);
        assert_eq!(kind_of(".env"), Kind::File);
        assert_eq!(type_label("archive.tar"), "TAR archive");
        assert_eq!(type_label("notes.md"), "Markdown");
        assert_eq!(type_label("README"), "File");
    }

    #[test]
    fn formats_sizes_and_counts() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1536), "1.50 KB");
        assert_eq!(size(3_000_000), "2.86 MB");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(plural(1, "item"), "1 item");
        assert_eq!(plural(1200, "file"), "1,200 files");
    }

    #[test]
    fn parses_dates() {
        let iso = parse_iso8601("2024-05-01T10:00:00.000Z").unwrap();
        let http = parse_http_date("Wed, 01 May 2024 10:00:00 GMT").unwrap();
        assert_eq!(iso, 1_714_557_600);
        assert_eq!(iso, http);
        assert_eq!(civil(iso), (2024, 5, 1, 10, 0, 0));
        assert_eq!(parse_iso8601("garbage"), None);
    }

    #[test]
    fn splits_keys() {
        assert_eq!(base_name("a/b/c.txt"), "c.txt");
        assert_eq!(base_name("a/b/"), "b");
        assert_eq!(parent_prefix("a/b/c.txt"), "a/b/");
        assert_eq!(parent_prefix("a/b/"), "a/");
        assert_eq!(parent_prefix("top.txt"), "");
        assert_eq!(
            public_url("https://cdn.example.com/", "photos/a b/c+d.png"),
            "https://cdn.example.com/photos/a%20b/c%2Bd.png"
        );
    }

    #[test]
    fn sorts_naturally() {
        let mut names = vec!["file10", "File2", "file1", "file02b", "alpha"];
        names.sort_by(|a, b| compare_names(a, b));
        assert_eq!(names, ["alpha", "file1", "File2", "file02b", "file10"]);
    }
}
