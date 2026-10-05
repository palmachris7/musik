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
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};

use crate::lyrics::{lyric_lines, lyric_visual, zone_rows};
use crate::media::*;
use crate::state::*;
use crate::view::*;

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
    let picker = match cover_mode {
        CoverMode::Halfblocks => {
            let mut p = Picker::from_fontsize((8, 16));
            p.set_protocol_type(ratatui_image::picker::ProtocolType::Halfblocks);
            p
        }
        CoverMode::Foto => match Picker::from_query_stdio() {
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
        },
    };
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
    // Se espera (tope 800ms) en vez de disparar `get` que puede traer lo viejo.
    let mut pending_art = false;
    let mut art_deadline: Option<Instant> = None;
    let mut lyrics: Vec<Lyric> = vec![];
    let mut lyrics_on = true;
    let mut seeking = false; // arrastre en la barra (igual que Go)
    let mut last_seek_sent = Instant::now() - Duration::from_secs(10);
    let mut vis_cache: Vec<usize> = vec![]; // ventana pintada (para clicks exactos)

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
        // Ancho contenido = cw+2 como tu Go (las líneas traen indent/texto
        // de hasta 62; con 60 se recortaba la derecha: "-2:", sin ↻/r).
        let content_w = cw_w + 2;
        let show_cov = show_cover(cover_img.is_some(), h);
        // Halfblocks: alto fijo full-bleed como chafa (el string llena todo).
        // Foto: área según aspecto real (celdas 1:2), tope cover_rows.
        let cov_h = if show_cov {
            if cover_mode == CoverMode::Halfblocks {
                cover_rows(h)
            } else {
                match cover_img.as_ref() {
                    Some((_, img)) => {
                        let (iw, ih) = (img.width().max(1), img.height().max(1));
                        ((cw_w as u32 * ih / iw / 2).max(4) as usize).min(cover_rows(h))
                    }
                    None => 0,
                }
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
        // La foto se centra dentro de su área con su aspecto real (Fit):
        // el protocolo la pega arriba-izquierda y si no es cuadrada
        // queda el hueco a la derecha.
        let cover_r: Option<Rect> = r_cover.map(|i| {
            let area = rows[i];
            // Centrado solo con aspecto (ratio de celda, invariante al DPI):
            // nunca con píxeles absolutos (mienten en Retina).
            let (img_w, img_h) = cover_img
                .as_ref()
                .map(|(_, img)| (img.width().max(1) as f64, img.height().max(1) as f64))
                .unwrap_or((1.0, 1.0));
            let (fw, fh) = picker.font_size();
            let cell_ratio = fh.max(1) as f64 / fw.max(1) as f64; // ~2.0 siempre
            let (aw, ah) = (area.width as f64, area.height as f64);
            let mut cw2 = aw.min(ah * cell_ratio * img_w / img_h);
            let mut ch2 = ah.min(aw / (cell_ratio * img_w / img_h));
            cw2 = cw2.max(1.0);
            ch2 = ch2.max(1.0);
            let cw2 = cw2 as u16;
            let ch2 = ch2 as u16;
            Rect::new(
                area.x + (area.width - cw2) / 2,
                area.y + (area.height - ch2) / 2,
                cw2,
                ch2,
            )
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
                    // String a celdas exactas (caché por track+tamaño): el
                    // mismo path centrado que todo lo demás, cero protocolo.
                    let area = rows[r_cover.unwrap()];
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
                                if dy >= area.height as usize {
                                    break;
                                }
                                let mut spans = vec![Span::raw("".to_string())];
                                spans.extend(l.spans);
                                f.render_widget(
                                    Paragraph::new(Line::from(spans)),
                                    Rect::new(area.x, area.y + dy as u16, area.width, 1),
                                );
                            }
                        }
                    }
                } else if let Some(proto) = cover_proto.as_mut() {
                    f.render_stateful_widget(StatefulImage::default(), cr, proto);
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
                f.render_widget(Paragraph::new(ll.clone()), lr);
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
