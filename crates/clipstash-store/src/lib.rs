use std::collections::VecDeque;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use clipstash_types::{
    ClipStashError, ClipboardContent, ClipboardItem, Config, MAX_CAPACITY,
};

/// A ring-buffer clipboard store backed by SQLite for persistence.
pub struct ClipStore {
    ring: VecDeque<ClipboardItem>,
    capacity: usize,
    active_index: usize,
    next_id: u64,
    db: Option<Connection>,
}

/// Wrapper for serializing `ClipboardContent` via MessagePack.
#[derive(Serialize, Deserialize)]
struct ContentWrapper {
    content: ClipboardContent,
}

impl ClipStore {
    /// Create a new `ClipStore` from the given configuration.
    ///
    /// If `config.persist` is true, opens (or creates) a SQLite database at
    /// `Config::data_dir().join("history.db")`, enables WAL mode, creates the
    /// schema, and loads the most recent N non-evicted items (where N is the
    /// configured capacity).
    pub fn new(config: &Config) -> Result<Self, ClipStashError> {
        let capacity = config.capacity.clamp(1, MAX_CAPACITY);

        let (db, ring, next_id) = if config.persist {
            let data_dir = Config::data_dir();
            std::fs::create_dir_all(&data_dir).map_err(|e| {
                ClipStashError::ConfigError(format!(
                    "Failed to create data directory {}: {e}",
                    data_dir.display()
                ))
            })?;

            let db_path = data_dir.join("history.db");
            let conn = Connection::open(&db_path)?;

            conn.pragma_update(None, "journal_mode", "WAL")?;

            Self::create_schema(&conn)?;

            let (items, max_id) = Self::load_items(&conn, capacity)?;

            let next_id = max_id.map_or(1, |id| id + 1);

            (Some(conn), items, next_id)
        } else {
            (None, VecDeque::with_capacity(capacity), 1)
        };

        Ok(Self {
            ring,
            capacity,
            active_index: 0,
            next_id,
            db,
        })
    }

    /// Create a `ClipStore` that uses the given SQLite connection, primarily
    /// for testing with a custom database path.
    #[cfg(test)]
    fn new_with_connection(conn: Connection, capacity: usize) -> Result<Self, ClipStashError> {
        let capacity = capacity.clamp(1, MAX_CAPACITY);

        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::create_schema(&conn)?;

        let (items, max_id) = Self::load_items(&conn, capacity)?;
        let next_id = max_id.map_or(1, |id| id + 1);

        Ok(Self {
            ring: items,
            capacity,
            active_index: 0,
            next_id,
            db: Some(conn),
        })
    }

    /// Push a new clipboard item into the ring buffer.
    ///
    /// Deduplicates against `ring[0]` (the most recent item) by content hash.
    /// If the content hash matches, the push is skipped and the existing item's
    /// ID is returned.
    ///
    /// Assigns `next_id` to the item, persists it to the database (best-effort),
    /// evicts the oldest item if at capacity, and resets `active_index` to 0.
    pub fn push(&mut self, mut item: ClipboardItem) -> Result<u64, ClipStashError> {
        // Dedup against the most recent item (ring[0]).
        if let Some(front) = self.ring.front() {
            if front.content_hash == item.content_hash {
                return Ok(front.id);
            }
        }

        // Assign the next sequential ID.
        let id = self.next_id;
        item.id = id;
        self.next_id += 1;

        // Evict oldest if at capacity.
        if self.ring.len() >= self.capacity {
            if let Some(evicted) = self.ring.pop_back() {
                self.mark_evicted(evicted.id);
            }
        }

        // Persist to DB (best-effort).
        self.persist_item(&item);

        // Push to front of ring.
        self.ring.push_front(item);

        // Reset active index.
        self.active_index = 0;

        Ok(id)
    }

    /// Returns a reference to the currently active clipboard item.
    pub fn active(&self) -> Option<&ClipboardItem> {
        self.ring.get(self.active_index)
    }

    /// Returns the current active index.
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Returns the next ID that will be assigned to a new item.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Returns a reference to the item at the given index.
    pub fn get(&self, index: usize) -> Option<&ClipboardItem> {
        self.ring.get(index)
    }

    /// Cycle the active index forward (toward older items), wrapping around.
    pub fn cycle_forward(&mut self) {
        if !self.ring.is_empty() {
            self.active_index = (self.active_index + 1) % self.ring.len();
        }
    }

