use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::{
    style::Style,
    text::Line,
};

use crate::state::{Lyric, VisLine};
use crate::view::{LYRIC_ROWS, dim_style};

pub(crate) fn parse_lrc(body: &str) -> Vec<Lyric> {
    let mut out: Vec<Lyric> = vec![];
    for raw in body.lines() {
        // extrae todos [mm:ss.xx] del inicio
        let mut times: Vec<f64> = vec![];
        let mut rest = raw;
        loop {
            let l = match rest.find('[') {
                Some(i) => i,
                None => break,
            };
            let r = match rest[l..].find(']') {
                Some(i) => l + i,
                None => break,
            };
            let inner = &rest[l + 1..r];
            if let Some(ci) = inner.find(':') {
                if let (Ok(m), Ok(s)) = (
                    inner[..ci].parse::<f64>(),
                    inner[ci + 1..].parse::<f64>(),
                ) {
                    times.push(m * 60.0 + s);
                    rest = &rest[r + 1..];
                    continue;
                }
            }
            break;
        }
        let text = rest.trim().to_string();
        if times.is_empty() || text.is_empty() {
            continue;
        }
        for sec in times {
            out.push(Lyric { sec, text: text.clone() });
        }
    }
    out.sort_by(|a, b| a.sec.partial_cmp(&b.sec).unwrap());
    out
}

pub(crate) fn http_agent() -> ureq::Agent {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(5))
                .build()
        })
        .clone()
}

pub(crate) fn curl_json(url: &str) -> Option<serde_json::Value> {
    // Conexión reutilizada (keep-alive) como tu Go net/http: sin fork ni
    // TLS frío por tema.
    http_agent().get(url).call().ok()?.into_json().ok()
}

// ---- caché en disco: en el concurso repiten temas; la 2ª vez es instantáneo.
// Tu Go no cachea (refetch por cambio de track). ----

pub(crate) fn cache_file(kind: &str, name: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME").ok().filter(|s| !s.is_empty()).or_else(|| {
        std::env::var("HOME").ok().map(|h| format!("{}/.cache", h))
    })?;
    let dir = std::path::Path::new(&base).join("musik").join(kind);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(name))
}

pub(crate) fn hash_key(parts: &[&str]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

pub(crate) fn fetch_lyrics(artist: &str, title: &str, album: &str, duration: f64) -> Vec<Lyric> {
    // percent-encoding correcto (igual que Go url.Values.Encode):
    // deja A-Za-z0-9 - _ . ~, todo lo demás (% á é ( ) etc) va %XX por byte UTF-8.
    let enc = |s: &str| {
        let mut o = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
                o.push(b as char);
            } else {
                o.push_str(&format!("%{:02X}", b));
            }
        }
        o
    };
    let dur_i = (duration + 0.5) as i64;
    // caché: artista+título+álbum+duración (a disco, sobrevive reinicios)
    let ckey = hash_key(&[artist, title, album, &dur_i.to_string()]) + ".lrc";
    if let Some(p) = cache_file("lyrics", &ckey) {
        if let Ok(raw) = std::fs::read_to_string(&p) {
            let l = parse_lrc(&raw);
            if !l.is_empty() {
                return l;
            }
        }
    }
    let mut u = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        enc(artist),
        enc(title)
    );
    if !album.is_empty() {
        u += &format!("&album_name={}", enc(album));
    }
    if duration > 0.0 {
        u += &format!("&duration={}", dur_i);
    }
    let u2 = format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        enc(artist),
        enc(title)
    );
    // exact + search EN PARALELO (tu Go los hace en serie: gana ~1 RTT).
    let (tx2, rx2) = mpsc::channel::<(&str, Option<String>)>();
    thread::scope(|s| {
        s.spawn(|| {
            let raw = curl_json(&u)
                .and_then(|v| v.get("syncedLyrics").and_then(|x| x.as_str()).map(|x| x.to_string()))
                .filter(|x| !parse_lrc(x).is_empty());
            let _ = tx2.send(("exact", raw));
        });
        s.spawn(|| {
            let mut best: Option<String> = None;
            if let Some(v) = curl_json(&u2) {
                if let Some(arr) = v.as_array() {
                    for f in arr {
                        let x = f.get("syncedLyrics").and_then(|x| x.as_str()).unwrap_or("");
                        if x.is_empty() {
                            continue;
                        }
                        let d = f.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        if duration > 0.0 && (d - duration).abs() > 5.0 {
                            continue;
                        }
                        if !parse_lrc(x).is_empty() {
                            best = Some(x.to_string());
                            break;
                        }
                    }
                }
            }
            let _ = tx2.send(("search", best));
        });
    });
    let mut exact: Option<String> = None;
    let mut search: Option<String> = None;
    for _ in 0..2 {
        if let Ok((which, raw)) = rx2.recv() {
            if which == "exact" {
                exact = raw;
            } else {
                search = raw;
            }
        }
    }
    // exact manda (igual que Go); search es fallback.
    let raw = exact.or(search).unwrap_or_default();
    if raw.is_empty() {
        return vec![];
    }
    if let Some(p) = cache_file("lyrics", &ckey) {
        let _ = std::fs::write(&p, &raw);
    }
    parse_lrc(&raw)
}

