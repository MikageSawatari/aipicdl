use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix 時刻を日本時間の `YYYY-MM-DD HH:MM` にする (サイトの表示に合わせて JST 固定)。
pub fn format_time(unix: i64) -> String {
    let t = unix + 9 * 3600;
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

pub fn format_date(unix: i64) -> String {
    format_time(unix)[..10].to_string()
}

/// Howard Hinnant の days_from_civil の逆変換。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Windows で使えない文字を置き換える。
pub fn safe_file_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    out = out.trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        out.push('_');
    }
    out
}

pub fn truncate_chars(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// `dir` 内で拡張子を除いた名前が `stem` のファイル (書きかけの .part 以外) を探す。
pub fn find_by_stem(dir: &Path, stem: &str) -> Option<String> {
    let rd = fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".part") {
            continue;
        }
        if let Some((s, _)) = name.rsplit_once('.')
            && s == stem
        {
            return Some(name);
        }
    }
    None
}

pub fn detect_ext(bytes: &[u8], content_type: Option<&str>, url: &str) -> String {
    let magic = if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        if &bytes[8..12] == b"avif" || &bytes[8..12] == b"avis" {
            Some("avif")
        } else {
            Some("mp4")
        }
    } else {
        None
    };
    if let Some(m) = magic {
        return m.to_string();
    }
    let ct = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    let from_ct = match ct {
        "image/webp" => Some("webp"),
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/avif" => Some("avif"),
        "image/svg+xml" => Some("svg"),
        "video/mp4" => Some("mp4"),
        _ => None,
    };
    if let Some(e) = from_ct {
        return e.to_string();
    }
    if let Ok(u) = url::Url::parse(url)
        && let Some(last) = u.path_segments().and_then(|mut s| s.next_back())
        && let Some((_, ext)) = last.rsplit_once('.')
        && (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return ext.to_ascii_lowercase();
    }
    "bin".to_string()
}

pub fn join_url(base: &str, rel: &str) -> Result<String> {
    let b = url::Url::parse(base).with_context(|| format!("URL が不正です: {base}"))?;
    Ok(b.join(rel)
        .with_context(|| format!("URL が不正です: {rel}"))?
        .to_string())
}

/// 一覧用の縮小画像 (長辺 480px の JPEG) を作る。
pub fn make_thumbnail(src: &Path, dst: &Path) -> Result<()> {
    let bytes = fs::read(src)?;
    let img = image::load_from_memory(&bytes).context("画像を読み込めません")?;
    let thumb = img.thumbnail(480, 480).to_rgb8();
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    thumb.write_with_encoder(enc)?;
    crate::job::write_atomic(dst, &out)
}

/// Markdown 本文中の画像 URL (`![](...)` と `<img src="...">`) を出現順・重複なしで返す。
pub fn markdown_image_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |u: &str| {
        let u = u.split_whitespace().next().unwrap_or("");
        let u = u.trim_start_matches('<').trim_end_matches('>');
        if (u.starts_with("http://") || u.starts_with("https://")) && !out.iter().any(|x| x == u) {
            out.push(u.to_string());
        }
    };
    let mut rest = text;
    while let Some(p) = rest.find("![") {
        rest = &rest[p + 2..];
        let Some(close) = rest.find("](") else { break };
        if rest[..close].contains('\n') {
            continue;
        }
        let after = &rest[close + 2..];
        let end = after.find([')', '\n']).unwrap_or(after.len());
        push(&after[..end]);
        rest = &after[end..];
    }
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("<img") {
        let start = from + p;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(lower.len());
        let tag = &text[start..end];
        let tag_l = &lower[start..end];
        if let Some(sp) = tag_l.find("src=") {
            let v = &tag[sp + 4..];
            let (q, v) = match v.chars().next() {
                Some(c @ ('"' | '\'')) => (Some(c), &v[1..]),
                _ => (None, v),
            };
            let stop = match q {
                Some(c) => v.find(c),
                None => v.find(|c: char| c.is_whitespace()),
            }
            .unwrap_or(v.len());
            push(&v[..stop]);
        }
        from = end.max(start + 4);
    }
    out
}

pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// プレーンテキストをエスケープし、改行を <br> に、http(s) URL をリンクにする。
pub fn text_to_html(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let next = [rest.find("https://"), rest.find("http://")]
            .into_iter()
            .flatten()
            .min();
        let Some(p) = next else {
            out.push_str(&html_escape(rest));
            break;
        };
        out.push_str(&html_escape(&rest[..p]));
        let tail = &rest[p..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "<>\"'「」（）()、。".contains(c))
            .unwrap_or(tail.len());
        let u = &tail[..end];
        let e = html_escape(u);
        out.push_str(&format!(
            "<a href=\"{e}\" target=\"_blank\" rel=\"noopener noreferrer\">{e}</a>"
        ));
        rest = &tail[end..];
    }
    out.replace("\r\n", "\n").replace('\n', "<br>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_jst() {
        assert_eq!(format_time(0), "1970-01-01 09:00");
        assert_eq!(format_time(1736780400), "2025-01-14 00:00");
        assert_eq!(format_time(951782400), "2000-02-29 09:00");
    }

    #[test]
    fn finds_markdown_images() {
        let t = "a ![x](https://e.com/a.png) b\n![y](<https://e.com/b.webp> \"t\")\n<IMG alt=1 src=\"https://e.com/c.jpg\">\n![z](https://e.com/a.png)";
        assert_eq!(
            markdown_image_urls(t),
            vec![
                "https://e.com/a.png",
                "https://e.com/b.webp",
                "https://e.com/c.jpg"
            ]
        );
    }

    #[test]
    fn linkifies() {
        assert_eq!(
            text_to_html("see https://x.com/a?b=1&c\nok<"),
            "see <a href=\"https://x.com/a?b=1&amp;c\" target=\"_blank\" rel=\"noopener noreferrer\">https://x.com/a?b=1&amp;c</a><br>\nok&lt;"
        );
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(safe_file_name("a/b:c?. "), "a_b_c_");
        assert_eq!(safe_file_name(""), "_");
    }
}
