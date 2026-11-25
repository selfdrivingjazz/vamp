use std::{sync::{Arc, mpsc}, time::{Duration, Instant}};
use tiny_http::Method;
use vamp_diagnostics::{Event, Feature, LibraryRole, Reporter};

#[derive(Clone)]
pub struct Telemetry(mpsc::SyncSender<Event>);

impl Telemetry {
    pub fn new(reporter: Arc<Reporter>) -> Self {
        const QUEUE: usize = 32;
        let (send, receive) = mpsc::sync_channel(QUEUE);
        std::thread::spawn(move || {
            loop {
                match receive.recv_timeout(Duration::from_secs(5)) {
                    Ok(event) => {
                        reporter.record_batch(std::iter::once(event).chain(receive.try_iter().take(QUEUE - 1)));
                        // Coalesce a burst into the next write instead of doing disk I/O per request.
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => reporter.flush(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        reporter.flush();
                        return;
                    }
                }
            }
        });
        Self(send)
    }

    pub fn observe(&self, feature: Feature, ok: bool, started: Option<Instant>) {
        let mut event = Event::new(feature, ok);
        if let Some(started) = started {
            event = event.timed(started.elapsed());
        }
        event.library_role = Some(LibraryRole::Authority);
        let _ = self.0.try_send(event);
    }

    pub(crate) fn job(&self, name: &str, status: &str) {
        let ok = match status {
            "completed" => true,
            "failed" => false,
            _ => return,
        };
        let feature = match name {
            "siglip-embedding" => Feature::Embedding,
            "florence-description" | "florence-tags" => Feature::Captions,
            _ => Feature::OtherProcessor,
        };
        self.observe(feature, ok, None);
    }
}

/// Only instrumentation points controlled by this program; no raw routes or query data.
pub(crate) fn route(method: &Method, segments: &[&str]) -> Option<Feature> {
    Some(match (method, segments) {
        // Every incremental pull checks /hello; /library is the attach handshake.
        (Method::Get, ["v1", "library"]) => Feature::LibraryConnect,
        (Method::Get, ["v1", "snapshot"]) => Feature::Sync,
        (Method::Get, ["v1", "search"]) => Feature::Search,
        (Method::Get, ["v1", "assets", _, "similar"]) => Feature::Similar,
        (Method::Post, ["v1", "clip"] | ["v1", "clip", "batch"]) => Feature::Clip,
        // Bulk view reads use POST, but aren't writes.
        (Method::Post, ["v1", "assets", "batch"]) => return None,
        (Method::Post, ["v1", "ingest"] | ["v1", "assets", _] | ["v1", "assets", _, "accept-tags"])
        | (Method::Delete, ["v1", "assets", _]) => Feature::Save,
        _ => return None,
    })
}

pub(crate) fn native(operation: &str) -> Option<Feature> {
    Some(match operation {
        "galaxy" | "galaxy_search" => Feature::Galaxy,
        "similar" => Feature::Similar,
        "update" | "remove" | "restore" | "empty_trash" | "tag_rename" | "tag_delete"
        | "collection_create" | "collection_move" | "smart_folder_create" | "smart_folder_update"
        | "smart_folder_delete" => Feature::Save,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_pulls_do_not_count_as_attaches_or_saves() {
        assert!(route(&Method::Get, &["v1", "hello"]).is_none());
        assert!(route(&Method::Post, &["v1", "assets", "batch"]).is_none());
        assert!(matches!(route(&Method::Get, &["v1", "library"]), Some(Feature::LibraryConnect)));
        assert!(matches!(route(&Method::Post, &["v1", "assets", "asset-id"]), Some(Feature::Save)));
    }
}