pub(crate) fn lyric_current(lyrics: &[Lyric], pos: f64) -> i64 {
    let mut c: i64 = -1;
    for (i, l) in lyrics.iter().enumerate() {
        if l.sec <= pos {
            c = i as i64;
        } else {
            break;
        }
    }
    c
}

pub(crate) fn wrap_cell(s: &str, w: usize) -> Vec<String> {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = vec![];
    let mut cur = String::new();
    for wd in words {
        // palabra más larga que w: corta en duro (igual que Go)
        let mut rest = wd;
        while rest.chars().count() > w {
            if !cur.is_empty() {
                lines.push(cur);
                cur = String::new();
            }
            let part: String = rest.chars().take(w).collect();
            lines.push(part);
            rest = &rest[rest.char_indices().nth(w).map(|(i, _)| i).unwrap_or(rest.len())..];
            // recalcula con chars (evita cortar utf8): rehace simple
            let chars: Vec<char> = wd.chars().collect();
            let _ = chars;
        }
        if cur.is_empty() {
            cur = rest.to_string();
        } else if cur.chars().count() + 1 + rest.chars().count() <= w {
            cur += " ";
            cur += rest;
        } else {
            lines.push(cur);
            cur = rest.to_string();
        }
    }
    lines.push(cur);
    lines
}

pub(crate) fn lyric_visual(lyrics: &[Lyric], pos: f64, w: usize, term_h: usize) -> Vec<VisLine> {
    let cur = lyric_current(lyrics, pos);
    let mut flat: Vec<VisLine> = vec![];
    for (i, l) in lyrics.iter().enumerate() {
        for (j, t) in wrap_cell(&l.text, w.saturating_sub(2)).into_iter().enumerate() {
            flat.push(VisLine { idx: i, first: j == 0, text: t });
        }
    }
    let mut at: usize = 0;
    for (i, v) in flat.iter().enumerate() {
        if v.idx as i64 == cur && v.first {
            at = i;
            break;
        }
        if v.idx as i64 > cur {
            break;
        }
        at = i + 1;
    }
    if at > flat.len() {
        at = flat.len();
    }
    let zr = zone_rows(term_h);
    let mut start = at.saturating_sub(zr / 2);
    if start + zr > flat.len() {
        start = flat.len().saturating_sub(zr);
    }
    let end = (start + zr).min(flat.len());
    flat.into_iter().skip(start).take(end - start).collect()
}

pub(crate) fn center_text(s: &str, w: usize) -> String {
    let r = s.chars().count();
    if r >= w {
        return s.to_string();
    }
    " ".repeat((w - r) / 2) + s
}

pub(crate) fn lyric_lines(lyrics: &[Lyric], pos: f64, playing: bool, w: usize, term_h: usize) -> Vec<Line<'static>> {
    if lyrics.is_empty() {
        return vec![];
    }
    let cur = lyric_current(lyrics, pos);
    let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
    let dim = dim_style();
    let mut out: Vec<Line> = vec![];
    for v in lyric_visual(lyrics, pos, w, term_h) {
        let t = center_text(&v.text, w + 2);
        if v.idx as i64 == cur && v.first && playing {
            out.push(Line::from(vec![ratatui::text::Span::styled(t, bold)]));
        } else {
            out.push(Line::from(vec![ratatui::text::Span::styled(t, dim)]));
        }
    }
    while out.len() < zone_rows(term_h) {
        out.push(Line::raw(""));
    }
    out
}

pub(crate) fn zone_rows(term_h: usize) -> usize {
    if term_h > 0 && term_h < 20 {
        return 1; // tiny: siempre el verso activo
    }
    if term_h > 0 && term_h < 34 {
        return 3;
    }
    LYRIC_ROWS // full
}
