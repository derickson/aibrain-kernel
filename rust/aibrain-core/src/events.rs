//! The change feed.
//!
//! Ingest already knew when a note moved; nothing downstream could hear it, so
//! the browser had to poll or reload. A broadcast channel is the right shape
//! here because the events are worthless to a client that was not listening:
//! there is no backlog to replay, only "your copy is stale, ask again".
//!
//! A subscriber that falls behind is told so rather than quietly skipping
//! events — `Lagged` becomes a `reindex` event with no brain, which the
//! browser reads as "refetch everything".

use serde::Serialize;
use tokio::sync::broadcast;

/// Enough room for a reindex of a handful of vaults to land while a slow
/// browser is repainting. Past this a subscriber is told to resync instead.
const CAPACITY: usize = 256;

/// One thing that changed.
///
/// `kind` is `note` for a single file the watcher saw, `brain` for a vault
/// whose revision moved, and `reindex` for a whole rescan.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub kind: &'static str,
    pub brain_id: String,
    pub revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<i64>,
}

impl Change {
    pub fn brain(brain_id: impl Into<String>, revision: i64) -> Self {
        Change { kind: "brain", brain_id: brain_id.into(), revision, note_id: None }
    }

    pub fn note(brain_id: impl Into<String>, revision: i64, note_id: i64) -> Self {
        Change {
            kind: "note",
            brain_id: brain_id.into(),
            revision,
            note_id: Some(note_id),
        }
    }

    pub fn reindex(brain_id: impl Into<String>, revision: i64) -> Self {
        Change { kind: "reindex", brain_id: brain_id.into(), revision, note_id: None }
    }

    /// What a subscriber that fell behind is sent. No brain, so a client reads
    /// it as "everything may have moved".
    pub fn lagged() -> Self {
        Change { kind: "reindex", brain_id: String::new(), revision: 0, note_id: None }
    }
}

/// The publish side. Cloneable, cheap, and never fails: a send with no
/// listeners is not an error here, it is the normal state of a desktop app
/// with no browser tab open.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Change>,
}

impl Default for Bus {
    fn default() -> Self {
        Bus::new()
    }
}

impl Bus {
    pub fn new() -> Self {
        Bus { tx: broadcast::channel(CAPACITY).0 }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.tx.subscribe()
    }

    /// How many streams are open. Only used for logging and tests.
    pub fn listeners(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn publish(&self, change: Change) {
        let _ = self.tx.send(change);
    }

    pub fn publish_all(&self, changes: impl IntoIterator<Item = Change>) {
        for change in changes {
            self.publish(change);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_into_the_void_is_not_an_error() {
        let bus = Bus::new();
        assert_eq!(bus.listeners(), 0);
        bus.publish(Change::brain("v", 3));
    }

    #[tokio::test]
    async fn a_subscriber_sees_what_was_published_after_it_joined() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(Change::note("v", 4, 17));
        let change = rx.recv().await.unwrap();
        assert_eq!(change.kind, "note");
        assert_eq!(change.brain_id, "v");
        assert_eq!(change.revision, 4);
        assert_eq!(change.note_id, Some(17));
    }

    #[test]
    fn a_brain_event_leaves_note_id_out_of_the_json() {
        let json = serde_json::to_string(&Change::brain("v", 1)).unwrap();
        assert!(!json.contains("note_id"), "{json}");
        assert!(json.contains("\"kind\":\"brain\""));
    }
}
