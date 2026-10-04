// musik — paridad visual con music-cli (Go). Solo Apple Music por ahora.
// Transporte reactivo: `media-control stream` en hilo (igual que Go stream.go),
// carátula (chafa) y letras (LRCLIB) en workers para no congelar la UI.
// Regla: la UI debe ser exactamente como ahora para comparar en dos pantallas.

use std::io::{self, BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use image::DynamicImage;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Gauge, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};

const COVER_W: usize = 60; // = coverWidth en Go (tui.go + cover.go)
const LYRIC_ROWS: usize = 9;

// Faint SGR 2 idéntico al Go (styleDim=Faint): verificado por captura PTY que
// music-cli emite `\x1b[2m...\x1b[0m` en meta y versos. Nada de grises fijos.
fn dim_style() -> Style {
    Style::default().add_modifier(ratatui::style::Modifier::DIM)
}

// Traza a /tmp/musik-debug.log solo con MUSIK_DEBUG=1 (para el concurso).
macro_rules! dlog {
    ($($a:tt)*) => {
        if std::env::var("MUSIK_DEBUG").map(|v| v == "1").unwrap_or(false) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/musik-debug.log") {
                let _ = writeln!(f, "[{:?}] {}", std::time::Instant::now(), format!($($a)*));
            }
        }
    };
}

fn zone_rows(term_h: usize) -> usize {
    if term_h > 0 && term_h < 20 {
        return 1; // tiny: siempre el verso activo
    }
    if term_h > 0 && term_h < 34 {
        return 3;
    }
    LYRIC_ROWS // full
}

#[derive(Default, Clone)]
struct Lyric {
    sec: f64,
    text: String,
}

fn parse_lrc(body: &str) -> Vec<Lyric> {
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

fn http_agent() -> ureq::Agent {
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

fn curl_json(url: &str) -> Option<serde_json::Value> {
    // Conexión reutilizada (keep-alive) como tu Go net/http: sin fork ni
    // TLS frío por tema.
    http_agent().get(url).call().ok()?.into_json().ok()
}

// ---- caché en disco: en el concurso repiten temas; la 2ª vez es instantáneo.
// Tu Go no cachea (refetch por cambio de track). ----

fn cache_file(kind: &str, name: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME").ok().filter(|s| !s.is_empty()).or_else(|| {
        std::env::var("HOME").ok().map(|h| format!("{}/.cache", h))
    })?;
    let dir = std::path::Path::new(&base).join("musik").join(kind);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(name))
}