    /// Cycle the active index backward (toward newer items), wrapping around.
    pub fn cycle_backward(&mut self) {
        if !self.ring.is_empty() {
            if self.active_index == 0 {
                self.active_index = self.ring.len() - 1;
            } else {
                self.active_index -= 1;
            }
        }
    }

    /// Set the active index, clamping to the valid range.
    pub fn set_active(&mut self, index: usize) {
        if self.ring.is_empty() {
            self.active_index = 0;
        } else {
            self.active_index = index.min(self.ring.len() - 1);
        }
    }

    /// Reset the active index to 0 (the most recent item).
    pub fn reset_active(&mut self) {
        self.active_index = 0;
    }

    /// Returns an enumerated iterator over all items in the ring buffer.
    pub fn items(&self) -> impl Iterator<Item = (usize, &ClipboardItem)> {
        self.ring.iter().enumerate()
    }

    /// Returns the number of items in the ring buffer.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Returns true if the ring buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Remove an item by its ID. Returns `true` if the item was found and
    /// removed, `false` otherwise. Adjusts `active_index` appropriately.
    pub fn remove(&mut self, id: u64) -> Result<bool, ClipStashError> {
        let pos = self.ring.iter().position(|item| item.id == id);

        match pos {
            Some(index) => {
                self.ring.remove(index);

                // Mark as evicted in DB.
                self.mark_evicted(id);

                // Adjust active_index.
                if self.ring.is_empty() {
                    self.active_index = 0;
                } else if self.active_index >= self.ring.len() {
                    self.active_index = self.ring.len() - 1;
                } else if index < self.active_index {
                    self.active_index -= 1;
                }

                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Clear all items from the ring buffer and mark them as evicted in the DB.
    pub fn clear(&mut self) -> Result<(), ClipStashError> {
        if let Some(ref db) = self.db {
            if let Err(e) = db.execute("UPDATE items SET evicted = 1 WHERE evicted = 0", []) {
                log::error!("Failed to mark items as evicted in DB: {e}");
            }
        }

        self.ring.clear();
        self.active_index = 0;

        Ok(())
    }

    /// Change the capacity of the ring buffer. Evicts excess items if the new
    /// capacity is smaller. Capacity is clamped to [1, MAX_CAPACITY].
    pub fn set_capacity(&mut self, new_capacity: usize) -> Result<(), ClipStashError> {
        let new_capacity = new_capacity.clamp(1, MAX_CAPACITY);
        self.capacity = new_capacity;

        while self.ring.len() > self.capacity {
            if let Some(evicted) = self.ring.pop_back() {
                self.mark_evicted(evicted.id);
            }
        }

        // Adjust active_index if it's now out of range.
        if !self.ring.is_empty() && self.active_index >= self.ring.len() {
            self.active_index = self.ring.len() - 1;
        } else if self.ring.is_empty() {
            self.active_index = 0;
        }

        Ok(())
    }

    /// Flush is a no-op for now; WAL mode auto-flushes.
    pub fn flush(&mut self) -> Result<(), ClipStashError> {
        Ok(())
    }

    // ── Private helpers ──

    fn create_schema(conn: &Connection) -> Result<(), ClipStashError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
                id INTEGER PRIMARY KEY,
                content BLOB NOT NULL,
                source_app TEXT,
                captured_at TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                content_hash BLOB NOT NULL,
                evicted INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_items_captured ON items(captured_at DESC);
            CREATE INDEX IF NOT EXISTS idx_items_hash ON items(content_hash);",
        )?;
        Ok(())
    }

    fn load_items(
        conn: &Connection,
        capacity: usize,
    ) -> Result<(VecDeque<ClipboardItem>, Option<u64>), ClipStashError> {
        // Get the max ID across all items (including evicted) for next_id.
        let max_id: Option<u64> = conn.query_row(
            "SELECT MAX(id) FROM items",
            [],
            |row| row.get(0),
        )?;

        // Load the most recent non-evicted items up to capacity.
        let mut stmt = conn.prepare(
            "SELECT id, content, source_app, captured_at, byte_size, content_hash
             FROM items
             WHERE evicted = 0
             ORDER BY captured_at DESC
             LIMIT ?1",
        )?;

        let items = stmt.query_map(params![capacity as i64], |row| {
            let id: u64 = row.get(0)?;
            let content_blob: Vec<u8> = row.get(1)?;
            let source_app: Option<String> = row.get(2)?;
            let captured_at_str: String = row.get(3)?;
            let byte_size: usize = row.get::<_, i64>(4)? as usize;
            let content_hash_blob: Vec<u8> = row.get(5)?;

            Ok((id, content_blob, source_app, captured_at_str, byte_size, content_hash_blob))
        })?;

        let mut ring = VecDeque::with_capacity(capacity);

        for item_result in items {
            let (id, content_blob, source_app, captured_at_str, byte_size, content_hash_blob) =
                item_result?;

            // Deserialize content from MessagePack.
            let wrapper: ContentWrapper = match rmp_serde::from_slice(&content_blob) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("Failed to deserialize item {id}: {e}");
                    continue;
                }
            };

            // Parse captured_at from ISO 8601.
            let captured_at = match captured_at_str.parse::<DateTime<Utc>>() {
                Ok(dt) => SystemTime::from(dt),
                Err(e) => {
                    log::error!("Failed to parse captured_at for item {id}: {e}");
                    SystemTime::now()
                }
            };

            // Reconstruct content_hash.
            let mut content_hash = [0u8; 32];
            if content_hash_blob.len() == 32 {
                content_hash.copy_from_slice(&content_hash_blob);
            } else {
                log::error!(
                    "Invalid content_hash length for item {id}: {}",
                    content_hash_blob.len()
                );
                content_hash = wrapper.content.content_hash();
            }

            let clip_item = ClipboardItem {
                id,
                content: wrapper.content,
                source_app,
                captured_at,
                byte_size,
                content_hash,
            };

            ring.push_back(clip_item);
        }

