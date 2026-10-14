pub(crate) const COVER_W: usize = 60; // = coverWidth en Go (tui.go + cover.go)
pub(crate) const LYRIC_ROWS: usize = 9;

use std::time::{Duration, Instant};

use image::DynamicImage;
use ratatui::{layout::Rect, style::Style};

use crate::state::Now;

pub(crate) fn dim_style() -> Style {
    Style::default().add_modifier(ratatui::style::Modifier::DIM)
}

pub(crate) fn fmt_time(s: f64) -> String {
    let m = (s as i64) / 60;
    let sec = (s as i64) % 60;
    format!("{}:{:02}", m, sec)
}

pub(crate) fn char_count(s: &str) -> usize {
    s.chars().count()
}

// marquee 1-pass: 3s quieto, un barrido ~10 pasos/s, luego quieto al inicio.

pub(crate) fn marquee(s: &str, width: usize, now: Instant, since: Instant) -> String {
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

pub(crate) fn cw(term_w: usize) -> usize {
    let mut w = COVER_W;
    if term_w >= 2 && term_w - 2 < w {
        w = term_w.saturating_sub(4);
    }
    if w < 16 {
        w = 16;
    }
    w
}

pub(crate) fn marquee_live(n: &Now, width: usize) -> bool {
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

pub(crate) fn control_icons(n: &Now) -> (Vec<String>, Vec<String>) {
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

pub(crate) fn progress_bar(pos: f64, dur: f64, width: usize) -> (String, String, String, usize) {
    // Devuelve (barra, left, right, gap): el look probado de main.
    if dur <= 0.0 {
        return (String::new(), String::new(), String::new(), 1);
    }
    let mut ratio = pos / dur;
    if ratio < 0.0 {
        ratio = 0.0;
    }
    if ratio > 1.0 {
        ratio = 1.0;
    }
    let fill = (ratio * width as f64) as usize;
    let bar = format!("  {}{}●{}", "━".repeat(fill), "", "─".repeat(width - fill));
    let mut remain = dur - pos;
    if remain < 0.0 {
        remain = 0.0;
    }
    let (left, right) = (fmt_time(pos), format!("-{}", fmt_time(remain)));
    let mut gap = (2 + width as i64 + 1) - (2 + char_count(&left) as i64) - char_count(&right) as i64;
    if gap < 1 {
        gap = 1;
    }
    (bar, left, right, gap as usize)
}

// Fit sin deformar: una celda mide aproximadamente el doble de alto que de ancho.
pub(crate) fn cover_rect(area: Rect, img_w: u32, img_h: u32) -> Rect {
    let (w, h) = (area.width as u64, area.height as u64);
    let (iw, ih) = (img_w.max(1) as u64, img_h.max(1) as u64);
    let (rw, rh) = if w * ih > h * 2 * iw {
        ((h * 2 * iw / ih).max(1).min(w), h)
    } else {
        (w, (w * ih / (2 * iw)).max(1).min(h))
    };
    Rect::new(
        area.x + ((w - rw) / 2) as u16,
        area.y + ((h - rh) / 2) as u16,
        rw as u16,
        rh as u16,
    )
}

pub(crate) fn cover_rows(term_h: usize) -> usize {
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

// Halfblocks a string (técnica viuer: ▄ fg=abajo bg=arriba): celdas exactas,
// cero protocolo, centrado garantizado por ser texto normal.
pub(crate) fn halfblocks_string(img: &DynamicImage, width: usize, rows: usize) -> String {
    if width == 0 || rows == 0 {
        return String::new();
    }
    let rgb = img.to_rgb8();
    let thumb = image::imageops::resize(
        &rgb,
        width as u32,
        (rows * 2).max(2) as u32,
        image::imageops::FilterType::Triangle,
    );
    let mut out = String::new();
    for y in 0..rows {
        for x in 0..width {
            let t = thumb.get_pixel(x as u32, (y * 2) as u32);
            let b = thumb.get_pixel(x as u32, (y * 2 + 1) as u32);
            out.push_str(&format!(
                "\x1b[38;2;{};{};{};48;2;{};{};{}m▄",
                b[0], b[1], b[2], t[0], t[1], t[2]
            ));
        }
        out.push_str("\x1b[0m");
        if y + 1 < rows {
            out.push('\n');
        }
    }
    out
}

pub(crate) fn show_cover(has_cover: bool, term_h: usize) -> bool {
    if !has_cover {
        return false;
    }
    !(term_h > 0 && term_h < 20) // mismo gate que Go: tiny esconde cover
}


// Artwork del `get` completo (con carátula). Solo lo llama el worker de
// carátula cuando el stream no trajo artworkData.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::zone_rows;
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
    fn cover_fits_and_centers() {
        assert_eq!(cover_rect(Rect::new(10, 2, 60, 20), 600, 600), Rect::new(20, 2, 40, 20));
        assert_eq!(cover_rect(Rect::new(10, 2, 60, 30), 600, 600), Rect::new(10, 2, 60, 30));
        assert_eq!(cover_rect(Rect::new(10, 2, 60, 20), 300, 600), Rect::new(30, 2, 20, 20));
    }
    #[test]
    fn show_cover_tiny() {
        assert!(!show_cover(true, 15));
        assert!(show_cover(true, 30));
        assert!(!show_cover(false, 40));
    }
}

