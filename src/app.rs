use std::io;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind},
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
    widgets::Paragraph,
};
use ratatui_image::{Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol};

use crate::finder::Found;
use crate::lyrics::{lyric_lines, lyric_visual, zone_rows};
use crate::media::*;
use crate::state::*;
use crate::view::*;

// Fila del finder: entrada de índice o track (el render y el Enter la construyen igual).
pub(crate) enum Rowpick { Album(String), Artist(String), Playlist(String), Page(usize), Track(usize) }

// Prefs persistentes: el menú guarda, el arranque restaura.
// (ponytail: JSON plano en ~/.config/musik/prefs.json, sin dependencias nuevas.)
fn prefs_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config/musik/prefs.json"))
}
fn save_prefs(lyrics_on: bool, cover_on: bool, cover_mode: CoverMode) {
    let Some(p) = prefs_path() else { return; };
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let mode = if cover_mode == CoverMode::Foto { "foto" } else { "halfblocks" };
    let _ = std::fs::write(&p, format!("{{\"lyrics\":{lyrics_on},\"cover\":{cover_on},\"mode\":\"{mode}\"}}"));
}
fn load_prefs() -> Option<(bool, bool, CoverMode)> {
    let raw = std::fs::read_to_string(prefs_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some((
        v.get("lyrics").and_then(|x| x.as_bool()).unwrap_or(true),
        v.get("cover").and_then(|x| x.as_bool()).unwrap_or(true),
        if v.get("mode").and_then(|x| x.as_str()).unwrap_or("foto") == "halfblocks" { CoverMode::Halfblocks } else { CoverMode::Foto },
    ))
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CoverMode {
    Foto,
    Halfblocks,
}

pub(crate) fn run(cover_mode: CoverMode, hidpi: f64) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    // Canales background (nada bloquea el hilo UI, igual que Go tea.Cmd).
    let (bg_tx, bg_rx) = mpsc::channel::<BgMsg>();
    spawn_stream(bg_tx.clone());
    spawn_music_watch(bg_tx.clone()); // reactivo Music fino + podcast genérico via stream
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
    // Rama exp: dos formas de cover. `foto` (auto) o `halfblocks` (texto).
    // HiDPI: el bitmap se dimensiona con la fuente; en Retina va x2 para
    // llenar las celdas (el centrado usa ratio, inmune al DPI).
    // V alterna el modo en vivo (igual que c/l ocultan).
    let mut cover_mode = cover_mode;
    // Se consulta el terminal UNA sola vez al arrancar: cada query escribe un
    // escape y la respuesta ("60;30;...") se colaba como texto al alternar.
    let mut half_picker = Picker::from_fontsize((8, 16));
    half_picker.set_protocol_type(ratatui_image::picker::ProtocolType::Halfblocks);
    let foto_picker = match Picker::from_query_stdio() {
        Ok(p) => {
            let (fw, fh) = p.font_size();
            let proto = p.protocol_type();
            let scaled = (
                (fw.max(1) as f64 * hidpi).round() as u16,
                (fh.max(1) as f64 * hidpi).round() as u16,
            );
            let mut np = Picker::from_fontsize((scaled.0.max(1), scaled.1.max(1)));
            np.set_protocol_type(proto);
            np
        }
        Err(_) => {
            let mut p = Picker::from_fontsize((8, 16));
            p.set_protocol_type(ratatui_image::picker::ProtocolType::Halfblocks);
            p
        }
    };
    let mut picker = if cover_mode == CoverMode::Halfblocks { half_picker.clone() } else { foto_picker.clone() };
    dlog!(
        "picker proto={:?} font={:?} mode={}",
        picker.protocol_type(),
        picker.font_size(),
        if cover_mode == CoverMode::Halfblocks { "halfblocks" } else { "foto" }
    );
    let mut cover_img: Option<(String, DynamicImage)> = None; // (key, imagen)
    let mut cover_proto: Option<StatefulProtocol> = None; // render del área
    let mut cover_txt: Option<(String, u16, u16, String)> = None; // halfblocks: (key,w,h,texto)
    let mut cover_key = String::new(); // key de la carátula MOSTRADA
    let mut cover_fresh = false; // true si lo mostrado viene del stream de este track
    let mut last_cover_rect: Option<Rect> = None; // para loguear cambios
    // Arte pendiente: el diff de artwork suele llegar ms después del cambio.
    // Se espera hasta 250ms antes del fallback `get`; el stream fresco gana.
    let mut pending_art = false;
    let mut art_deadline: Option<Instant> = None;
    let mut lyrics: Vec<Lyric> = vec![];
    let mut lyrics_on = true;
    let mut cover_on = true; // c muestra/oculta carátula (igual que l con letras)
    // Prefs: el archivo manda; --cover explícito gana al modo guardado.
    let cli_cover = std::env::args().any(|a| a == "--cover" || a.starts_with("--cover="));
    if let Some((l, c, m)) = load_prefs() {
        lyrics_on = l; cover_on = c;
        if !cli_cover { cover_mode = m; }
    }
    let mut seeking = false; // arrastre en la barra (igual que Go)
    let mut last_seek_sent = Instant::now() - Duration::from_secs(10);
    let mut vis_cache: Vec<usize> = vec![]; // ventana pintada (para clicks exactos)
    let mut finder_open = false;
    let mut query = String::new();
    let mut results: Vec<Found> = vec![];
    let mut cursor: usize = 0;
    let mut finder_offset: usize = 0;
    let mut fire_enter = false; // Enter diferido: teclado o doble click lo piden, el loop lo ejecuta
    let mut last_click: Option<(u16, Instant)> = None; // (fila, cuándo) para doble click
    let mut searched = String::new();
    let mut scope = String::from("library"); // library | catalog (Tab alterna, igual que Go)
    let mut cat_songs: Vec<crate::catalog::Song> = vec![];
    let mut cat_albums: Vec<crate::catalog::Album> = vec![];
    let mut cat_tracks: Vec<crate::catalog::Song> = vec![];
    let mut cat_drill = false;
    let mut cat_err = String::new();
    // browse biblioteca: drill álbum/artista/playlist + modo agregar (a)
    let mut drilled: Option<(String, String)> = None; // (kind,name)
    let mut drill_tracks: Vec<Found> = vec![];
    let mut filter_on = true; // typing filtra; ↑/↓ pasan a navegar (igual que Go)
    let mut lib_page: Vec<Found> = vec![]; // página biblioteca al abrir (Go: fetchTracks library)
    let mut lib_done = false; // fin de biblioteca: la última página trajo < 30
    let mut add_mode = false;
    let mut track_idx: usize = 0; // track bajo cursor al abrir modo agregar
    let mut drill_worker = crate::finder_thread::DrillWorker::new();
    let mut drill_loading = String::new(); // nombre mostrado mientras osascript responde
    let mut cat_worker = crate::finder_thread::CatWorker::new();
    let mut cat_query = String::new(); // query de la página cargada
    let mut cat_more = false; // quedan más páginas en Apple Music
    // floating menu (m / right-click) + help, same box like Go
    let mut menu_open = false;
    let mut menu_help = false;
    let mut menu_cur: usize = 0;
    // Menú: ayuda + ajustes. La reproducción ya tiene teclas y clicks propios.
    let menu_items = |lyr: bool, cov: bool, foto: bool| -> Vec<(String, &'static str)> {
        vec![
            ("Help".to_string(), "help"),
            (format!("Lyrics: {}", if lyr { "on" } else { "off" }), "lyrics"),
            (format!("Cover: {}", if cov { "on" } else { "off" }), "cover"),
            (format!("Cover mode: {}", if foto { "foto" } else { "halfblocks" }), "covermode"),
        ]
    };
    let help_lines: [&str; 21] = [
        "q quit", "esc close", "/ search", "tab scope", "l lyrics",
        "m menu", "right-click close", "f favorite", "+ add to playlist",
        "space play/pause", "n next", "p previous", "s shuffle",
        "r repeat", "+/- volume", "click bar seeks", "click verse jumps",
        "up/down navigate", "enter play", "wheel scroll", "c cover",
    ];
    kick_index();

    // Estado inicial instantáneo (el stream solo manda diffs al cambiar algo).
    if let Some(mut fresh) = fetch_now() {
        fresh.track_since = Some(Instant::now());
        let key = fresh.key();
        state = fresh;
        let _ = art_tx.send(ArtJob { key: key.clone(), b64: None });
        spawn_lyr_fetch(key, state.artist.clone(), state.title.clone(), state.album.clone(), state.duration, bg_tx.clone());
    }

    // Pide letras ya; carátula: si el diff trajo artwork va al instante,
    // si no se espera el diff de artwork (tope 250ms) antes del fallback `get`.
    // Letras+meta al cambio: hilos sueltos (sin cola).
    let request_lyr = |key: &str, st: &Now, tx: &Sender<BgMsg>| {
        spawn_lyr_fetch(key.to_string(), st.artist.clone(), st.title.clone(), st.album.clone(), st.duration, tx.clone());
    };
    let send_art = |key: &str, b64: &Option<String>, art_tx: &Sender<ArtJob>| {
        let _ = art_tx.send(ArtJob { key: key.to_string(), b64: b64.clone() });
    };

    loop {
        // Tracks de la página que coinciden con el filtro: así typing filtra
        // también por canción, no solo el índice (álbum/artista/playlist).
        let page_hits: Vec<usize> = if query.trim().is_empty() {
            vec![]
        } else {
            let ql = query.to_lowercase();
            lib_page.iter().enumerate().filter(|(_, r)| {
                r.name.to_lowercase().contains(&ql)
                    || r.artist.to_lowercase().contains(&ql)
                    || r.album.to_lowercase().contains(&ql)
            }).map(|(i, _)| i).collect()
        };
        // Enter diferido del finder: lo piden el teclado o el doble click.
        // (ponytail: el cuerpo vivía en el brazo KeyCode::Enter; el brazo
        // muerto KeyCode::Null lo conserva como referencia.)
        if std::mem::take(&mut fire_enter) && finder_open {
            if scope == "catalog" {
                let nalb = cat_albums.len();
                if cat_drill {
                    if let Some(s) = cat_tracks.get(cursor) {
                        play_catalog_song(s);
                        finder_open = false; query.clear(); cat_songs.clear(); cat_albums.clear(); cat_tracks.clear(); cat_drill = false; cursor = 0;
                    }
                } else if cursor < nalb {
                    let id = cat_albums.get(cursor).map(|a| a.id.clone()).unwrap_or_default();
                    if !id.is_empty() {
                        cat_tracks.clear(); cat_err.clear();
                        cat_worker.tracks(&id);
                        cat_drill = true; cursor = 0; finder_offset = 0;
                    }
                } else if let Some(s) = cat_songs.get(cursor - nalb) {
                    play_catalog_song(s);
                    finder_open = false; query.clear(); cat_songs.clear(); cat_albums.clear(); cursor = 0;
                } else if !query.is_empty() && searched != query {
                    cat_songs.clear(); cat_albums.clear(); cat_drill = false; cat_err.clear();
                    cat_query = query.clone(); cat_more = false;
                    cat_worker.page(&query, 0);
                    searched = query.clone(); cursor = 0;
                }
            } else if add_mode {
                let (_, _, pls) = lib_index();
                // track_idx cuenta page hits primero, luego resultados (igual que el render).
                let f = page_hits.get(track_idx).and_then(|&i| lib_page.get(i))
                    .or_else(|| results.get(track_idx.saturating_sub(page_hits.len())));
                if let (Some(pl), Some(f)) = (pls.get(cursor), f) { add_to_playlist(f, pl); }
                add_mode = false; cursor = 0;
            } else if let Some((kind, name)) = drilled.clone() {
                if filter_on {
                    if !query.is_empty() { results = search_library(&query); searched = query.clone(); cursor = 0; }
                } else {
                    if drill_tracks.get(cursor).is_some() {
                        play_in_context(&kind, &name, cursor + 1);
                    }
                    finder_open = false; query.clear(); results.clear(); drilled = None; drill_tracks.clear(); cursor = 0;
                }
            } else if query.trim().is_empty() {
                if let Some(f) = lib_page.get(cursor) {
                    let idx = cursor + 1;
                    play_in_context("library", "", idx);
                    let _ = f;
                }
                finder_open = false; query.clear(); results.clear(); cursor = 0;
            } else if filter_on && !results.is_empty() && searched == query {
                if let Some(f) = results.get(cursor) { play_library(f); }
                finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0;
            } else if filter_on {
                results = search_library(&query); searched = query.clone(); cursor = 0;
            } else {
                let ql = query.to_lowercase();
                let (albums, artists, pls) = lib_index();
                let mut rows: Vec<Rowpick> = vec![];
                for a in albums.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(Rowpick::Album(a.clone())); }
                for a in artists.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(Rowpick::Artist(a.clone())); }
                for p in pls.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(Rowpick::Playlist(p.clone())); }
                let do_search = searched != query && !query.is_empty();
                if do_search { results = search_library(&query); searched = query.clone(); }
                for &i in page_hits.iter() { rows.push(Rowpick::Page(i)); }
                for i in 0..results.len() { rows.push(Rowpick::Track(i)); }
                match rows.get(cursor) {
                    Some(Rowpick::Album(a)) => { drill_worker.ask("album", a); drill_loading = a.clone(); drilled = Some(("album".to_string(), a.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                    Some(Rowpick::Artist(a)) => { drill_worker.ask("artist", a); drill_loading = a.clone(); drilled = Some(("artist".to_string(), a.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                    Some(Rowpick::Playlist(p)) => { drill_worker.ask("playlist", p); drill_loading = p.clone(); drilled = Some(("playlist".to_string(), p.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                    Some(Rowpick::Page(i)) => {
                        play_in_context("library", "", i + 1);
                        finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0;
                    }
                    Some(Rowpick::Track(i)) => {
                        if let Some(f) = results.get(*i) { play_library(f); }
                        finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0;
                    }
                    None => { if do_search {} else if !query.is_empty() { results = search_library(&query); searched = query.clone(); cursor = 0; } }
                }
            }
            continue;
        }
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
                    // Arrastrando: el remoto no mueve la barra (como play_hold).
                    let local_elapsed = state.elapsed;
                    let local_at = state.at;
                    let was_seeking = seeking;
                    let changed = merge_update(&mut state, &u, at);
                    if changed {
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
                            art_deadline = Some(at + Duration::from_millis(250));
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
                    if was_seeking && !changed {
                        // Arrastrando: la barra es mía hasta soltar.
                        state.elapsed = local_elapsed;
                        state.at = local_at;
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
                    art_deadline = Some(Instant::now() + Duration::from_millis(250));
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
        // Ancho contenido = cw+2 como tu Go (las líneas traen indent/texto
        // de hasta 62; con 60 se recortaba la derecha: "-2:", sin ↻/r).
        let content_w = cw_w + 2;
        let show_cov = cover_on && show_cover(cover_img.is_some(), h);
        // Misma altura para ambos modos; si falta alto, reducir ancho sin estirar.
        let cov_h = if show_cov {
            match cover_img.as_ref() {
                Some((_, img)) => ((cw_w as u64 * img.height() as u64 / img.width().max(1) as u64 / 2).max(4) as usize).min(cover_rows(h)),
                None => 0,
            }
        } else {
            0
        };
        let has_bar = state.duration > 0.0;
        // Una sola posición por frame: la ventana visible y el click usan
        // exactamente lo mismo (si no, al cruzar un verso el click cae mal).
        let frame_pos = state.position();
        let frame_vis: Vec<usize> = if !state.title.is_empty() && lyrics_on && !lyrics.is_empty() {
            lyric_visual(&lyrics, frame_pos, cw_w.saturating_sub(2), h)
                .into_iter()
                .map(|v| v.idx)
                .collect()
        } else {
            vec![]
        };
        vis_cache = frame_vis;
        let ll: Vec<Line> = if !state.title.is_empty() && lyrics_on {
            lyric_lines(&lyrics, frame_pos, state.playing, cw_w.saturating_sub(2), h)
        } else {
            vec![]
        };
        let lyrics_shown = !ll.is_empty();
        let zone_n = if lyrics_shown { zone_rows(h) } else { 0 };
        // Filas espejo de Go: cover+blanco?, título, meta, gauge+tiempos?,
        // blanco, controles, keys, blancos2?, zona?
        let mut row_cons: Vec<Constraint> = vec![];
        let mut r_cover: Option<usize> = None;
        let (r_title, r_meta): (usize, usize);
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
            // Doble blanco tras foto: si el protocolo pinta de más (métrica
            // de fuente), se come blancos y nunca el título.
            row!(Constraint::Length(1));
            row!(Constraint::Length(1));
        }
        if state.title.is_empty() {
            r_title = row!(Constraint::Length(1)); // "waiting…" reutiliza fila título
            r_meta = r_title;
        } else {
            r_title = row!(Constraint::Length(1));
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
        let cover_r: Option<Rect> = r_cover.and_then(|i| {
            cover_img.as_ref().map(|(_, img)| cover_rect(rows[i], img.width(), img.height()))
        });
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
            // Limpia todo el frame: al ocultar cover/cambiar layout el centrado
            // mueve filas y el texto viejo (halfblocks) quedaba fantasma.
            f.render_widget(ratatui::widgets::Clear, frame_rect);
            if state.title.is_empty() {
                f.render_widget(
                    Paragraph::new("waiting for the Music app… play something ♪")
                        .alignment(Alignment::Center)
                        .style(dim_style()),
                    title_r,
                );
                return;
            }
            if let Some(cr) = cover_r {
                if last_cover_rect != Some(cr) {
                    dlog!("cover rect {:?} img={}", cr, cover_img.as_ref().map(|(_, img)| format!("{}x{}", img.width(), img.height())).unwrap_or_default());
                    last_cover_rect = Some(cr);
                }
                if cover_mode == CoverMode::Halfblocks {
                    // String a celdas exactas (caché por track+tamaño).
                    let area = cr;
                    let key = cover_key.clone();
                    let cached = match &cover_txt {
                        Some((k, ew, eh, _)) if *k == key && *ew == area.width && *eh == area.height => true,
                        _ => false,
                    };
                    if !cached {
                        if let Some((_, img)) = cover_img.as_ref() {
                            let text = halfblocks_string(img, area.width as usize, area.height as usize);
                            cover_txt = Some((key, area.width, area.height, text));
                        }
                    }
                    if let Some((_, _, _, text)) = cover_txt.as_ref() {
                        use ansi_to_tui::IntoText;
                        if let Ok(tt) = text.as_str().into_text() {
                            for (dy, l) in tt.lines.into_iter().enumerate() {
                                if dy >= area.height as usize { break; }
                                let mut spans = vec![Span::raw(String::new())];
                                spans.extend(l.spans);
                                f.render_widget(
                                    Paragraph::new(Line::from(spans)),
                                    Rect::new(area.x, area.y + dy as u16, area.width, 1),
                                );
                            }
                        }
                    }
                } else if let Some(proto) = cover_proto.as_mut() {
                    f.render_stateful_widget(
                        StatefulImage::default().resize(Resize::Scale(Some(image::imageops::FilterType::Lanczos3))),
                        cr,
                        proto,
                    );
                }
            }
            if let Some((kind, name, tracks)) = drill_worker.poll() {
                let cur = drilled.clone().map(|(_, n)| n).unwrap_or_default();
                if cur == name || drill_loading == name {
                    drill_tracks = tracks;
                    cursor = 0; finder_offset = 0;
                }
                let _ = kind;
                drill_loading.clear();
            }
            if let Some(rep) = cat_worker.poll() {
                use crate::finder_thread::CatReply;
                match rep {
                    CatReply::Page { query, offset, songs, albums, more, err } => {
                        if query == cat_query || cat_query.is_empty() {
                            cat_query = query;
                            if offset == 0 { cat_songs = songs; cat_albums = albums; }
                            else { cat_songs.extend(songs); cat_albums.extend(albums); }
                            cat_more = more; cat_err = err;
                        }
                    }
                    CatReply::Tracks { tracks, err, .. } => {
                        cat_tracks = tracks; cat_err = err;
                        cursor = 0; finder_offset = 0;
                    }
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
                // Barra clásica ━━●── (el Gauge gordo no va con este look).
                let (bar, left, right, gap) = progress_bar(state.position(), state.duration, cw_w - 1);
                f.render_widget(Paragraph::new(bar), rows[gi]);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(left, dim_style()),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(right, dim_style()),
                    ])),
                    rows[ti],
                );
            }
            let (icons, keys) = control_icons(&state);
            let dim = dim_style();
            let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
            // Controles compactos con slots (el look probado), no Fill total.
            let slot = cw_w / 5;
            let mut top_spans: Vec<Span> = vec![Span::raw("  ")];
            for (i, ic) in icons.iter().enumerate() {
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
                let padn = slot.saturating_sub(1);
                let (l, r) = match i {
                    0 => (0, padn),
                    4 => (padn, 0),
                    _ => (padn / 2, padn - padn / 2),
                };
                top_spans.push(Span::raw(" ".repeat(l)));
                top_spans.push(Span::styled(ic.clone(), st));
                top_spans.push(Span::raw(" ".repeat(r)));
            }
            f.render_widget(Paragraph::new(Line::from(top_spans)), rows[r_ctrl]);
            let mut bot_spans: Vec<Span> = vec![Span::raw("  ")];
            for (i, k) in keys.iter().enumerate() {
                let padn = slot.saturating_sub(1);
                let (l, r) = match i {
                    0 => (0, padn),
                    4 => (padn, 0),
                    _ => (padn / 2, padn - padn / 2),
                };
                bot_spans.push(Span::raw(" ".repeat(l)));
                bot_spans.push(Span::styled(k.clone(), dim));
                bot_spans.push(Span::raw(" ".repeat(r)));
            }
            f.render_widget(Paragraph::new(Line::from(bot_spans)), rows[r_keys]);
            if let Some(lr) = lyr_rect {
                f.render_widget(Paragraph::new(ll.clone()).alignment(Alignment::Center), lr);
            }
            if menu_open {
                let rows: Vec<String> = if menu_help {
                    help_lines.iter().map(|s| s.to_string()).collect()
                } else {
                    menu_items(lyrics_on, cover_on, cover_mode == CoverMode::Foto).iter().map(|(l, _)| l.clone()).collect()
                };
                // Mismo lugar que el finder, pero alto capado: del título hasta
                // el fin del hero + 2 filas. No baja hasta el pie como el finder.
                let hero_from_title = content.height.saturating_sub(title_r.y.saturating_sub(content.y));
                let mh = (hero_from_title + 2).max(6).min(frame_rect.height.saturating_sub(title_r.y.saturating_sub(frame_rect.y)).max(6));
                let area = Rect::new(content.x, title_r.y, content.width, mh);
                f.render_widget(ratatui::widgets::Clear, area);
                let title = if menu_help { "help" } else { "menu" };
                let block = ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL).title(title);
                let inner = block.inner(area);
                f.render_widget(block, area);
                let lines: Vec<Line> = rows.iter().enumerate().map(|(i, s)| {
                    let st = if !menu_help && i == menu_cur { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                    Line::from(Span::styled(format!("  {}", s), st))
                }).collect();
                f.render_widget(Paragraph::new(lines), inner);
            }
            if finder_open {
                // Overlay anclado al hero: mismo x/ancho del contenido,
                // arranca en la fila del título (cover visible) y baja.
                let w2 = content.width;
                let top = title_r.y;
                let h2 = frame_rect.height.saturating_sub(top.saturating_sub(frame_rect.y)).max(6);
                let area = Rect::new(content.x, top, w2, h2);
                f.render_widget(ratatui::widgets::Clear, area);
                let scope_tag = if scope == "catalog" { "[apple music]" } else { "[library]" };
                let block = ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL).title(format!("/{}_ {}", query, scope_tag));
                let inner = block.inner(area);
                f.render_widget(block, area);
                let mut lines: Vec<Line> = vec![];
                if scope == "catalog" {
                    if !cat_err.is_empty() { lines.push(Line::from(Span::styled(format!("  {}", cat_err), dim_style()))); }
                    else if cat_drill {
                        for (i, t) in cat_tracks.iter().skip(finder_offset).take(inner.height as usize).enumerate() {
                            let i = i + finder_offset;
                            let st = if i == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                            lines.push(Line::from(Span::styled(format!("  {}", t.title), st)));
                        }
                    } else {
                        // Ventana de scroll [finder_offset, +max) sobre albums+songs.
                        // Antes se dibujaba siempre desde 0: el cursor bajaba y las
                        // canciones quedaban inalcanzables ("no encuentra").
                        let max = inner.height as usize;
                        let win1 = finder_offset + max;
                        let mut n = 0;
                        for a in cat_albums.iter() {
                            if n >= win1 { break; }
                            let gi = n; n += 1;
                            if gi < finder_offset { continue; }
                            let st = if gi == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                            let y = if a.year.is_empty() { String::new() } else { format!("  {}", a.year) };
                            lines.push(Line::from(Span::styled(format!("  ◉ {} — {}{}", a.title, a.artist, y), st)));
                        }
                        for s in cat_songs.iter() {
                            if n >= win1 { break; }
                            let gi = n; n += 1;
                            if gi < finder_offset { continue; }
                            let st = if gi == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                            lines.push(Line::from(Span::styled(format!("  {} — {}", s.title, s.artist), st)));
                        }
                        if cat_worker.loading { lines.push(Line::from(Span::styled("  loading…", dim_style()))); }
                    }
                    if lines.is_empty() { lines.push(Line::from(Span::styled("  Enter to search · Tab changes scope", dim_style()))); }
                } else if add_mode {
                    let (_, _, pls) = lib_index();
                    for (i, p) in pls.iter().skip(finder_offset).take(inner.height as usize).enumerate() {
                        let i = i + finder_offset;
                        let st = if i == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                        lines.push(Line::from(Span::styled(format!("  + {}", p), st)));
                    }
                    if lines.is_empty() { lines.push(Line::from(Span::styled("  no playlists", dim_style()))); }
                } else if drilled.is_some() || !drill_loading.is_empty() {
                    let name = drilled.clone().map(|(_, n)| n).unwrap_or_else(|| drill_loading.clone());
                    lines.push(Line::from(Span::styled(format!("  {}", name), dim_style())));
                    for (i, r) in drill_tracks.iter().skip(finder_offset).take(inner.height.saturating_sub(1) as usize).enumerate() {
                        let i = i + finder_offset;
                        let st = if i == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                        lines.push(Line::from(Span::styled(format!("  {}", r.label()), st)));
                    }
                    if drill_tracks.is_empty() {
                        let msg = if drill_loading.is_empty() { "  empty" } else { "  loading…" };
                        lines.push(Line::from(Span::styled(msg, dim_style())));
                    }
                } else if query.trim().is_empty() {
                    // Sin filtro: página de biblioteca (Go muestra tracks, no índice).
                    for (i, r) in lib_page.iter().skip(finder_offset).take(inner.height as usize).enumerate() {
                        let gi = i + finder_offset;
                        let st = if gi == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                        lines.push(Line::from(Span::styled(format!("  {}", r.label()), st)));
                    }
                    if lib_done && !lib_page.is_empty() { lines.push(Line::from(Span::styled("  — end of library —", dim_style()))); }
                    if lines.is_empty() { lines.push(Line::from(Span::styled("  type to filter · Tab for Apple Music", dim_style()))); }
                } else {
                    // index entries (◉ album ⚉ artist ☰ playlist) + page hits + tracks.
                    // Ventana de scroll [finder_offset, finder_offset+max), cursor global.
                    let ql = query.to_lowercase();
                    let (albums, artists, pls) = lib_index();
                    let mut n = 0;
                    let max = inner.height as usize;
                    let win1 = finder_offset + max;
                    macro_rules! wrow {
                        ($txt:expr) => {{
                            let gi = n; n += 1;
                            if gi < finder_offset { None } else if gi >= win1 { None } else {
                                let st = if gi == cursor { Style::default().add_modifier(ratatui::style::Modifier::REVERSED) } else { Style::default() };
                                Some(Line::from(Span::styled($txt, st)))
                            }
                        }};
                    }
                    // Igual que Go: entradas capped a 12 en total, luego tracks.
                    for a in albums.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) {
                        if n >= 12 || n >= win1 { break; }
                        if let Some(l) = wrow!(format!("  ◉ {}", a)) { lines.push(l); }
                    }
                    for a in artists.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) {
                        if n >= 12 || n >= win1 { break; }
                        if let Some(l) = wrow!(format!("  ⚉ {}", a)) { lines.push(l); }
                    }
                    for p in pls.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) {
                        if n >= 12 || n >= win1 { break; }
                        if let Some(l) = wrow!(format!("  ☰ {}", p)) { lines.push(l); }
                    }
                    for &i in page_hits.iter() {
                        if n >= win1 { break; }
                        if let Some(r) = lib_page.get(i) {
                            if let Some(l) = wrow!(format!("  {}", r.label())) { lines.push(l); }
                        }
                    }
                    for r in results.iter() {
                        if n >= win1 { break; }
                        if let Some(l) = wrow!(format!("  {}", r.label())) { lines.push(l); }
                    }
                    if lines.is_empty() {
                        if !query.trim().is_empty() && searched == query {
                            lines.push(Line::from(Span::styled(format!("  no results for '{}'", query), dim_style())));
                        } else {
                            lines.push(Line::from(Span::styled("  Enter to search · Tab for Apple Music · + add", dim_style())));
                        }
                    }
                }
                f.render_widget(Paragraph::new(lines), inner);
            }
        })?;

        // Revisar pronto el canal al cambiar pista, aun si está en pausa.
        let awaiting_cover = pending_art || (cover_key != state.key()
            && state.track_since.is_some_and(|t| t.elapsed() < Duration::from_secs(3)));
        let tick = if awaiting_cover {
            Duration::from_millis(50)
        } else if marquee_live(&state, cw(w)) {
            Duration::from_millis(100)
        } else if state.playing {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(200) // los eventos del stream no despiertan event::poll
        };
        if event::poll(tick)? {
            match event::read()? {
                // Resize: redraw inmediato en la próxima vuelta (el size se
                // relee arriba y el cover se re-renderiza con want_w/want_h).
                Event::Resize(_, _) => {}
                Event::Mouse(m) => {
                    // Misma geometría que la vista: los Rects del Layout.
                    use ratatui::layout::Position;
                    // Soltar siempre corta el arrastre (igual que Go seeking)
                    // y manda el seek final en la posición de release.
                    if m.kind == MouseEventKind::Up(MouseButton::Left) {
                        if seeking {
                            seeking = false;
                            if !state.title.is_empty() && state.duration > 0.0 {
                                if let Some(gr) = gauge_rect {
                                    let ratio = if gr.width > 0 {
                                        (m.column.saturating_sub(gr.x) as f64) / gr.width as f64
                                    } else {
                                        0.0
                                    };
                                    let pos = ratio.clamp(0.0, 1.0) * state.duration;
                                    state.elapsed = pos;
                                    state.at = Some(Instant::now());
                                    transport_seek(state.src == "podcast", pos);
                                    last_seek_sent = Instant::now();
                                }
                            }
                        } else {
                            seeking = false;
                        }
                        continue;
                    }
                    // Arrastrar con botón held: seek continuo con throttle
                    // (máx 1/120ms; el final va al soltar).
                    if m.kind == MouseEventKind::Drag(MouseButton::Left) {
                        if seeking
                            && !state.title.is_empty()
                            && state.duration > 0.0
                            && last_seek_sent.elapsed() > Duration::from_millis(120)
                        {
                            if let Some(gr) = gauge_rect {
                                let ratio = if gr.width > 0 {
                                    (m.column.saturating_sub(gr.x) as f64) / gr.width as f64
                                } else {
                                    0.0
                                };
                                let pos = ratio.clamp(0.0, 1.0) * state.duration;
                                state.elapsed = pos;
                                state.at = Some(Instant::now());
                                transport_seek(state.src == "podcast", pos);
                                last_seek_sent = Instant::now();
                            }
                        }
                        continue;
                    }
                    if m.kind == MouseEventKind::Down(MouseButton::Right) {
                        // Click derecho solo cierra; el menú se abre con m.
                        if finder_open { finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0; }
                        if menu_open { menu_open = false; menu_help = false; }
                        continue;
                    }
                    if finder_open {
                        // Rueda = navegar, click = mover cursor. (Go no tiene
                        // mouse en el overlay; aquí solo selecciona, Enter confirma.)
                        let my = title_r.y;
                        let mh = frame_rect.height.saturating_sub(my.saturating_sub(frame_rect.y)).max(6);
                        let marea = Rect::new(content.x, my, content.width, mh);
                        let pos = Position::new(m.column, m.row);
                        let total = if scope == "catalog" { if cat_drill { cat_tracks.len() } else { cat_albums.len() + cat_songs.len() } }
                            else if add_mode { lib_index().2.len() }
                            else if drilled.is_some() { drill_tracks.len() }
                            else if query.trim().is_empty() { lib_page.len() }
                            else { results.len() + page_hits.len() + 12 };
                        let step = |cur: usize, off: usize, d: isize, h: usize| -> (usize, usize) {
                            let ncur = (cur as isize + d).clamp(0, total.saturating_sub(1).max(0) as isize) as usize;
                            let mut noff = off;
                            if ncur < noff { noff = ncur; }
                            if ncur >= noff + h { noff = ncur + 1 - h; }
                            (ncur, noff)
                        };
                        let vh = marea.height.saturating_sub(3) as usize;
                        match m.kind {
                            MouseEventKind::ScrollUp => { let (c, o) = step(cursor, finder_offset, -3, vh.max(1)); cursor = c; finder_offset = o; continue; }
                            MouseEventKind::ScrollDown => { let (c, o) = step(cursor, finder_offset, 3, vh.max(1)); cursor = c; finder_offset = o; continue; }
                            MouseEventKind::Down(MouseButton::Left) => {
                                if !marea.contains(pos) { continue; }
                                // Solo el drill de biblioteca tiene fila de encabezado
                                // (‹backspace); el resto empieza a seleccionar en la línea 0.
                                let head = if drilled.is_some() { 1 } else { 0 };
                                let rel = m.row.saturating_sub(marea.y + 1) as usize;
                                // La fila del nombre es título, no seleccionable; Esc vuelve.
                                if rel < head { continue; }
                                let want = finder_offset + rel - head;
                                if want < total { cursor = want; }
                                // Doble click en la misma fila = Enter.
                                let dbl = last_click.is_some_and(|(r, t)| r == m.row && t.elapsed() < Duration::from_millis(500));
                                last_click = Some((m.row, Instant::now()));
                                if dbl && want < total { fire_enter = true; }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    if menu_open {
                        // El menú vive en el overlay del finder: la rueda navega,
                        // el click elige, solo el click derecho cierra.
                        let my = title_r.y;
                        let mh = frame_rect.height.saturating_sub(my.saturating_sub(frame_rect.y)).max(6);
                        let marea = Rect::new(content.x, my, content.width, mh);
                        let pos = Position::new(m.column, m.row);
                        let n = if menu_help { help_lines.len() } else { menu_items(lyrics_on, cover_on, cover_mode == CoverMode::Foto).len() };
                        match m.kind {
                            MouseEventKind::Down(MouseButton::Right) => { menu_open = false; menu_help = false; continue; } // solo cierra
                            MouseEventKind::ScrollUp => { menu_cur = menu_cur.saturating_sub(1); continue; }
                            MouseEventKind::ScrollDown => { if menu_cur + 1 < n { menu_cur += 1; } continue; }
                            MouseEventKind::Down(MouseButton::Left) => {
                                if menu_help || !marea.contains(pos) { menu_open = false; menu_help = false; continue; }
                                // fila 0 = borde, fila 1 = primer item
                                let idx = (m.row.saturating_sub(marea.y + 1)) as usize;
                                if idx < n { menu_cur = idx; }
                                else { continue; }
                            }
                            _ => continue,
                        }
                        // click en item = activar; los ajustes no cierran el menú.
                        let act = menu_items(lyrics_on, cover_on, cover_mode == CoverMode::Foto).get(menu_cur).map(|(_, a)| *a).unwrap_or("");
                        match act {
                            "lyrics" => { lyrics_on = !lyrics_on; menu_open = true; }
                            "cover" => { cover_on = !cover_on; menu_open = true; }
                            "covermode" => {
                                cover_mode = if cover_mode == CoverMode::Halfblocks { CoverMode::Foto } else { CoverMode::Halfblocks };
                                picker = if cover_mode == CoverMode::Halfblocks { half_picker.clone() } else { foto_picker.clone() };
                                cover_txt = None;
                                cover_proto = cover_img.as_ref().map(|(_, img)| picker.new_resize_protocol(img.clone()));
                                menu_open = true;
                            }
                            _ => { menu_help = true; menu_open = true; menu_cur = 0; }
                        }
                        save_prefs(lyrics_on, cover_on, cover_mode);
                        continue;
                    }
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
                            seeking = true;
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
                    // Verso: salta ahí (ventana cacheada del frame: exacta).
                    // lr.y YA es la primera línea (los 2 blancos quedan
                    // fuera del rect): sin restar nada más.
                    if let Some(lr) = lyr_rect {
                        if lr.contains(pos) && lyrics_shown {
                            let row = pos.y.saturating_sub(lr.y) as usize;
                            if let Some(&li) = vis_cache.get(row) {
                                if let Some(lyr) = lyrics.get(li) {
                                    let sec = lyr.sec + 0.01;
                                    state.elapsed = sec;
                                    state.at = Some(Instant::now());
                                    transport_seek(state.src == "podcast", sec);
                                }
                            }
                        }
                    }
                }
                Event::Key(k) => {
                dlog!("KEY {:?}", k.code);
                if menu_open {
                    match k.code {
                        KeyCode::Esc | KeyCode::Char('m') | KeyCode::Char('q') => { menu_open = false; menu_help = false; continue; }
                        KeyCode::Up => { menu_cur = menu_cur.saturating_sub(1); continue; }
                        KeyCode::Down => {
                            let n = if menu_help { help_lines.len() } else { menu_items(lyrics_on, cover_on, cover_mode == CoverMode::Foto).len() };
                            if menu_cur + 1 < n { menu_cur += 1; }
                            continue;
                        }
                        KeyCode::Enter => {
                            if menu_help { menu_open = false; menu_help = false; continue; }
                            let act = menu_items(lyrics_on, cover_on, cover_mode == CoverMode::Foto).get(menu_cur).map(|(_, a)| *a).unwrap_or("");
                            match act {
                                "lyrics" => { lyrics_on = !lyrics_on; menu_open = true; }
                                "cover" => { cover_on = !cover_on; menu_open = true; }
                                "covermode" => {
                                    cover_mode = if cover_mode == CoverMode::Halfblocks { CoverMode::Foto } else { CoverMode::Halfblocks };
                                    picker = if cover_mode == CoverMode::Halfblocks { half_picker.clone() } else { foto_picker.clone() };
                                    cover_txt = None;
                                    cover_proto = cover_img.as_ref().map(|(_, img)| picker.new_resize_protocol(img.clone()));
                                    menu_open = true;
                                }
                                _ => { menu_help = true; menu_open = true; menu_cur = 0; }
                            }
                            save_prefs(lyrics_on, cover_on, cover_mode);
                            continue;
                        }
                        _ => { menu_open = false; menu_help = false; }
                    }
                }
                if k.code == KeyCode::Char('m') && !finder_open {
                    menu_open = true; menu_help = false; menu_cur = 0;
                    continue;
                }
                if finder_open {
                    // Visible row count of the current finder view (mirrors render).
                    let finder_n = || -> usize {
                        if scope == "catalog" { if cat_drill { cat_tracks.len() } else { cat_albums.len() + cat_songs.len() } }
                        else if add_mode { lib_index().2.len() }
                        else if drilled.is_some() { drill_tracks.len() }
                        else if query.trim().is_empty() { lib_page.len() }
                        else { let ql = query.to_lowercase(); let (a, r, p) = lib_index();
                            let mut ne = 0;
                            for s in a.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if ne >= 12 { break; } ne += 1; let _ = s; }
                            for s in r.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if ne >= 12 { break; } ne += 1; let _ = s; }
                            for s in p.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if ne >= 12 { break; } ne += 1; let _ = s; }
                            ne + page_hits.len() + results.len() }
                    };
                    // Filas visibles del overlay (igual que el render: inner del bloque).
                    let finder_vh = || -> usize {
                        let top = title_r.y;
                        let h2 = frame_rect.height.saturating_sub(top.saturating_sub(frame_rect.y)).max(6);
                        (h2.saturating_sub(2) as usize).max(1)
                    };
                    match k.code {
                        KeyCode::Char('/') => {
                            finder_open = false; query.clear(); results.clear(); searched.clear();
                            cursor = 0; finder_offset = 0; continue;
                        }
                        // Esc vuelve atrás (del drill a la lista) y solo cierra en la lista.
                        KeyCode::Esc => {
                            if cat_drill { cat_drill = false; cat_tracks.clear(); cursor = 0; finder_offset = 0; continue; }
                            if drilled.is_some() { drilled = None; drill_tracks.clear(); drill_loading.clear(); cursor = 0; finder_offset = 0; continue; }
                            if add_mode { add_mode = false; cursor = 0; finder_offset = 0; continue; }
                            finder_open = false; query.clear(); results.clear(); searched.clear(); cat_songs.clear(); cat_albums.clear(); cat_tracks.clear(); cat_err.clear(); scope = "library".to_string(); continue;
                        }
                        KeyCode::Tab => {
                            scope = if scope == "library" { "catalog".to_string() } else { "library".to_string() };
                            cursor = 0; searched.clear();
                            if !query.is_empty() {
                                if scope == "catalog" {
                                    cat_songs.clear(); cat_albums.clear(); cat_drill = false; cat_err.clear();
                                    cat_query = query.clone(); cat_more = false;
                                    cat_worker.page(&query, 0);
                                } else { results = search_library(&query); }
                                searched = query.clone();
                            }
                            continue;
                        }
                        KeyCode::BackTab => {
                            scope = if scope == "library" { "catalog".to_string() } else { "library".to_string() };
                            cursor = 0; continue;
                        }
                        KeyCode::Enter => { fire_enter = true; continue; }
                        // → entra (igual que Enter), ← sale (igual que Esc).
                        KeyCode::Right => { fire_enter = true; continue; }
                        KeyCode::Left => {
                            if cat_drill { cat_drill = false; cat_tracks.clear(); cursor = 0; finder_offset = 0; continue; }
                            if drilled.is_some() { drilled = None; drill_tracks.clear(); drill_loading.clear(); cursor = 0; finder_offset = 0; continue; }
                            if add_mode { add_mode = false; cursor = 0; finder_offset = 0; continue; }
                            finder_open = false; query.clear(); results.clear(); searched.clear(); cat_songs.clear(); cat_albums.clear(); cat_tracks.clear(); cat_err.clear(); scope = "library".to_string(); continue;
                        }
                        KeyCode::Null if false => {
                            if scope == "catalog" {
                                let nalb = cat_albums.len();
                                if cat_drill {
                                    if let Some(s) = cat_tracks.get(cursor) {
                                        play_catalog_song(s);
                                        finder_open = false; query.clear(); cat_songs.clear(); cat_albums.clear(); cat_tracks.clear(); cat_drill = false; cursor = 0;
                                    }
                                } else if cursor < nalb {
                                    let id = cat_albums.get(cursor).map(|a| a.id.clone()).unwrap_or_default();
                                    if !id.is_empty() {
                                        cat_tracks.clear(); cat_err.clear();
                                        cat_worker.tracks(&id);
                                        cat_drill = true; cursor = 0; finder_offset = 0;
                                    }
                                } else if let Some(s) = cat_songs.get(cursor - nalb) {
                                    play_catalog_song(s);
                                    finder_open = false; query.clear(); cat_songs.clear(); cat_albums.clear(); cursor = 0;
                                } else if !query.is_empty() && searched != query {
                                    cat_songs.clear(); cat_albums.clear(); cat_drill = false; cat_err.clear();
                                    cat_query = query.clone(); cat_more = false;
                                    cat_worker.page(&query, 0);
                                    searched = query.clone(); cursor = 0;
                                }
                            } else if add_mode {
                                let (_, _, pls) = lib_index();
                                // agrega el track bajo el cursor original (track_idx)
                                if let (Some(pl), Some(f)) = (pls.get(cursor), results.get(track_idx)) { add_to_playlist(f, pl); }
                                add_mode = false; cursor = 0;
                            } else if let Some((kind, name)) = drilled.clone() {
                                if filter_on {
                                    // typing + enter = buscar dentro del contexto (doSearch)
                                    if !query.is_empty() { results = search_library(&query); searched = query.clone(); cursor = 0; }
                                } else {
                                    if drill_tracks.get(cursor).is_some() {
                                        play_in_context(&kind, &name, cursor + 1); // índice real en contexto, igual que Go
                                    }
                                    finder_open = false; query.clear(); results.clear(); drilled = None; drill_tracks.clear(); cursor = 0;
                                }
                            } else if query.trim().is_empty() {
                                // Sin filtro: Enter reproduce el track de la página en contexto library.
                                if let Some(f) = lib_page.get(cursor) {
                                    let idx = cursor + 1;
                                    play_in_context("library", "", idx);
                                    let _ = f;
                                }
                                finder_open = false; query.clear(); results.clear(); cursor = 0;
                            } else if filter_on && !results.is_empty() && searched == query {
                                // Resultado visible y seleccionado: Enter reproduce directamente.
                                if let Some(f) = results.get(cursor) { play_library(f); }
                                finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0;
                            } else if filter_on {
                                // Primera pulsación: ejecutar búsqueda.
                                results = search_library(&query); searched = query.clone(); cursor = 0;
                            } else {
                                // misma construcción que el render: entradas + tracks
                                let ql = query.to_lowercase();
                                let (albums, artists, pls) = lib_index();
                                enum EK { Album(String), Artist(String), Playlist(String), Track(usize) }
                                let mut rows: Vec<EK> = vec![];
                                for a in albums.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(EK::Album(a.clone())); }
                                for a in artists.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(EK::Artist(a.clone())); }
                                for p in pls.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)) { if rows.len() >= 12 { break; } rows.push(EK::Playlist(p.clone())); }
                                let do_search = searched != query && !query.is_empty();
                                if do_search { results = search_library(&query); searched = query.clone(); }
                                for i in 0..results.len() { rows.push(EK::Track(i)); }
                                match rows.get(cursor) {
                                    Some(EK::Album(a)) => { drill_worker.ask("album", a); drill_loading = a.clone(); drilled = Some(("album".to_string(), a.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                                    Some(EK::Artist(a)) => { drill_worker.ask("artist", a); drill_loading = a.clone(); drilled = Some(("artist".to_string(), a.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                                    Some(EK::Playlist(p)) => { drill_worker.ask("playlist", p); drill_loading = p.clone(); drilled = Some(("playlist".to_string(), p.clone())); drill_tracks.clear(); cursor = 0; finder_offset = 0; }
                                    Some(EK::Track(i)) => {
                                        if let Some(f) = results.get(*i) { play_library(f); }
                                        finder_open = false; query.clear(); results.clear(); searched.clear(); cursor = 0;
                                    }
                                    None => { if do_search {} else if !query.is_empty() { results = search_library(&query); searched = query.clone(); cursor = 0; } }
                                }
                            }
                            continue;
                        }
                        KeyCode::Up => {
                            filter_on = false;
                            cursor = cursor.saturating_sub(1);
                            if cursor < finder_offset { finder_offset = cursor; }
                            continue;
                        }
                        KeyCode::Home => { filter_on = false; cursor = 0; finder_offset = 0; continue; }
                        KeyCode::End => {
                            filter_on = false;
                            let n = finder_n();
                            if n > 0 {
                                cursor = n - 1;
                                let visible = finder_vh();
                                finder_offset = cursor.saturating_sub(visible - 1);
                            }
                            continue;
                        }
                        KeyCode::Down => {
                            filter_on = false; // ↑/↓ pasan a navegar (Go blurea el filtro)
                            let mut n = finder_n();
                            // Catálogo: siguiente página de Apple Music al acercarse al final.
                            if scope == "catalog" && !cat_drill && cat_more && !cat_worker.loading
                                && cursor + 5 >= cat_albums.len() + cat_songs.len() {
                                cat_worker.page(&cat_query, cat_songs.len().max(cat_albums.len()));
                            }
                            // Biblioteca: carga la siguiente página al acercarse al final.
                            if scope == "library" && !add_mode && drilled.is_none() && query.trim().is_empty()
                                && !lib_done && cursor + 5 >= lib_page.len() && lib_page.len() % 30 == 0 {
                                let page = filtered_page("library", "", lib_page.len());
                                if page.len() < 30 { lib_done = true; }
                                if !page.is_empty() { lib_page.extend(page); n = lib_page.len(); }
                            }
                            if cursor + 1 < n {
                                cursor += 1;
                                let visible = finder_vh();
                                if cursor >= finder_offset + visible { finder_offset = cursor + 1 - visible; }
                            }
                            continue;
                        }
                        KeyCode::Backspace => {
                            if add_mode { add_mode = false; cursor = 0; finder_offset = 0; continue; }
                            if !filter_on && !query.is_empty() {
                                // Go: en navegación, backspace limpia el texto y resultados.
                                query.clear(); results.clear(); searched.clear();
                                cat_songs.clear(); cat_albums.clear(); cat_tracks.clear(); cat_drill = false;
                                cursor = 0; filter_on = true; continue;
                            }
                            if cat_drill && query.is_empty() { cat_drill = false; cursor = 0; continue; }
                            if scope == "library" && drilled.is_some() && query.is_empty() { drilled = None; drill_tracks.clear(); cursor = 0; continue; }
                            query.pop();
                            filter_on = true;
                            continue;
                        }
                        KeyCode::Char('+') if scope == "library" && !add_mode && drilled.is_none() => {
                            // igual que Go: a abre modo agregar sobre el track actual
                            let ql = query.to_lowercase();
                            let (albums, artists, pls) = lib_index();
                            let mut nentries = 0;
                            nentries += albums.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)).take(12).count();
                            nentries += artists.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)).take(12).count();
                            nentries += pls.iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)).take(12).count();
                            if cursor >= nentries {
                                track_idx = cursor - nentries;
                                add_mode = true; cursor = 0;
                            }
                            continue;
                        }
                        KeyCode::Char(c) => {
                            // typing solo filtra el índice (Go); Enter busca.
                            query.push(c); filter_on = true;
                            continue;
                        }
                        _ => { continue; }
                    }
                }
                match k.code {
                    KeyCode::Char('/') => {
                        // Open finder.
                        dlog!("OPEN finder, lib_page={}", lib_page.len());
                        finder_open = true; query.clear(); results.clear(); searched.clear(); cursor = 0; finder_offset = 0;
                        filter_on = true; drilled = None; drill_tracks.clear();
                        if lib_page.is_empty() { lib_page = library_page(); lib_done = lib_page.len() < 30; dlog!("lib_page loaded={} done={}", lib_page.len(), lib_done); }
                        continue;
                    }
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
                        save_prefs(lyrics_on, cover_on, cover_mode);
                    }
                    KeyCode::Char('c') => {
                        cover_on = !cover_on;
                        save_prefs(lyrics_on, cover_on, cover_mode);
                    }
                    KeyCode::Char('C') => {
                        // Alterna foto <-> halfblocks en vivo.
                        cover_mode = if cover_mode == CoverMode::Halfblocks { CoverMode::Foto } else { CoverMode::Halfblocks };
                        picker = if cover_mode == CoverMode::Halfblocks { half_picker.clone() } else { foto_picker.clone() };
                        cover_txt = None;
                        cover_proto = cover_img.as_ref().map(|(_, img)| picker.new_resize_protocol(img.clone()));
                        dlog!("cover mode -> {}", if cover_mode == CoverMode::Halfblocks { "halfblocks" } else { "foto" });
                        save_prefs(lyrics_on, cover_on, cover_mode);
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