        Ok((ring, max_id))
    }

    fn persist_item(&self, item: &ClipboardItem) {
        if let Some(ref db) = self.db {
            let wrapper = ContentWrapper {
                content: item.content.clone(),
            };

            let content_blob = match rmp_serde::to_vec(&wrapper) {
                Ok(blob) => blob,
                Err(e) => {
                    log::error!("Failed to serialize item {}: {e}", item.id);
                    return;
                }
            };

            let captured_at: DateTime<Utc> = item.captured_at.into();
            let captured_at_str = captured_at.to_rfc3339();

            if let Err(e) = db.execute(
                "INSERT OR REPLACE INTO items (id, content, source_app, captured_at, byte_size, content_hash, evicted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    item.id as i64,
                    content_blob,
                    item.source_app,
                    captured_at_str,
                    item.byte_size as i64,
                    item.content_hash.as_slice(),
                ],
            ) {
                log::error!("Failed to persist item {}: {e}", item.id);
            }
        }
    }

    fn mark_evicted(&self, id: u64) {
        if let Some(ref db) = self.db {
            if let Err(e) = db.execute(
                "UPDATE items SET evicted = 1 WHERE id = ?1",
                params![id as i64],
            ) {
                log::error!("Failed to mark item {id} as evicted: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipstash_types::{ClipboardContent, ClipboardItem};
    use tempfile::tempdir;

    /// Helper: create an in-memory ClipStore with no persistence.
    fn mem_store(capacity: usize) -> ClipStore {
        ClipStore {
            ring: VecDeque::with_capacity(capacity),
            capacity: capacity.clamp(1, MAX_CAPACITY),
            active_index: 0,
            next_id: 1,
            db: None,
        }
    }

    /// Helper: create a persisted ClipStore using a temp directory.
    fn db_store(dir: &std::path::Path, capacity: usize) -> ClipStore {
        let db_path = dir.join("history.db");
        let conn = Connection::open(&db_path).expect("open db");
        ClipStore::new_with_connection(conn, capacity).expect("create store")
    }

    /// Helper: create a simple text ClipboardItem.
    fn text_item(text: &str) -> ClipboardItem {
        ClipboardItem::new(0, ClipboardContent::Text(text.into()), None)
    }

    #[test]
    fn push_5_items_into_capacity_5() {
        let mut store = mem_store(5);
        for i in 0..5 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }
        assert_eq!(store.len(), 5);
        // Most recent should be at index 0.
        assert!(matches!(
            &store.get(0).unwrap().content,
            ClipboardContent::Text(s) if s == "item 4"
        ));
        // Oldest should be at index 4.
        assert!(matches!(
            &store.get(4).unwrap().content,
            ClipboardContent::Text(s) if s == "item 0"
        ));
    }

    #[test]
    fn push_6th_into_capacity_5_evicts_oldest() {
        let mut store = mem_store(5);
        for i in 0..6 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }
        assert_eq!(store.len(), 5);
        // Oldest should now be "item 1" (item 0 was evicted).
        assert!(matches!(
            &store.get(4).unwrap().content,
            ClipboardContent::Text(s) if s == "item 1"
        ));
        // Newest should be "item 5".
        assert!(matches!(
            &store.get(0).unwrap().content,
            ClipboardContent::Text(s) if s == "item 5"
        ));
    }

    #[test]
    fn push_duplicate_of_most_recent_is_deduped() {
        let mut store = mem_store(5);
        store.push(text_item("hello")).unwrap();
        store.push(text_item("world")).unwrap();
        assert_eq!(store.len(), 2);

        // Push duplicate of ring[0] ("world").
        store.push(text_item("world")).unwrap();
        assert_eq!(store.len(), 2); // Should not change.
    }

    #[test]
    fn push_matching_ring2_but_not_ring0_is_inserted() {
        let mut store = mem_store(5);
        store.push(text_item("alpha")).unwrap();
        store.push(text_item("beta")).unwrap();
        store.push(text_item("gamma")).unwrap();
        assert_eq!(store.len(), 3);

        // "alpha" matches ring[2] but not ring[0] ("gamma"), so it should be inserted.
        store.push(text_item("alpha")).unwrap();
        assert_eq!(store.len(), 4);
    }

    #[test]
    fn cycle_forward_wraps() {
        let mut store = mem_store(5);
        for i in 0..3 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }

        assert_eq!(store.active_index, 0);
        store.cycle_forward();
        assert_eq!(store.active_index, 1);
        store.cycle_forward();
        assert_eq!(store.active_index, 2);
        store.cycle_forward();
        assert_eq!(store.active_index, 0); // Wrapped around.
    }

    #[test]
    fn cycle_backward_wraps() {
        let mut store = mem_store(5);
        for i in 0..3 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }

        assert_eq!(store.active_index, 0);
        store.cycle_backward();
        assert_eq!(store.active_index, 2); // Wrapped around.
        store.cycle_backward();
        assert_eq!(store.active_index, 1);
        store.cycle_backward();
        assert_eq!(store.active_index, 0);
    }

    #[test]
    fn set_active_clamps_to_valid_range() {
        let mut store = mem_store(5);
        for i in 0..5 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }

        store.set_active(99);
        assert_eq!(store.active_index, 4); // Clamped to len - 1.

        store.set_active(2);
        assert_eq!(store.active_index, 2);
    }

    #[test]
    fn push_resets_active_index() {
        let mut store = mem_store(5);
        store.push(text_item("a")).unwrap();
        store.push(text_item("b")).unwrap();
        store.push(text_item("c")).unwrap();

        store.set_active(2);
        assert_eq!(store.active_index, 2);

        store.push(text_item("d")).unwrap();
        assert_eq!(store.active_index, 0); // Reset after push.
    }

    #[test]
    fn set_capacity_shrinks_ring() {
        let mut store = mem_store(5);
        for i in 0..5 {
            store.push(text_item(&format!("item {i}"))).unwrap();
        }
        assert_eq!(store.len(), 5);

        store.set_capacity(2).unwrap();
        assert_eq!(store.len(), 2);

        // Should keep the 2 newest items.
        assert!(matches!(
            &store.get(0).unwrap().content,
            ClipboardContent::Text(s) if s == "item 4"
        ));
        assert!(matches!(
            &store.get(1).unwrap().content,
            ClipboardContent::Text(s) if s == "item 3"
        ));
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempdir().unwrap();

        // Create store, push items, then drop.
        {
            let mut store = db_store(dir.path(), 5);
            store.push(text_item("first")).unwrap();
            store.push(text_item("second")).unwrap();
            store.push(text_item("third")).unwrap();
        }

        // Recreate store from same DB, items should be present.
        {
            let store = db_store(dir.path(), 5);
            assert_eq!(store.len(), 3);
            // Most recent should be at index 0.
            assert!(matches!(
                &store.get(0).unwrap().content,
                ClipboardContent::Text(s) if s == "third"
            ));
            assert!(matches!(
                &store.get(1).unwrap().content,
                ClipboardContent::Text(s) if s == "second"
            ));
            assert!(matches!(
                &store.get(2).unwrap().content,
                ClipboardContent::Text(s) if s == "first"
            ));
        }
    }

    #[test]
    fn remove_correct_item() {
        let mut store = mem_store(5);
        let id1 = store.push(text_item("one")).unwrap();
        let id2 = store.push(text_item("two")).unwrap();
        let id3 = store.push(text_item("three")).unwrap();
        assert_eq!(store.len(), 3);

        let removed = store.remove(id2).unwrap();
        assert!(removed);
        assert_eq!(store.len(), 2);

        // Remaining items: "three" (id3) at 0, "one" (id1) at 1.
        assert_eq!(store.get(0).unwrap().id, id3);
        assert_eq!(store.get(1).unwrap().id, id1);

        // Removing nonexistent ID returns false.
        let removed = store.remove(9999).unwrap();
        assert!(!removed);
    }

    #[test]
    fn clear_empties_store() {
        let mut store = mem_store(5);
        store.push(text_item("a")).unwrap();
        store.push(text_item("b")).unwrap();
        store.push(text_item("c")).unwrap();
        assert_eq!(store.len(), 3);

        store.clear().unwrap();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.active_index, 0);
    }

    #[test]
    fn items_iterator() {
        let mut store = mem_store(5);
        store.push(text_item("a")).unwrap();
        store.push(text_item("b")).unwrap();
        store.push(text_item("c")).unwrap();

        let collected: Vec<(usize, &ClipboardItem)> = store.items().collect();
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].0, 0);
        assert_eq!(collected[1].0, 1);
        assert_eq!(collected[2].0, 2);
    }

    #[test]
    fn active_returns_correct_item() {
        let mut store = mem_store(5);
        store.push(text_item("x")).unwrap();
        store.push(text_item("y")).unwrap();

        assert!(matches!(
            &store.active().unwrap().content,
            ClipboardContent::Text(s) if s == "y"
        ));

        store.cycle_forward();
        assert!(matches!(
            &store.active().unwrap().content,
            ClipboardContent::Text(s) if s == "x"
        ));
    }

    #[test]
    fn empty_store_active_is_none() {
        let store = mem_store(5);
        assert!(store.active().is_none());
    }

    #[test]
    fn cycle_on_empty_store_does_not_panic() {
        let mut store = mem_store(5);
        store.cycle_forward();
        store.cycle_backward();
        assert_eq!(store.active_index, 0);
    }

    #[test]
    fn remove_adjusts_active_index() {
        let mut store = mem_store(5);
        store.push(text_item("a")).unwrap();
        store.push(text_item("b")).unwrap();
        store.push(text_item("c")).unwrap();
        // Ring: [c, b, a] at indices [0, 1, 2]

        store.set_active(2); // active points to "a"
        assert_eq!(store.active_index, 2);

        // Remove "b" at index 1 (before active_index=2).
        let id_b = store.get(1).unwrap().id;
        store.remove(id_b).unwrap();
        // Ring: [c, a] at indices [0, 1]
        // active_index should be decremented to 1.
        assert_eq!(store.active_index, 1);
    }

    #[test]
    fn persistence_respects_evicted_items() {
        let dir = tempdir().unwrap();

        // Create a capacity-2 store, push 3 items (evicts the first).
        {
            let mut store = db_store(dir.path(), 2);
            store.push(text_item("old")).unwrap();
            store.push(text_item("mid")).unwrap();
            store.push(text_item("new")).unwrap();
            assert_eq!(store.len(), 2);
        }

        // Reload: evicted "old" should not appear.
        {
            let store = db_store(dir.path(), 5);
            assert_eq!(store.len(), 2);
            assert!(matches!(
                &store.get(0).unwrap().content,
                ClipboardContent::Text(s) if s == "new"
            ));
            assert!(matches!(
                &store.get(1).unwrap().content,
                ClipboardContent::Text(s) if s == "mid"
            ));
        }
    }

    #[test]
    fn next_id_continues_after_reload() {
        let dir = tempdir().unwrap();

        let last_id;
        {
            let mut store = db_store(dir.path(), 5);
            store.push(text_item("a")).unwrap();
            store.push(text_item("b")).unwrap();
            last_id = store.push(text_item("c")).unwrap();
        }

        {
            let mut store = db_store(dir.path(), 5);
            let new_id = store.push(text_item("d")).unwrap();
            assert!(new_id > last_id, "next_id should continue from max existing id");
        }
    }

    #[test]
    fn flush_is_noop() {
        let mut store = mem_store(5);
        store.flush().unwrap(); // Should not panic.
    }

    #[test]
    fn set_capacity_clamps() {
        let mut store = mem_store(5);
        store.set_capacity(0).unwrap();
        assert_eq!(store.capacity, 1);

        store.set_capacity(999).unwrap();
        assert_eq!(store.capacity, MAX_CAPACITY);
    }

    #[test]
    fn reset_active() {
        let mut store = mem_store(5);
        store.push(text_item("a")).unwrap();
        store.push(text_item("b")).unwrap();
        store.push(text_item("c")).unwrap();

        store.set_active(2);
        assert_eq!(store.active_index, 2);

        store.reset_active();
        assert_eq!(store.active_index, 0);
    }
}
