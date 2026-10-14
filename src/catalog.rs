use std::time::Duration;

#[derive(Default, Clone)]
pub(crate) struct Song { pub(crate) title: String, pub(crate) artist: String, pub(crate) url: String }
#[derive(Default, Clone)]
pub(crate) struct Album { pub(crate) id: String, pub(crate) title: String, pub(crate) artist: String, pub(crate) year: String }

fn agent() -> ureq::Agent {
    use std::sync::OnceLock;
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build()).clone()
}

fn dev_token() -> Option<String> {
    // caché disco igual que Go
    let dir = std::env::var("HOME").ok().map(|h| format!("{}/.cache/music-cli", h))?;
    let _ = std::fs::create_dir_all(&dir);
    let p = format!("{}/devtoken.json", dir);
    if let Ok(raw) = std::fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            let exp = v.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0);
            let tok = v.get("token").and_then(|x| x.as_str()).unwrap_or("");
            if !tok.is_empty() && exp - 3600 > chrono_now() { return Some(tok.to_string()); }
        }
    }
    let r = agent().get("https://apple-music-oauth.raycast.com/developer-token").call().ok()?;
    let v: serde_json::Value = r.into_json().ok()?;
    let tok = v.get("developer_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let exp = v.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0);
    if tok.is_empty() { return None; }
    let _ = std::fs::write(&p, serde_json::json!({"token": &tok, "expires_at": exp}).to_string());
    Some(tok)
}

fn chrono_now() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) }

fn api_get(path: &str) -> Option<serde_json::Value> {
    let t = dev_token()?;
    agent().get(&format!("https://api.music.apple.com{}", path))
        .set("Authorization", &format!("Bearer {}", t))
        .call().ok()?.into_json().ok()
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' { o.push(b as char); }
        else { o.push_str(&format!("%{:02X}", b)); }
    }
    o
}

/// Paginated search: 15 per type per page, like library pages.
/// Returns (songs, albums, has_more).
#[allow(clippy::type_complexity)]
pub(crate) fn search_page(q: &str, offset: usize) -> Result<(Vec<Song>, Vec<Album>, bool), String> {
    let v = api_get(&format!("/v1/catalog/us/search?types=songs,albums&limit=15&offset={}&term={}", offset, enc(q))).ok_or("Apple Music catalog unavailable".to_string())?;
    let mut songs = vec![];
    let mut albums = vec![];
    if let Some(arr) = v.pointer("/results/songs/data").and_then(|x| x.as_array()) {
        for s in arr {
            let a = &s["attributes"];
            songs.push(Song {
                title: a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                artist: a.get("artistName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                url: a.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    if let Some(arr) = v.pointer("/results/albums/data").and_then(|x| x.as_array()) {
        for s in arr {
            let a = &s["attributes"];
            let rel = a.get("releaseDate").and_then(|x| x.as_str()).unwrap_or("");
            albums.push(Album {
                id: s.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                title: a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                artist: a.get("artistName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                year: rel.get(..4).unwrap_or("").to_string(),
            });
        }
    }
    let more = songs.len() == 15 || albums.len() == 15;
    Ok((songs, albums, more))
}

pub(crate) fn album_tracks(id: &str) -> Result<(Album, Vec<Song>), String> {
    let v = api_get(&format!("/v1/catalog/us/albums/{}?include=tracks", enc(id))).ok_or("Apple Music catalog unavailable".to_string())?;
    let d = v.get("data").and_then(|x| x.as_array()).and_then(|a| a.first()).ok_or("Empty album".to_string())?;
    let a = &d["attributes"];
    let rel = a.get("releaseDate").and_then(|x| x.as_str()).unwrap_or("");
    let album = Album {
        id: d.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        title: a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        artist: a.get("artistName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        year: rel.get(..4).unwrap_or("").to_string(),
    };
    let mut out = vec![];
    if let Some(arr) = d.pointer("/relationships/tracks/data").and_then(|x| x.as_array()) {
        for s in arr {
            let at = &s["attributes"];
            out.push(Song {
                title: at.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                artist: at.get("artistName").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_else(|| album.artist.clone()),
                url: at.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    Ok((album, out))
}
