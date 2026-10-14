// Background finder query: drill stays responsive while osascript works.
// KISS: single worker message, single reply channel. (ponytail: blocks UI
// while osascript runs; next step is a bus like lyrics if Enter still lags.)
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::catalog::{Album, Song};
use crate::finder::Found;
use crate::media::{context_tracks, filtered_page};

pub(crate) struct DrillReq {
    pub(crate) kind: String,
    pub(crate) name: String,
}

type Reply = (String, String, Vec<Found>);

pub(crate) struct DrillWorker {
    tx: Sender<DrillReq>,
    rx: Receiver<Reply>,
    pub(crate) pending: Option<DrillReq>,
}

impl DrillWorker {
    pub(crate) fn new() -> Self {
        let (tx, req_rx): (Sender<DrillReq>, Receiver<DrillReq>) = channel();
        let (rep_tx, rep_rx): (Sender<Reply>, Receiver<Reply>) = channel();
        thread::spawn(move || {
            while let Ok(r) = req_rx.recv() {
                let mut tracks = context_tracks(&r.kind, &r.name);
                if tracks.is_empty() && r.kind != "search" {
                    tracks = filtered_page(&r.kind, &r.name, 0);
                }
                let _ = rep_tx.send((r.kind, r.name, tracks));
            }
        });
        Self { tx, rx: rep_rx, pending: None }
    }

    pub(crate) fn ask(&mut self, kind: &str, name: &str) {
        let req = DrillReq { kind: kind.to_string(), name: name.to_string() };
        self.pending = Some(DrillReq { kind: req.kind.clone(), name: req.name.clone() });
        let _ = self.tx.send(req);
    }

    pub(crate) fn poll(&mut self) -> Option<Reply> {
        match self.rx.try_recv() {
            Ok(r) => {
                self.pending = None;
                Some(r)
            }
            Err(_) => None,
        }
    }
}

// Catalog worker: Apple Music search paging in background, same shape as drill.
// (ponytail: one in-flight request; newer query wins on arrival.)
pub(crate) enum CatReply {
    Page { query: String, offset: usize, songs: Vec<Song>, albums: Vec<Album>, more: bool, err: String },
    Tracks { tracks: Vec<Song>, err: String },
}

pub(crate) struct CatWorker {
    tx: Sender<(String, usize, String)>,
    rx: Receiver<CatReply>,
    pub(crate) loading: bool,
}

impl CatWorker {
    pub(crate) fn new() -> Self {
        let (tx, req_rx) = channel::<(String, usize, String)>();
        let (rep_tx, rep_rx) = channel::<CatReply>();
        thread::spawn(move || {
            while let Ok((op, off, q)) = req_rx.recv() {
                if op == "page" {
                    match crate::catalog::search_page(&q, off) {
                        Ok((s, a, m)) => { let _ = rep_tx.send(CatReply::Page { query: q, offset: off, songs: s, albums: a, more: m, err: String::new() }); }
                        Err(e) => { let _ = rep_tx.send(CatReply::Page { query: q, offset: off, songs: vec![], albums: vec![], more: false, err: e }); }
                    }
                } else {
                    match crate::catalog::album_tracks(&q) {
                        Ok((_al, t)) => { let _ = rep_tx.send(CatReply::Tracks { tracks: t, err: String::new() }); }
                        Err(e) => { let _ = rep_tx.send(CatReply::Tracks { tracks: vec![], err: e }); }
                    }
                }
            }
        });
        Self { tx, rx: rep_rx, loading: false }
    }

    pub(crate) fn page(&mut self, query: &str, offset: usize) {
        self.loading = true;
        let _ = self.tx.send(("page".to_string(), offset, query.to_string()));
    }

    pub(crate) fn tracks(&mut self, id: &str) {
        self.loading = true;
        let _ = self.tx.send(("tracks".to_string(), 0, id.to_string()));
    }

    pub(crate) fn poll(&mut self) -> Option<CatReply> {
        match self.rx.try_recv() {
            Ok(r) => { self.loading = false; Some(r) }
            Err(_) => None,
        }
    }
}
