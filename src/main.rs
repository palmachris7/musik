#[macro_export]
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

mod app;
mod lyrics;
mod media;
mod state;
mod view;

use std::io;

use crate::media::mc_control;

fn print_usage() {
    eprintln!("musik — Apple Music en tu terminal (q sale)");
    eprintln!("  musik [--cover foto|halfblocks] [--hidpi N]   TUI");
    eprintln!("  musik play | pause | toggle | next | prev | version");
}

fn main() -> io::Result<()> {
    // Subcomandos primero (sin TUI): iguales que music-cli, vía media-control.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Forma de cover (rama exp): foto (auto) o halfblocks (texto, medible).
    let mut cover_mode = crate::app::CoverMode::Foto;
    for w in args.windows(2) {
        if w[0] == "--cover" && w[1] == "halfblocks" {
            cover_mode = crate::app::CoverMode::Halfblocks;
        }
    }
    if args.iter().any(|a| a == "--cover=halfblocks") {
        cover_mode = crate::app::CoverMode::Halfblocks;
    }
    // Escala HiDPI para la foto (Retina = puntos vs píxeles). Default 2.0
    // en macOS-arm64 (todo Retina), 1.0 resto. Override: --hidpi 1.
    let mut hidpi = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        2.0
    } else {
        1.0
    };
    for w in args.windows(2) {
        if w[0] == "--hidpi" {
            if let Ok(n) = w[1].parse::<f64>() {
                if n > 0.0 && n <= 4.0 {
                    hidpi = n;
                }
            }
        }
    }
    let is_flag_value = |a: &String| {
        *a == "viuer"
            || *a == "halfblocks"
            || *a == "foto"
            || a.parse::<f64>().is_ok()
    };
    if let Some(cmd) = args.iter().find(|a| !a.starts_with('-') && !is_flag_value(a)) {
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
    crate::app::run(cover_mode, hidpi)
}
