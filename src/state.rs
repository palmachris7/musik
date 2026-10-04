use std::time::Instant;

#[derive(Default, Clone)]
pub(crate) struct Lyric {
    pub(crate) sec: f64,
    pub(crate) text: String,
}

pub(crate) struct VisLine {
    pub(crate) idx: usize,
    pub(crate) first: bool,
    pub(crate) text: String,
}

#[derive(Default, Clone)]
pub(crate) struct Now {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album: String,
    pub(crate) duration: f64,
    pub(crate) elapsed: f64,
    pub(crate) at: Option<Instant>,
    pub(crate) playing: bool,
    pub(crate) shuffle: bool,
    pub(crate) repeat: String, // "off" | "one" | "all"
    pub(crate) fav: bool,
    pub(crate) track_since: Option<Instant>,
    pub(crate) src: String, // "music" | "podcast" | "" — como Go nowInfo.Src
}

impl Now {
    pub(crate) fn key(&self) -> String {
        format!("{}\x00{}\x00{}", self.title, self.artist, self.album)
    }
    pub(crate) fn position(&self) -> f64 {
        if !self.playing {
            return self.elapsed;
        }
        match self.at {
            Some(t) => self.elapsed + t.elapsed().as_secs_f64(),
            None => self.elapsed,
        }
    }
    pub(crate) fn meta(&self) -> String {
        if self.album.is_empty() {
            self.artist.clone()
        } else {
            format!("{} — {}", self.artist, self.album)
        }
    }
    // Ciclo repeat igual que Go (tecla r): off -> one -> all -> off.
    pub(crate) fn repeat_cycle(&mut self) {
        self.repeat = match self.repeat.as_str() {
            "one" => "all".to_string(),
            "all" => "off".to_string(),
            _ => "one".to_string(),
        };
    }
}

// ---- helpers idénticos a Go ----

pub(crate) struct StreamUpdate {
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) duration: Option<f64>,
    pub(crate) elapsed: Option<f64>,
    pub(crate) playing: Option<bool>,
    pub(crate) artwork: Option<String>,
    pub(crate) src: String, // "music" | "podcast" | "" — "other" se descarta en el hilo
}

pub(crate) enum BgMsg {
    Update(StreamUpdate),
    CoverReady { key: String, img: image::DynamicImage, fresh: bool },
    LyricsReady { key: String, items: Vec<Lyric> },
    MetaReady { key: String, fav: bool, shuffle: bool, repeat: String },
    StreamAlive,
    StreamDead,
}

pub(crate) struct ArtJob {
    pub(crate) key: String,
    pub(crate) b64: Option<String>, // None => el worker hace `get` (lento, en background)
}

// Experimental --cover viuer: la técnica de viuer (half-block inferior ▄,
// fg=píxel abajo bg=píxel arriba, resize con image) en vez de chafa.
// Mismo marco (indent 2, ANSI) para comparar el LOOK 1:1.

// (fetch de letras: hilos sueltos por track, ver spawn_lyr_fetch)

// srcOf igual que Go stream.go: Safari & co => "other" (se descarta).
