//! User-driven component library.
//!
//! The agent populates this store at runtime: when the user shows it a
//! component (photo, datasheet, breakout module, …), the agent calls
//! `library.create` with a structured pad list + description, optionally
//! attaches the source photo / datasheet via `library.attach`, and
//! later spawns palette items by key with `palette.add_from_library`.
//!
//! On-disk layout (under `~/.pcb-library/`):
//!
//!   index.json
//!   attachments/<uuid>.<ext>
//!
//! Persistence is best-effort: every mutation writes the index back to
//! disk; errors are surfaced to the caller as `String`. The index lives
//! inside an `RwLock` so reads are cheap and writes serial.
//!
//! The store is process-global — one library is shared across every
//! `Project` opened on the same machine.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryPad {
    pub number: String,
    #[serde(default)]
    pub name: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub w_mm: f64,
    pub h_mm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// UUIDv4. Also the on-disk basename (extension follows the mime).
    pub id: String,
    /// What the agent thinks this file is — free text, but we suggest
    /// "photo", "datasheet", "note".
    pub kind: String,
    /// Original filename the agent sent. Purely human-facing.
    pub filename: String,
    /// MIME type ("image/jpeg", "application/pdf", "text/plain"…).
    pub mime: String,
    /// Unix seconds — kept simple to avoid a chrono dep for one field.
    pub added_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryEntry {
    /// Stable identifier (snake_case). Used by `palette.add_from_library`.
    pub key: String,
    /// What the part is + any orientation / role intent. The agent
    /// writes this when creating the entry, often after looking at an
    /// attached photo or datasheet.
    pub description: String,
    /// Suggested value for new symbols (e.g. "100nF", "ESP32-S3-Zero").
    #[serde(default)]
    pub default_value: String,
    /// Suggested rotation in degrees CCW when dropped on the board.
    #[serde(default)]
    pub default_rotation_deg: f32,
    /// True if this part should sit flush against a board edge (USB,
    /// screw terminal, antenna). Honoured by placement checks.
    #[serde(default)]
    pub edge_mounted: bool,
    pub pads: Vec<LibraryPad>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Unix seconds at creation.
    pub created_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LibraryIndex {
    #[serde(default)]
    entries: Vec<LibraryEntry>,
}

#[derive(Debug)]
pub struct Library {
    index_path: PathBuf,
    attachments_dir: PathBuf,
    inner: RwLock<LibraryIndex>,
}

/// Default location: `~/.pcb-library/`. Created on first access.
fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".pcb-library")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => "bin",
    }
}

impl Library {
    /// Open (or create) the default-location library.
    pub fn open_default() -> Result<Self, String> {
        Self::open_at(default_root())
    }

    pub fn open_at<P: AsRef<Path>>(root: P) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let attachments_dir = root.join("attachments");
        let index_path = root.join("index.json");
        fs::create_dir_all(&attachments_dir)
            .map_err(|e| format!("library: create {}: {e}", attachments_dir.display()))?;
        let index = match fs::read(&index_path) {
            Ok(bytes) => serde_json::from_slice::<LibraryIndex>(&bytes)
                .map_err(|e| format!("library: parse {}: {e}", index_path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => LibraryIndex::default(),
            Err(e) => return Err(format!("library: read {}: {e}", index_path.display())),
        };
        Ok(Self {
            index_path,
            attachments_dir,
            inner: RwLock::new(index),
        })
    }

    fn save(&self, index: &LibraryIndex) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(index)
            .map_err(|e| format!("library: serialise: {e}"))?;
        // Atomic-ish: write to .tmp then rename.
        let tmp = self.index_path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(|e| format!("library: write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.index_path)
            .map_err(|e| format!("library: rename {}: {e}", self.index_path.display()))?;
        Ok(())
    }

    pub fn list(&self) -> Vec<LibraryEntry> {
        let inner = self.inner.read().expect("library lock poisoned");
        inner.entries.clone()
    }

    pub fn find(&self, key: &str) -> Option<LibraryEntry> {
        let inner = self.inner.read().expect("library lock poisoned");
        inner.entries.iter().find(|e| e.key == key).cloned()
    }

    /// Insert or replace an entry by `key`. Replacing preserves any
    /// existing attachments unless the caller explicitly hands a new
    /// list — this lets the agent re-state pads / description without
    /// detaching files.
    pub fn upsert(&self, mut entry: LibraryEntry) -> Result<LibraryEntry, String> {
        if entry.key.is_empty() {
            return Err("library: key must not be empty".into());
        }
        if entry.created_at == 0 {
            entry.created_at = now_secs();
        }
        let mut inner = self.inner.write().expect("library lock poisoned");
        if let Some(existing) = inner.entries.iter().position(|e| e.key == entry.key) {
            // Preserve attachments from the existing entry if the
            // caller didn't override them.
            if entry.attachments.is_empty() {
                entry.attachments = inner.entries[existing].attachments.clone();
            }
            inner.entries[existing] = entry.clone();
        } else {
            inner.entries.push(entry.clone());
        }
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(entry)
    }

    pub fn delete(&self, key: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(pos) = inner.entries.iter().position(|e| e.key == key) else {
            return Ok(false);
        };
        // Drop attachments from disk too.
        for att in inner.entries[pos].attachments.clone() {
            let _ = fs::remove_file(self.attachment_path(&att));
        }
        inner.entries.remove(pos);
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Attach a binary blob to an existing entry. Stores the file under
    /// `attachments/<uuid>.<ext>` and records the metadata.
    pub fn attach(
        &self,
        key: &str,
        kind: String,
        filename: String,
        mime: String,
        data: &[u8],
    ) -> Result<Attachment, String> {
        let id = Uuid::new_v4().to_string();
        let path = self.attachments_dir.join(format!("{}.{}", id, ext_for_mime(&mime)));
        fs::write(&path, data)
            .map_err(|e| format!("library: write {}: {e}", path.display()))?;
        let att = Attachment {
            id,
            kind,
            filename,
            mime,
            added_at: now_secs(),
        };
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            // Roll back the file write so we don't leak orphans.
            let _ = fs::remove_file(&path);
            return Err(format!("library: no entry with key {key}"));
        };
        entry.attachments.push(att.clone());
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(att)
    }

    pub fn delete_attachment(&self, key: &str, attachment_id: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        let Some(pos) = entry.attachments.iter().position(|a| a.id == attachment_id) else {
            return Ok(false);
        };
        let att = entry.attachments.remove(pos);
        let _ = fs::remove_file(self.attachment_path(&att));
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Resolve an attachment's on-disk path. The file may not exist if
    /// the user manually nuked the attachments dir; callers handle the
    /// missing-file case.
    #[must_use]
    pub fn attachment_path(&self, att: &Attachment) -> PathBuf {
        self.attachments_dir
            .join(format!("{}.{}", att.id, ext_for_mime(&att.mime)))
    }

    /// Read the bytes of an attachment, or an error if it's missing.
    pub fn read_attachment(&self, att: &Attachment) -> Result<Vec<u8>, String> {
        let path = self.attachment_path(att);
        fs::read(&path).map_err(|e| format!("library: read {}: {e}", path.display()))
    }
}
