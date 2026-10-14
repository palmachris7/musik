use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::lyrics::fetch_lyrics;
use crate::state::{ArtJob, BgMsg, Now, StreamUpdate};

pub(crate) fn mc(args: &[&str]) -> Option<String> {
    let out = Command::new("media-control").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn mc_control(args: &[&str]) {
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

pub(crate) fn osa_control(script: &str) {
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub(crate) fn music_ctl(script: &str, mc_args: &[&str]) {
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

pub(crate) fn transport_pp(podcast: bool, to_playing: bool) {
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

pub(crate) fn transport_next(podcast: bool) {
    if podcast {
        mc_control(&["next-track"]);
    } else {
        music_ctl("tell application \"Music\" to next track", &["next-track"]);
    }
}

pub(crate) fn transport_prev(podcast: bool) {
    if podcast {
        mc_control(&["previous-track"]);
    } else {
        music_ctl("tell application \"Music\" to previous track", &["previous-track"]);
    }
}

pub(crate) fn transport_seek(podcast: bool, pos: f64) {
    if podcast {
        mc_control(&["seek", &format!("{:.1}", pos)]);
    } else {
        music_ctl(
            &format!("tell application \"Music\" to set player position to {}", pos),
            &["seek", &format!("{:.1}", pos)],
        );
    }
}

pub(crate) fn transport_shuffle(on: bool) {
    music_ctl(
        &format!("tell application \"Music\" to set shuffle enabled to {}", if on { "true" } else { "false" }),
        &["toggle-shuffle"],
    );
}

pub(crate) fn transport_repeat(mode: &str) {
    music_ctl(
        &format!("tell application \"Music\" to set song repeat to {}", mode),
        &["toggle-repeat"],
    );
}

// osascript sin espera (favoritos/volumen): best-effort, como Go playCmd.

pub(crate) fn get_str(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

// ---- carátula idéntica a Go (cover.go): media-control artworkData -> tmp jpg -> chafa ----

pub(crate) fn fetch_now() -> Option<Now> {
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

pub(crate) fn fetch_meta() -> Option<(bool, bool, String)> {
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

pub(crate) fn fetch_art_b64() -> Option<String> {
    let out = mc(&["get"])?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let b64 = v.get("artworkData").and_then(|x| x.as_str()).unwrap_or("");
    if b64.is_empty() {
        return None;
    }
    Some(b64.to_string())
}

// ---- background: stream reactivo + workers (nada bloquea el hilo UI) ----

pub(crate) fn src_of(p: &serde_json::Value) -> String {
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

pub(crate) fn merge_update(n: &mut Now, u: &StreamUpdate, at: Instant) -> bool {
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

pub(crate) fn spawn_stream(tx: Sender<BgMsg>) {
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

pub(crate) fn spawn_cover_worker(rx: Receiver<ArtJob>, tx: Sender<BgMsg>) {
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

pub(crate) fn spawn_lyr_fetch(key: String, artist: String, title: String, album: String, duration: f64, tx: Sender<BgMsg>) {
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

// ---- finder biblioteca (igual que Go SearchLibrary/PlayTrackInLibrary) ----
fn esc(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"") }

pub(crate) fn search_library(q: &str) -> Vec<crate::finder::Found> {
    // Un solo osascript como Go SearchLibrary (el doble call bloqueaba el UI por tecla).
    let script = format!("tell application \"Music\"\nset rs to search playlist \"Library\" for \"{}\"\nset o to \"\"\nset i to 0\nrepeat with t in rs\nset o to o & name of t & \"||\" & artist of t & \"||\" & album of t & \"\\n\"\nset i to i + 1\nif i >= 30 then exit repeat\nend repeat\nreturn o\nend tell", esc(q));
    let out = match Command::new("osascript").arg("-e").arg(&script).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return vec![],
    };
    out.lines().filter_map(|l| {
        let p: Vec<&str> = l.split("||").collect();
        if p.is_empty() || p[0].trim().is_empty() { return None; }
        Some(crate::finder::Found { name: p[0].trim().to_string(), artist: p.get(1).unwrap_or(&"").trim().to_string(), album: p.get(2).unwrap_or(&"").trim().to_string() })
    }).collect()
}

pub(crate) fn play_catalog_song(s: &crate::catalog::Song) {
    // Igual que Go playCatalogSongCmd: match biblioteca → shortcut
    // headless music-cli-play → abrir URL en Music como último recurso.
    let q = format!("{} {}", s.title, s.artist);
    if !q.trim().is_empty() && play_first_match(&q) { return; } // PlayFirstLibraryMatch idéntico a Go
    if shortcut_exists("music-cli-play") {
        let before = fetch_now().map(|n| (n.title, n.playing));
        let _ = std::process::Command::new("shortcuts").args(["run", "music-cli-play", "-i", &q]).output();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let after = fetch_now().map(|n| (n.title, n.playing));
        if before != after { return; } // algo empezó a sonar: listo
    }
    if !s.url.is_empty() {
        let _ = std::process::Command::new("open").args(["-a", "Music", &s.url]).spawn();
    }
}
fn shortcut_exists(name: &str) -> bool {
    Command::new("shortcuts").arg("list").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == name))
        .unwrap_or(false)
}

// ---- índice biblioteca (álbumes/artistas/playlists), caché por sesión ----
static INDEX: std::sync::OnceLock<std::sync::Mutex<Option<(Vec<String>, Vec<String>, Vec<String>)>>> = std::sync::OnceLock::new();

pub(crate) fn kick_index() {
    let m = INDEX.get_or_init(|| std::sync::Mutex::new(None));
    if m.lock().ok().and_then(|g| g.clone()).is_some() { return; }
    std::thread::spawn(|| {
        let albums = osa_list("album", "||");
        let artists = osa_list("artist", "||");
        let playlists = osa_playlists();
        if let Some(m) = INDEX.get() { *m.lock().unwrap() = Some((albums, artists, playlists)); }
    });
}
fn osa_out(script: &str) -> String {
    Command::new("osascript").arg("-e").arg(script).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
}
fn osa_list(getter: &str, sep: &str) -> Vec<String> {
    // El delimitador debe fijarse DENTRO del script; si no Music devuelve
    // todo pegado con ", " y salía una sola fila gigante.
    let full = format!("tell application \"Music\"\nset AppleScript's text item delimiters to \"{}\"\nset xs to {} of tracks of playlist \"Library\"\nreturn xs as string\nend tell", sep, getter);
    let out = osa_out(&full);
    let mut seen = std::collections::HashSet::new();
    let mut v = vec![];
    for p in out.split(sep) {
        let t = p.trim().to_string();
        if !t.is_empty() && seen.insert(t.clone()) { v.push(t); }
    }
    v
}
fn osa_playlists() -> Vec<String> {
    let out = osa_out("tell application \"Music\"\nset o to \"\"\nrepeat with p in every playlist\nif special kind of p as string is \"none\" then\nset o to o & name of p & \"||\"\nend if\nend repeat\nreturn o\nend tell");
    out.split("||").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}
pub(crate) fn lib_index() -> (Vec<String>, Vec<String>, Vec<String>) {
    INDEX.get().and_then(|m| m.lock().ok()).and_then(|g| g.clone()).unwrap_or_default()
}
pub(crate) fn library_page() -> Vec<crate::finder::Found> { filtered_page("library", "", 0) }
pub(crate) fn filtered_page(kind: &str, name: &str, offset: usize) -> Vec<crate::finder::Found> {
    // GetFilteredTracks idéntico a Go (página 30, con duración ignorada).
    let (scope, pred) = match kind {
        "playlist" => (format!("playlist \"{}\"", esc(name)), String::new()),
        "artist" => ("playlist \"Library\"".to_string(), format!(" whose artist is \"{}\"", esc(name))),
        "album" => ("playlist \"Library\"".to_string(), format!(" whose album is \"{}\"", esc(name))),
        _ => ("playlist \"Library\"".to_string(), String::new()),
    };
    let s = format!("tell application \"Music\"\nset tl to (tracks of {}{})\nset o to \"\"\nset totalCount to count of tl\nset startIdx to {}\nset endIdx to {}\nif startIdx > totalCount then return \"\"\nif endIdx > totalCount then set endIdx to totalCount\nrepeat with i from startIdx to endIdx\nset t to item i of tl\nset o to o & name of t & \"||\" & artist of t & \"||\" & album of t & \"\\n\"\nend repeat\nreturn o\nend tell", scope, pred, offset + 1, offset + 30);
    osa_out(&s).lines().filter_map(|l| {
        let p: Vec<&str> = l.split("||").collect();
        if p.is_empty() || p[0].trim().is_empty() { return None; }
        Some(crate::finder::Found { name: p[0].trim().to_string(), artist: p.get(1).unwrap_or(&"").trim().to_string(), album: p.get(2).unwrap_or(&"").trim().to_string() })
    }).collect()
}
pub(crate) fn context_tracks(kind: &str, name: &str) -> Vec<crate::finder::Found> {
    let pred = match kind { "album" => format!("whose album is \"{}\"", esc(name)), "artist" => format!("whose artist is \"{}\"", esc(name)), _ => String::new() };
    let scope = if kind == "playlist" { format!("playlist \"{}\"", esc(name)) } else { format!("playlist \"Library\" {}", pred) };
    let s = format!("tell application \"Music\"\nset tl to tracks of {}\nset o to \"\"\nrepeat with i from 1 to (count of tl)\nif i > 50 then exit repeat\nset t to item i of tl\nset o to o & name of t & \"||\" & artist of t & \"||\" & album of t & \"\\n\"\nend repeat\nreturn o\nend tell", scope);
    osa_out(&s).lines().filter_map(|l| {
        let p: Vec<&str> = l.split("||").collect();
        if p.is_empty() || p[0].trim().is_empty() { return None; }
        Some(crate::finder::Found { name: p[0].trim().to_string(), artist: p.get(1).unwrap_or(&"").trim().to_string(), album: p.get(2).unwrap_or(&"").trim().to_string() })
    }).collect()
}
pub(crate) fn play_in_context(kind: &str, name: &str, idx1: usize) {
    let s = match kind {
        "playlist" => format!("tell application \"Music\"\nset tl to tracks of playlist \"{}\"\nplay item {} of tl\nend tell", esc(name), idx1),
        "album" => format!("tell application \"Music\"\nset tl to (tracks of playlist \"Library\" whose album is \"{}\")\nplay item {} of tl\nend tell", esc(name), idx1),
        "artist" => format!("tell application \"Music\"\nset tl to (tracks of playlist \"Library\" whose artist is \"{}\")\nplay item {} of tl\nend tell", esc(name), idx1),
        // Library no lleva filtro: antes caía incorrectamente en artist="".
        _ => format!("tell application \"Music\"\nset tl to tracks of playlist \"Library\"\nplay item {} of tl\nend tell", idx1),
    };
    osa_control(&s);
}
pub(crate) fn add_to_playlist(f: &crate::finder::Found, playlist: &str) {
    let s = format!("tell application \"Music\"\nset tl to (tracks of playlist \"Library\" whose name is \"{}\" and artist is \"{}\")\nif (count of tl) > 0 then duplicate item 1 of tl to playlist \"{}\"\nend tell", esc(&f.name), esc(&f.artist), esc(playlist));
    osa_control(&s);
}

// PlayFirstLibraryMatch idéntico a Go: search en library playlist 1 y
// play item 1. Sin match no toca Music (el tell con error no abre nada).
fn play_first_match(q: &str) -> bool {
    let s = format!("tell application \"Music\"\nset rs to (search (library playlist 1) for \"{}\")\nif (count of rs) is 0 then error \"NO_MATCH\"\nplay item 1 of rs\nend tell", esc(q));
    Command::new("osascript").arg("-e").arg(&s).output().map(|o| o.status.success()).unwrap_or(false)
}

pub(crate) fn play_library(f: &crate::finder::Found) {
    // Igual que Go PlayTrackInLibrary: match exacto nombre+artista+álbum
    // (el search por texto abría Music sin sonar cuando no matcheaba).
    let s = format!("tell application \"Music\"\nset tl to (tracks of playlist \"Library\" whose name is \"{}\" and artist is \"{}\" and album is \"{}\")\nif (count of tl) = 0 then set tl to (tracks of playlist \"Library\" whose name is \"{}\")\nif (count of tl) > 0 then play item 1 of tl\nend tell", esc(&f.name), esc(&f.artist), esc(&f.album), esc(&f.name));
    osa_control(&s);
}

// Poll reactivo sin osascript: `media-control get --no-artwork` no pide
// permiso -1743 y es lo mismo que usa music-cli para el hero.
