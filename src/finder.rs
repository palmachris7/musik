#[derive(Default, Clone)]
pub(crate) struct Found {
    pub(crate) name: String,
    pub(crate) artist: String,
    pub(crate) album: String,
}
impl Found {
    pub(crate) fn label(&self) -> String {
        if self.artist.is_empty() { self.name.clone() }
        else { format!("{} — {}", self.name, self.artist) }
    }
}