fn hash_key(parts: &[&str]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn fetch_lyrics(artist: &str, title: &str, album: &str, duration: f64) -> Vec<Lyric> {
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

fn lyric_current(lyrics: &[Lyric], pos: f64) -> i64 {
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

fn wrap_cell(s: &str, w: usize) -> Vec<String> {
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

struct VisLine {
    idx: usize,
    first: bool,
    text: String,
}

fn lyric_visual(lyrics: &[Lyric], pos: f64, w: usize, term_h: usize) -> Vec<VisLine> {
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

#[derive(Default, Clone)]
struct Now {
    title: String,
    artist: String,
    album: String,
    duration: f64,
    elapsed: f64,
    at: Option<Instant>,
    playing: bool,
    shuffle: bool,
    repeat: String, // "off" | "one" | "all"
    fav: bool,
    track_since: Option<Instant>,
    src: String, // "music" | "podcast" | "" — como Go nowInfo.Src
}

impl Now {
    fn key(&self) -> String {
        format!("{}\x00{}\x00{}", self.title, self.artist, self.album)
    }
    fn position(&self) -> f64 {
        if !self.playing {
            return self.elapsed;
        }
        match self.at {
            Some(t) => self.elapsed + t.elapsed().as_secs_f64(),
            None => self.elapsed,
        }
    }
    fn meta(&self) -> String {
        if self.album.is_empty() {
            self.artist.clone()
        } else {
            format!("{} — {}", self.artist, self.album)
        }
    }
    // Ciclo repeat igual que Go (tecla r): off -> one -> all -> off.
    fn repeat_cycle(&mut self) {
        self.repeat = match self.repeat.as_str() {
            "one" => "all".to_string(),
            "all" => "off".to_string(),
            _ => "one".to_string(),
        };
    }
}

// ---- helpers idénticos a Go ----

fn fmt_time(s: f64) -> String {
    let m = (s as i64) / 60;
    let sec = (s as i64) % 60;
    format!("{}:{:02}", m, sec)
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

// marquee 1-pass: 3s quieto, un barrido ~10 pasos/s, luego quieto al inicio.
fn marquee(s: &str, width: usize, now: Instant, since: Instant) -> String {
    let r: Vec<char> = s.chars().collect();
    if r.len() <= width {
        return s.to_string();
    }
    let head: String = r[..width.saturating_sub(1)].iter().collect::<String>() + "…";
    let e = now.duration_since(since);
    if e < Duration::from_secs(3) {
        return head;
    }
    let pad: Vec<char> = "       ".chars().collect();
    let mut lp: Vec<char> = Vec::with_capacity(r.len() * 2 + pad.len());
    lp.extend_from_slice(&r);
    lp.extend_from_slice(&pad);
    lp.extend_from_slice(&r);
    let max_step = r.len() + pad.len();
    let step = (e.as_millis().saturating_sub(3000) / 100) as usize;
    if step >= max_step {
        return head;
    }
    lp[step..step + width].iter().collect()
}

fn cw(term_w: usize) -> usize {
    let mut w = COVER_W;
    if term_w >= 2 && term_w - 2 < w {
        w = term_w.saturating_sub(4);
    }
    if w < 16 {
        w = 16;
    }
    w
}

fn marquee_live(n: &Now, width: usize) -> bool {
    let since = match n.track_since {
        Some(t) => t,
        None => return false,
    };
    let mut longest = char_count(&n.title);
    let m = char_count(&n.meta());
    if m > longest {
        longest = m;
    }
    if longest <= width {
        return false;
    }
    let pass = Duration::from_secs(3)
        + Duration::from_millis(((longest - width + 7) * 100) as u64);
    since.elapsed() < pass
}

// ---- media-control (sin permiso Automation, igual que music-cli stream.go) ----

fn mc(args: &[&str]) -> Option<String> {
    let out = Command::new("media-control").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn mc_control(args: &[&str]) {
    let _ = Command::new("media-control")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// Transporte Music: osascript al blanco explícito (igual que tu Go) con
// fallback a media-control si no hay permiso Automation (-1743).
// media-control solo NO sirve: va al dueño del Now Playing (p. ej. el
// browser) y el pause le pegaría a la pestaña en vez de a Music.
// Todo en background: la UI ya flipeó optimista.
fn music_ctl(script: &str, mc_args: &[&str]) {
    let script = script.to_string();
    let mc_args: Vec<String> = mc_args.iter().map(|s| s.to_string()).collect();
    thread::spawn(move || {
        let ok = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            dlog!("osascript falló, fallback media-control");
            let _ = Command::new("media-control")
                .args(&mc_args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    });
}

fn transport_pp(podcast: bool, to_playing: bool) {
    if podcast {
        // podcasts no tienen AppleScript: explícito (toggle-play-pause de
        // media-control no conmuta fiable: verificado).
        mc_control(&[if to_playing { "play" } else { "pause" }]);
    } else {
        music_ctl(
            "tell application \"Music\" to playpause",
            &[if to_playing { "play" } else { "pause" }],
        );
    }
}

fn transport_next(podcast: bool) {
    if podcast {
        mc_control(&["next-track"]);
    } else {
        music_ctl("tell application \"Music\" to next track", &["next-track"]);
    }
}

fn transport_prev(podcast: bool) {
    if podcast {
        mc_control(&["previous-track"]);
    } else {
        music_ctl("tell application \"Music\" to previous track", &["previous-track"]);
    }
}

fn transport_seek(podcast: bool, pos: f64) {
    if podcast {
        mc_control(&["seek", &format!("{:.1}", pos)]);
    } else {
        music_ctl(
            &format!("tell application \"Music\" to set player position to {}", pos),
            &["seek", &format!("{:.1}", pos)],
        );
    }
}

fn transport_shuffle(on: bool) {
    music_ctl(
        &format!("tell application \"Music\" to set shuffle enabled to {}", if on { "true" } else { "false" }),
        &["toggle-shuffle"],
    );
}

fn transport_repeat(mode: &str) {
    music_ctl(
        &format!("tell application \"Music\" to set song repeat to {}", mode),
        &["toggle-repeat"],
    );
}

// osascript sin espera (favoritos/volumen): best-effort, como Go playCmd.
fn osa_control(script: &str) {
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn get_str(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

// ---- carátula idéntica a Go (cover.go): media-control artworkData -> tmp jpg -> chafa ----

fn cover_rows(term_h: usize) -> usize {
    if term_h == 0 {
        return 0;
    }
    let mut h = term_h.saturating_sub(16);
    if h > 30 {
        h = 30;
    }
    let floor = if term_h < 20 { 4 } else { 6 };
    if h < floor {
        h = floor;
    }
    h
}

fn show_cover(has_cover: bool, term_h: usize) -> bool {
    if !has_cover {
        return false;
    }
    !(term_h > 0 && term_h < 20) // mismo gate que Go: tiny esconde cover
}


// Artwork del `get` completo (con carátula). Solo lo llama el worker de
// carátula cuando el stream no trajo artworkData.
fn fetch_art_b64() -> Option<String> {
    let out = mc(&["get"])?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let b64 = v.get("artworkData").and_then(|x| x.as_str()).unwrap_or("");
    if b64.is_empty() {
        return None;
    }
    Some(b64.to_string())
}

// ---- background: stream reactivo + workers (nada bloquea el hilo UI) ----

struct StreamUpdate {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    elapsed: Option<f64>,
    playing: Option<bool>,
    artwork: Option<String>,
    src: String, // "music" | "podcast" | "" — "other" se descarta en el hilo
}

enum BgMsg {
    Update(StreamUpdate),
    CoverReady { key: String, img: image::DynamicImage, fresh: bool },
    LyricsReady { key: String, items: Vec<Lyric> },
    MetaReady { key: String, fav: bool, shuffle: bool, repeat: String },
    StreamAlive,
    StreamDead,
}

struct ArtJob {
    key: String,
    b64: Option<String>, // None => el worker hace `get` (lento, en background)
}

// Experimental --cover viuer: la técnica de viuer (half-block inferior ▄,
// fg=píxel abajo bg=píxel arriba, resize con image) en vez de chafa.
// Mismo marco (indent 2, ANSI) para comparar el LOOK 1:1.

// (fetch de letras: hilos sueltos por track, ver spawn_lyr_fetch)

// srcOf igual que Go stream.go: Safari & co => "other" (se descarta).
fn src_of(p: &serde_json::Value) -> String {
    if let Some(b) = p.get("bundleIdentifier").and_then(|x| x.as_str()) {
        if !b.is_empty() {
            match b {
                "com.apple.Music" => return "music".to_string(),
                "com.apple.podcasts" => return "podcast".to_string(),
                _ => return "other".to_string(),
            }
        }
    }
    if p.get("isMusicApp").and_then(|x| x.as_bool()).unwrap_or(false) {
        return "music".to_string();
    }
    if let Some(m) = p.get("mediaType").and_then(|x| x.as_str()) {
        let ml = m.to_lowercase();
        if ml.contains("music") {
            return "music".to_string();
        }
        if ml.contains("podcast") {
            return "podcast".to_string();
        }
        return "other".to_string();
    }
    String::new()
}

// merge igual que Go nowInfo.merge: solo campos presentes; avisa si cambió track.
fn merge_update(n: &mut Now, u: &StreamUpdate, at: Instant) -> bool {
    let mut changed = false;
    if let (Some(t), Some(a), Some(al)) = (u.title.clone(), u.artist.clone(), u.album.clone()) {
        let key = format!("{}\x00{}\x00{}", t, a, al);
        if key != n.key() {
            changed = true;
            n.title = t;
            n.artist = a;
            n.album = al;
        }
    }
    if let Some(d) = u.duration {
        n.duration = d;
    }
    if let Some(e) = u.elapsed {
        n.elapsed = e;
        n.at = Some(at); // re-ancla en reportes reales
    }
    if let Some(p) = u.playing {
        n.playing = p;
    }
    changed
}

// Hilo stream: `media-control stream` línea a línea (igual que Go subscribeStream).
// Reintenta solo si muere; avisa Alive/Dead para el fallback de polling.
fn spawn_stream(tx: Sender<BgMsg>) {
    thread::spawn(move || {
        loop {
            let mut child = match Command::new("media-control")
                .arg("stream")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => {
                    let _ = tx.send(BgMsg::StreamDead);
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let _ = tx.send(BgMsg::StreamAlive);
            let died_early = std::time::Instant::now();
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v.get("type").and_then(|t| t.as_str()) != Some("data") {
                        continue;
                    }
                    let p = match v.get("payload") {
                        Some(p) => p,
                        None => continue,
                    };
                    if src_of(p) == "other" {
                        continue; // Safari & co: nunca se muestran
                    }
                    dlog!("stream data keys={:?}", p.as_object().map(|o| o.keys().collect::<Vec<_>>()));
                    let opt_str = |k: &str| {
                        p.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
                    };
                    let art = opt_str("artworkData").filter(|s| !s.is_empty());
                    let _ = tx.send(BgMsg::Update(StreamUpdate {
                        title: opt_str("title"),
                        artist: opt_str("artist"),
                        album: opt_str("album"),
                        duration: p.get("duration").and_then(|x| x.as_f64()),
                        elapsed: p.get("elapsedTime").and_then(|x| x.as_f64()),
                        playing: p.get("playing").and_then(|x| x.as_bool()),
                        artwork: art,
                        src: src_of(p),
                    }));
                }
            }
            let _ = child.wait();
            // Si murió al instante, espera antes de reintentar (binario roto?).
            if died_early.elapsed() < Duration::from_secs(2) {
                let _ = tx.send(BgMsg::StreamDead);
                thread::sleep(Duration::from_secs(5));
            } else {
                thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

// Worker carátula: decode + chafa fuera del hilo UI. Latest-wins en resize.
// Caché del render por (track,w,h): tu Go re-horquilla chafa en cada resize.
// Worker carátula (rama exp): decode a DynamicImage fuera del hilo UI.
// El widget (StatefulImage) adapta tamaño solo en el render: sin chafa,
// sin caché en disco, sin re-render en resize.
fn spawn_cover_worker(rx: Receiver<ArtJob>, tx: Sender<BgMsg>) {
    thread::spawn(move || {
        while let Ok(mut job) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                job = newer;
            }
            let had_b64 = matches!(&job.b64, Some(b) if !b.is_empty());
            let b64 = match job.b64 {
                Some(b) if !b.is_empty() => b,
                _ => {
                    let t0 = Instant::now();
                    let r = fetch_art_b64();
                    dlog!("cover job {}: get {}ms", job.key.replace('\0', "|"), t0.elapsed().as_millis());
                    match r {
                        Some(b) => b,
                        None => continue,
                    }
                }
            };
            let t0 = Instant::now();
            let raw =
                match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64) {
                    Ok(r) if !r.is_empty() => r,
                    _ => continue,
                };
            let img = match image::load_from_memory(&raw) {
                Ok(i) => i,
                Err(_) => continue,
            };
            dlog!("cover job {}: decode {}ms", job.key.replace('\0', "|"), t0.elapsed().as_millis());
            let _ = tx.send(BgMsg::CoverReady { key: job.key, img, fresh: had_b64 });
        }
    });
}

// Worker meta (★/shuffle/repeat): un solo osascript en background.
// Tu Go lo pide por track (favCmd); acá no toca el path crítico.
fn fetch_meta() -> Option<(bool, bool, String)> {
    let out = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"Music\" to (favorited of current track as string) & \"||\" & (shuffle enabled as string) & \"||\" & (song repeat as string)")
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // sin permiso Automation (-1743): se ignora en silencio
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p: Vec<&str> = s.split("||").collect();
    if p.len() < 3 {
        return None;
    }
    Some((
        p[0].to_lowercase() == "true",
        p[1].to_lowercase() == "true",
        p[2].to_lowercase(),
    ))
}

// Worker letras: curl LRCLIB fuera del hilo UI. Luego meta (★/shuffle/repeat)
// en el mismo worker para no sumar hilos ni tocar el path crítico.
// Un hilo por track (como tu Go tea.Cmd por fetch): sin cola, sin
// head-of-line blocking al saltar temas; lo viejo se descarta por key.
fn spawn_lyr_fetch(key: String, artist: String, title: String, album: String, duration: f64, tx: Sender<BgMsg>) {
    thread::spawn(move || {
        let t0 = Instant::now();
        let items = fetch_lyrics(&artist, &title, &album, duration);
        if !items.is_empty() {
            dlog!("letras fetch {}ms", t0.elapsed().as_millis());
            let _ = tx.send(BgMsg::LyricsReady { key: key.clone(), items });
        }
        if let Some((fav, sh, rp)) = fetch_meta() {
            let _ = tx.send(BgMsg::MetaReady { key, fav, shuffle: sh, repeat: rp });
        }
    });
}

// Poll reactivo sin osascript: `media-control get --no-artwork` no pide
// permiso -1743 y es lo mismo que usa music-cli para el hero.
fn fetch_now() -> Option<Now> {
    let out = mc(&["get", "--no-artwork"])?;
    if out.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    // Solo Music en fase 1 (podcasts en fase 2, igual que Go srcOf).
    let bundle = get_str(&v, "bundleIdentifier");
    if bundle != "com.apple.Music" {
        return None;
    }
    let title = get_str(&v, "title");
    if title.is_empty() {
        return None;
    }
    let playing = v.get("playing").and_then(|x| x.as_bool()).unwrap_or(false);
    let duration = v.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
    // elapsedTime viene anclado a timestamp; extrapolamos como Go position().
    let elapsed = v.get("elapsedTime").and_then(|x| x.as_f64()).unwrap_or(0.0);
    Some(Now {
        title,
        artist: get_str(&v, "artist"),
        album: get_str(&v, "album"),
        duration,
        elapsed,
        at: Some(Instant::now()),
        playing,
        shuffle: false, // fase 1: no bloqueamos por esto
        repeat: "off".to_string(),
        fav: false,
        track_since: None, // lo fija el caller al detectar cambio de track
        src: "music".to_string(), // fetch_now solo acepta com.apple.Music
    })
}

// ---- render: mismas líneas que Go nowLines() ----


fn center_text(s: &str, w: usize) -> String {
    let r = s.chars().count();
    if r >= w {
        return s.to_string();
    }
    " ".repeat((w - r) / 2) + s
}

fn lyric_lines(lyrics: &[Lyric], pos: f64, playing: bool, w: usize, term_h: usize) -> Vec<Line<'static>> {
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

fn control_icons(n: &Now) -> (Vec<String>, Vec<String>) {
    let play = if n.playing { "⏸" } else { "▶" };
    let mut rp = "↻".to_string();
    if n.repeat == "one" {
        rp = "↻¹".to_string();
    }
    let icons = vec![
        "⇄".to_string(),
        "⏮".to_string(),
        play.to_string(),
        "⏭".to_string(),
        rp,
    ];
    let keys = vec!["s", "p", "␣", "n", "r"]
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    (icons, keys)
}




fn print_usage() {
    eprintln!("musik — Apple Music en tu terminal (q sale)");
    eprintln!("  musik [--cover viuer]   TUI");
    eprintln!("  musik play | pause | toggle | next | prev | version");
}

fn main() -> io::Result<()> {
    // Subcomandos primero (sin TUI): iguales que music-cli, vía media-control.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cmd) = args.iter().find(|a| !a.starts_with('-') && *a != "viuer") {
        match cmd.as_str() {
            "version" | "--version" | "-v" => {
                println!("musik {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "play" => { mc_control(&["play"]); return Ok(()); }
            "pause" => { mc_control(&["pause"]); return Ok(()); }
            "toggle" => { mc_control(&["toggle-play-pause"]); return Ok(()); }
            "next" => { mc_control(&["next-track"]); return Ok(()); }
            "prev" | "previous" => { mc_control(&["previous-track"]); return Ok(()); }
            _ => {
                print_usage();
                std::process::exit(1);
            }
        }
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    // Canales background (nada bloquea el hilo UI, igual que Go tea.Cmd).
    let (bg_tx, bg_rx) = mpsc::channel::<BgMsg>();
    spawn_stream(bg_tx.clone());
    let (art_tx, art_rx) = mpsc::channel::<ArtJob>();
    spawn_cover_worker(art_rx, bg_tx.clone());


    let mut state = Now::default();
    let mut last_poll = Instant::now();
    let mut use_poll = true; // hasta que el stream confirme (StreamAlive)
    // Scoreboard de latencia por track (MUSIK_DEBUG=1): cover vs letras.
    let mut t_change: Option<Instant> = None;
    let mut t_cover_logged = false;
    let mut t_lyr_logged = false;
    // Tras un toggle del usuario se confía en local 1.5s: el remoto (stream
    // en fade o poll viejo) no puede devolver el icono. Sin esto se ve lag.
    let mut play_hold: Option<Instant> = None;
    // Rama exp: carátula foto (ratatui-image, protocolo auto) en vez de chafa.
    // Picker una vez al arranque (pregunta al terminal, con fallback).
    let picker = Picker::from_query_stdio()
        .unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
    let mut cover_img: Option<(String, DynamicImage)> = None; // (key, imagen)
    let mut cover_proto: Option<StatefulProtocol> = None; // render del área
    let mut cover_key = String::new(); // key de la carátula MOSTRADA
    let mut cover_fresh = false; // true si lo mostrado viene del stream de este track
    // Arte pendiente: el diff de artwork suele llegar ms después del cambio.
    // Se espera (tope 800ms) en vez de disparar `get` que puede traer lo viejo.
    let mut pending_art = false;
    let mut art_deadline: Option<Instant> = None;
    let mut lyrics: Vec<Lyric> = vec![];
    let mut lyrics_on = true;

    // Estado inicial instantáneo (el stream solo manda diffs al cambiar algo).
    if let Some(mut fresh) = fetch_now() {
        fresh.track_since = Some(Instant::now());
        let key = fresh.key();
        state = fresh;
        let _ = art_tx.send(ArtJob { key: key.clone(), b64: None });
        spawn_lyr_fetch(key, state.artist.clone(), state.title.clone(), state.album.clone(), state.duration, bg_tx.clone());
    }

    // Pide letras ya; carátula: si el diff trajo artwork va al instante,
    // si no se espera el diff de artwork (tope 800ms) antes del fallback `get`.
    // Letras+meta al cambio: hilos sueltos (sin cola).
    let request_lyr = |key: &str, st: &Now, tx: &Sender<BgMsg>| {
        spawn_lyr_fetch(key.to_string(), st.artist.clone(), st.title.clone(), st.album.clone(), st.duration, tx.clone());
    };
    let send_art = |key: &str, b64: &Option<String>, art_tx: &Sender<ArtJob>| {
        let _ = art_tx.send(ArtJob { key: key.to_string(), b64: b64.clone() });
    };

    loop {
        let w = term.size()?.width as usize;
        let h = term.size()?.height as usize;
        // Drena eventos background sin bloquear.
        while let Ok(msg) = bg_rx.try_recv() {
            match msg {
                BgMsg::StreamAlive => use_poll = false,
                BgMsg::StreamDead => use_poll = true,
                BgMsg::Update(u) => {
                    let at = Instant::now();
                    dlog!(
                        "update t={:?} a={:?} al={:?} el={:?} pl={:?} art={}",
                        u.title, u.artist, u.album, u.elapsed, u.playing,
                        u.artwork.as_ref().map(|s| s.len()).unwrap_or(0)
                    );
                    if !u.src.is_empty() {
                        state.src = u.src.clone();
                    }
                    let hold = matches!(play_hold, Some(dl) if at < dl);
                    let local_playing = state.playing;
                    if merge_update(&mut state, &u, at) {
                        dlog!("TRACK CHANGED -> {}", state.title);
                        state.track_since = Some(at);
                        t_change = Some(at);
                        t_cover_logged = false;
                        t_lyr_logged = false;
                        let key = state.key();
                        // La anterior se queda (sin hueco, igual que Go).
                        // cover_key sigue apuntando a lo MOSTRADO hasta el Ready.
                        lyrics.clear();
                        state.fav = false; // el worker trae el real (igual que Go)
                        cover_fresh = false;
                        request_lyr(&key, &state, &bg_tx);
                        if let Some(b) = u.artwork.filter(|s| !s.is_empty()) {
                            // artwork junto al cambio: al instante
                            pending_art = false;
                            send_art(&key, &Some(b), &art_tx);
                        } else {
                            // sin artwork aún: espera el diff (llega en ms)
                            pending_art = true;
                            art_deadline = Some(at + Duration::from_millis(800));
                        }
                    } else if let Some(b) = u.artwork {
                        // artwork en diff aparte: el stream fresco siempre gana
                        if !b.is_empty() && !state.title.is_empty() && cover_key != state.key() {
                            pending_art = false;
                            let _ = art_tx.send(ArtJob {
                                key: state.key(),
                                b64: Some(b),
                            });
                        }
                    }
                    if hold && state.playing != local_playing {
                        dlog!("hold: remoto viejo no pisa icono");
                        state.playing = local_playing;
                    } else if !hold {
                        play_hold = None;
                    }
                }
                BgMsg::CoverReady { key, img, fresh } => {
                    dlog!("cover ready {} fresh={}", key.replace('\0', "|"), fresh);
                    if key != state.key() || state.title.is_empty() {
                        continue; // de otro track: se descarta
                    }
                    if !fresh && cover_fresh && cover_key == key {
                        continue; // `get` tardío no pisa arte fresco del stream
                    }
                    if !t_cover_logged {
                        if let Some(t0) = t_change {
                            dlog!("SCORE cover lista en {}ms", t0.elapsed().as_millis());
                        }
                        t_cover_logged = true;
                    }
                    // El protocolo se adapta solo al área: sin re-renders.
                    cover_proto = Some(picker.new_resize_protocol(img.clone()));
                    cover_img = Some((key.clone(), img));
                    cover_key = key;
                    cover_fresh = fresh;
                }
                BgMsg::LyricsReady { key, items } => {
                    if key == state.key() {
                        if !t_lyr_logged {
                            if let Some(t0) = t_change {
                                dlog!("SCORE letras listas en {}ms", t0.elapsed().as_millis());
                            }
                            t_lyr_logged = true;
                        }
                        lyrics = items;
                    }
                }
                BgMsg::MetaReady { key, fav, shuffle, repeat } => {
                    if key == state.key() {
                        state.fav = fav;
                        state.shuffle = shuffle;
                        state.repeat = repeat;
                    }
                }
            }
        }

        // Fallback polling solo si el stream no va (binario ausente/roto).
        // Con stream vivo esto NUNCA corre: cero polling (ver MUSIK_DEBUG).
        if use_poll && last_poll.elapsed() > Duration::from_secs(1) {
            dlog!("poll fallback (stream muerto)");
            last_poll = Instant::now();
            if let Some(mut fresh) = fetch_now() {
                let changed = fresh.key() != state.key() || state.title.is_empty();
                if changed {
                    fresh.track_since = Some(Instant::now());
                } else {
                    fresh.track_since = state.track_since;
                }
                // El hold también vale contra el poll viejo.
                if matches!(play_hold, Some(dl) if Instant::now() < dl) {
                    fresh.playing = state.playing;
                } else {
                    play_hold = None;
                }
                state = fresh;
                if changed && !state.title.is_empty() {
                    let key = state.key();
                    // Igual que stream: anterior visible, letras ya, arte con
                    // tope (el poll no trae artwork: va directo a fallback).
                    lyrics.clear();
                    state.fav = false;
                    cover_fresh = false;
                    request_lyr(&key, &state, &bg_tx);
                    pending_art = true;
                    art_deadline = Some(Instant::now() + Duration::from_millis(800));
                }
            } else if !state.title.is_empty() && state.playing {
                state = Now::default();
            }
        }

        // Tope de espera del diff de artwork: fallback `get` una sola vez.
        // Resize: nada que hacer, el protocolo se adapta solo al área.
        if pending_art {
            if let Some(dl) = art_deadline {
                if Instant::now() >= dl {
                    pending_art = false;
                    if !state.title.is_empty() {
                        dlog!("art timeout, fallback get");
                        let _ = art_tx.send(ArtJob { key: state.key(), b64: None });
                    }
                }
            } else {
                pending_art = false;
            }
        }
        // ---- vista Layout (rama exp): rects únicos para vista y mouse.
        let cw_w = cw(w);
        let content_w = cw_w;
        let show_cov = show_cover(cover_img.is_some(), h);
        let cov_h = if show_cov { cover_rows(h) } else { 0 };
        let has_bar = state.duration > 0.0;
        let ll: Vec<Line> = if !state.title.is_empty() && lyrics_on {
            lyric_lines(&lyrics, state.position(), state.playing, cw_w.saturating_sub(2), h)
        } else {
            vec![]
        };
        let lyrics_shown = !ll.is_empty();
        let zone_n = if lyrics_shown { zone_rows(h) } else { 0 };
        // Filas espejo de Go: cover+blanco?, título, meta, gauge+tiempos?,
        // blanco, controles, keys, blancos2?, zona?
        let mut row_cons: Vec<Constraint> = vec![];
        let mut r_cover: Option<usize> = None;
        let (mut r_title, mut r_meta) = (0, 0);
        let (mut r_gauge, mut r_times) = (None, None);
        let (mut r_ctrl, mut r_keys) = (0, 0);
        let mut r_lyr = None;
        macro_rules! row {
            ($c:expr) => {{
                let idx = row_cons.len();
                row_cons.push($c);
                idx
            }};
        }
        if show_cov {
            r_cover = Some(row!(Constraint::Length(cov_h as u16)));
            row!(Constraint::Length(1)); // blanco tras cover
        }
        if state.title.is_empty() {
            r_title = row!(Constraint::Length(1)); // "waiting…" reutiliza fila título
        } else {
            r_meta = row!(Constraint::Length(1));
            if has_bar {
                r_gauge = Some(row!(Constraint::Length(1)));
                r_times = Some(row!(Constraint::Length(1)));
            }
            row!(Constraint::Length(1)); // blanco
            r_ctrl = row!(Constraint::Length(1));
            r_keys = row!(Constraint::Length(1));
            if lyrics_shown {
                row!(Constraint::Length(1));
                row!(Constraint::Length(1));
                r_lyr = Some(row!(Constraint::Length(zone_n as u16)));
            }
        }
        let total: u16 = row_cons.iter().map(|c| match c {
            Constraint::Length(n) => *n,
            _ => 0,
        }).sum();
        let frame_rect = Rect::new(0, 0, w.min(u16::MAX as usize) as u16, h.min(u16::MAX as usize) as u16);
        let v = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(total),
            Constraint::Fill(1),
        ])
        .split(frame_rect);
        let hz = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(content_w.min(w) as u16),
            Constraint::Fill(1),
        ])
        .split(v[1]);
        let content = hz[1];
        let rows = Layout::vertical(row_cons).split(content);
        let title_r = rows[r_title];
        let gauge_rect: Option<Rect> = r_gauge.map(|i| rows[i]);
        let lyr_rect: Option<Rect> = r_lyr.map(|i| rows[i]);
        let ctrl_cells: Vec<Rect> = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ])
        .split(rows[r_ctrl])
        .to_vec();
        let keys_cells: Vec<Rect> = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ])
        .split(rows[r_keys])
        .to_vec();

        term.draw(|f| {
            if state.title.is_empty() {
                f.render_widget(
                    Paragraph::new("waiting for the Music app… play something ♪")
                        .alignment(Alignment::Center)
                        .style(dim_style()),
                    title_r,
                );
                return;
            }
            if let Some(ci) = r_cover {
                if let Some(proto) = cover_proto.as_mut() {
                    f.render_stateful_widget(StatefulImage::default(), rows[ci], proto);
                }
            }
            let now = Instant::now();
            let since = state.track_since.unwrap_or(now);
            let title_spans: Vec<Span> = if state.fav {
                vec![Span::raw(format!("{} ★", marquee(&state.title, cw_w, now, since)))]
            } else if state.src != "podcast" {
                vec![
                    Span::raw(marquee(&state.title, cw_w, now, since)),
                    Span::styled(" ☆", dim_style()),
                ]
            } else {
                vec![Span::raw(marquee(&state.title, cw_w, now, since))]
            };
            f.render_widget(
                Paragraph::new(Line::from(title_spans)).alignment(Alignment::Center),
                title_r,
            );
            f.render_widget(
                Paragraph::new(marquee(&state.meta(), cw_w, now, since))
                    .alignment(Alignment::Center)
                    .style(dim_style()),
                rows[r_meta],
            );
            if let (Some(gi), Some(ti)) = (r_gauge, r_times) {
                let mut ratio = state.position() / state.duration;
                ratio = ratio.clamp(0.0, 1.0);
                f.render_widget(
                    Gauge::default()
                        .ratio(ratio)
                        .gauge_style(Style::default())
                        .use_unicode(true),
                    rows[gi],
                );
                let left = fmt_time(state.position());
                let mut remain = state.duration - state.position();
                if remain < 0.0 {
                    remain = 0.0;
                }
                let right = format!("-{}", fmt_time(remain));
                f.render_widget(
                    Paragraph::new(format!("{}  {}", left, right))
                        .alignment(Alignment::Center)
                        .style(dim_style()),
                    rows[ti],
                );
            }
            let (icons, keys) = control_icons(&state);
            let dim = dim_style();
            let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
            for (i, cell) in ctrl_cells.iter().enumerate() {
                let mut st = Style::default();
                if i == 0 {
                    st = if state.shuffle { bold } else { dim };
                } else if i == 4 {
                    st = if state.repeat != "" && state.repeat != "off" {
                        bold
                    } else {
                        dim
                    };
                }
                f.render_widget(
                    Paragraph::new(icons[i].clone()).alignment(Alignment::Center).style(st),
                    *cell,
                );
            }
            for (i, cell) in keys_cells.iter().enumerate() {
                f.render_widget(
                    Paragraph::new(keys[i].clone()).alignment(Alignment::Center).style(dim),
                    *cell,
                );
            }
            if let Some(li) = r_lyr {
                f.render_widget(Paragraph::new(ll.clone()), rows[li]);
                let cur = lyric_current(&lyrics, state.position()).max(0) as usize;
                let mut sb = ScrollbarState::new(lyrics.len().max(1)).position(cur);
                f.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight),
                    rows[li],
                    &mut sb,
                );
            }
        })?;

        // Tick: 100ms si hay marquee o si suena (barra fluida a 10fps;
        // tu Go usa 500ms sonando — acá se ve más vivo), 2s quieto.
        let tick = if marquee_live(&state, cw(w)) {
            Duration::from_millis(100)
        } else if state.playing {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(2)
        };
        if event::poll(tick)? {
            match event::read()? {
                // Resize: redraw inmediato en la próxima vuelta (el size se
                // relee arriba y el cover se re-renderiza con want_w/want_h).
                Event::Resize(_, _) => {}
                Event::Mouse(m) => {
                    // Misma geometría que la vista: los Rects del Layout.
                    // Solo botón izquierdo.
                    use ratatui::layout::Position;
                    if m.kind != MouseEventKind::Down(MouseButton::Left) {
                        continue;
                    }
                    if state.title.is_empty() {
                        continue;
                    }
                    let pos = Position::new(m.column, m.row);
                    // Título: alterna ★/☆.
                    if title_r.contains(pos) {
                        if state.src != "podcast" {
                            state.fav = !state.fav;
                            if state.fav {
                                osa_control("tell application \"Music\" to set favorited of current track to true");
                            } else {
                                osa_control("tell application \"Music\" to set favorited of current track to false");
                            }
                        }
                        continue;
                    }
                    // Gauge: seek (optimista, confirma stream).
                    if let Some(gr) = gauge_rect {
                        if gr.contains(pos) {
                            let ratio = if gr.width > 0 {
                                (pos.x.saturating_sub(gr.x) as f64) / gr.width as f64
                            } else {
                                0.0
                            };
                            let pos = ratio.clamp(0.0, 1.0) * state.duration;
                            state.elapsed = pos;
                            state.at = Some(Instant::now());
                            transport_seek(state.src == "podcast", pos);
                            continue;
                        }
                    }
                    // Controles (ambas filas, igual que Go).
                    let mut done = false;
                    for (s, cell) in ctrl_cells.iter().chain(keys_cells.iter()).enumerate() {
                        if !cell.contains(pos) {
                            continue;
                        }
                        done = true;
                        match s % 5 {
                            0 => {
                                if state.src == "podcast" {
                                    break;
                                }
                                state.shuffle = !state.shuffle;
                                transport_shuffle(state.shuffle);
                            }
                            1 => {
                                transport_prev(state.src == "podcast");
                                state.elapsed = 0.0;
                                state.at = Some(Instant::now());
                                state.track_since = Some(Instant::now());
                            }
                            2 => {
                                let to = !state.playing;
                                transport_pp(state.src == "podcast", to);
                                state.playing = to;
                                state.at = Some(Instant::now());
                                state.elapsed = state.position();
                                play_hold = Some(Instant::now() + Duration::from_millis(1500));
                            }
                            3 => {
                                transport_next(state.src == "podcast");
                                state.elapsed = 0.0;
                                state.at = Some(Instant::now());
                                state.track_since = Some(Instant::now());
                            }
                            _ => {
                                if state.src == "podcast" {
                                    break;
                                }
                                state.repeat_cycle();
                                transport_repeat(&state.repeat);
                            }
                        }
                        break;
                    }
                    if done {
                        continue;
                    }
                    // Verso: salta ahí.
                    if let Some(lr) = lyr_rect {
                        if lr.contains(pos) && lyrics_shown {
                            let vis = lyric_visual(&lyrics, state.position(), cw_w.saturating_sub(2), h);
                            let row = pos.y.saturating_sub(lr.y).saturating_sub(2) as usize;
                            if row < vis.len() {
                                let sec = lyrics[vis[row].idx].sec + 0.01;
                                state.elapsed = sec;
                                state.at = Some(Instant::now());
                                transport_seek(state.src == "podcast", sec);
                            }
                        }
                    }
                }
                Event::Key(k) => {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('f') => {
                        // Favorito ★/☆ (igual que Go; podcasts no tienen).
                        if state.src == "podcast" {
                            continue;
                        }
                        state.fav = !state.fav;
                        if state.fav {
                            osa_control("tell application \"Music\" to set favorited of current track to true");
                        } else {
                            osa_control("tell application \"Music\" to set favorited of current track to false");
                        }
                    }
                    KeyCode::Char('l') => {
                        lyrics_on = !lyrics_on; // igual que Go: l oculta/muestra letras
                    }
                    KeyCode::Char(' ') => {
                        let to = !state.playing;
                        transport_pp(state.src == "podcast", to);
                        state.playing = to;
                        state.at = Some(Instant::now());
                        state.elapsed = state.position();
                        play_hold = Some(Instant::now() + Duration::from_millis(1500));
                    }
                    KeyCode::Char('n') => {
                        transport_next(state.src == "podcast");
                        state.elapsed = 0.0;
                        state.at = Some(Instant::now());
                        state.track_since = Some(Instant::now());
                    }
                    KeyCode::Char('p') => {
                        transport_prev(state.src == "podcast");
                        state.elapsed = 0.0;
                        state.at = Some(Instant::now());
                        state.track_since = Some(Instant::now());
                    }
                    KeyCode::Char('s') => {
                        if state.src == "podcast" {
                            continue; // shuffle es de Music (igual que Go)
                        }
                        state.shuffle = !state.shuffle;
                        transport_shuffle(state.shuffle);
                    }
                    KeyCode::Char('r') => {
                        if state.src == "podcast" {
                            continue; // repeat es de Music (igual que Go)
                        }
                        state.repeat_cycle(); // igual que Go: off->one->all
                        transport_repeat(&state.repeat);
                    }
                    _ => {}
                }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fmt_time_ok() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(65.0), "1:05");
    }
    #[test]
    fn marquee_short() {
        let t = Instant::now();
        assert_eq!(marquee("hola", 10, t, t), "hola");
    }
    #[test]
    fn cw_floor() {
        assert_eq!(cw(100), 60);
        assert!(cw(20) >= 16);
    }
    #[test]
    fn zone_rows_parity() {
        // igual que Go TestZoneRows: tiny 1, short 3, full 9
        assert_eq!(zone_rows(0), LYRIC_ROWS);
        assert_eq!(zone_rows(15), 1);
        assert_eq!(zone_rows(30), 3);
        assert_eq!(zone_rows(60), LYRIC_ROWS);
    }
    #[test]
    fn show_cover_tiny() {
        assert!(!show_cover(true, 15));
        assert!(show_cover(true, 30));
        assert!(!show_cover(false, 40));
    }
}

