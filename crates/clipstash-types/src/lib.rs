use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

// ── Constants ──

pub const MAX_CAPACITY: usize = 10;
pub const DEFAULT_CAPACITY: usize = 5;
pub const MIN_POLL_INTERVAL_MS: u64 = 100;
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 250;
pub const MAX_ITEM_BYTES: usize = 50 * 1024 * 1024;

// ── Content Types ──

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Tiff,
    Gif,
    Bmp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    RichText { plain: String, rtf: Vec<u8> },
    Image { bytes: Vec<u8>, format: ImageFormat, width: u32, height: u32 },
    FileRef(Vec<PathBuf>),
    Url(String),
    Binary { uti: String, bytes: Vec<u8> },
}

impl ClipboardContent {
    /// Compute the byte size of the content payload.
    pub fn byte_size(&self) -> usize {
        match self {
            ClipboardContent::Text(s) => s.len(),
            ClipboardContent::RichText { plain, rtf } => plain.len() + rtf.len(),
            ClipboardContent::Image { bytes, .. } => bytes.len(),
            ClipboardContent::FileRef(paths) => {
                paths.iter().map(|p| p.as_os_str().len()).sum()
            }
            ClipboardContent::Url(s) => s.len(),
            ClipboardContent::Binary { uti, bytes } => uti.len() + bytes.len(),
        }
    }

    /// Compute the BLAKE3 hash of the content. The variant discriminant is
    /// included in the hash input so that e.g. Text("hello") and Url("hello")
    /// produce different hashes.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        match self {
            ClipboardContent::Text(s) => {
                hasher.update(b"text:");
                hasher.update(s.as_bytes());
            }
            ClipboardContent::RichText { plain, rtf } => {
                hasher.update(b"richtext:");
                hasher.update(plain.as_bytes());
                hasher.update(rtf);
            }
            ClipboardContent::Image { bytes, .. } => {
                hasher.update(b"image:");
                hasher.update(bytes);
            }
            ClipboardContent::FileRef(paths) => {
                hasher.update(b"fileref:");
                for p in paths {
                    hasher.update(p.to_string_lossy().as_bytes());
                    hasher.update(b"\0");
                }
            }
            ClipboardContent::Url(s) => {
                hasher.update(b"url:");
                hasher.update(s.as_bytes());
            }
            ClipboardContent::Binary { uti, bytes } => {
                hasher.update(b"binary:");
                hasher.update(uti.as_bytes());
                hasher.update(bytes);
            }
        }
        *hasher.finalize().as_bytes()
    }
}

// ── Clipboard Item ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub id: u64,
    pub content: ClipboardContent,
    pub source_app: Option<String>,
    pub captured_at: SystemTime,
    pub byte_size: usize,
    pub content_hash: [u8; 32],
}

impl ClipboardItem {
    /// Create a new ClipboardItem from content. Computes byte_size and content_hash automatically.
    pub fn new(id: u64, content: ClipboardContent, source_app: Option<String>) -> Self {
        let byte_size = content.byte_size();
        let content_hash = content.content_hash();
        Self {
            id,
            content,
            source_app,
            captured_at: SystemTime::now(),
            byte_size,
            content_hash,
        }
    }
}

// ── Config ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_capacity")]
    pub capacity: usize,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_picker_hotkey")]
    pub picker_hotkey: String,
    #[serde(default = "default_true")]
    pub direct_slot_hotkeys: bool,
    #[serde(default = "default_cycle_forward")]
    pub cycle_forward_hotkey: String,
    #[serde(default = "default_cycle_backward")]
    pub cycle_backward_hotkey: String,
    #[serde(default = "default_true")]
    pub persist: bool,
    #[serde(default = "default_max_item_bytes")]
    pub max_item_bytes: usize,
    #[serde(default)]
    pub launch_at_login: bool,
    #[serde(default = "default_true")]
    pub show_previews: bool,
    #[serde(default = "default_preview_length")]
    pub preview_length: usize,
    #[serde(default)]
    pub sound_on_capture: bool,
    #[serde(default = "default_true")]
    pub auto_paste: bool,
}

fn default_capacity() -> usize { DEFAULT_CAPACITY }
fn default_poll_interval() -> u64 { DEFAULT_POLL_INTERVAL_MS }
fn default_picker_hotkey() -> String { "cmd+shift+v".into() }
fn default_true() -> bool { true }
fn default_cycle_forward() -> String { "cmd+shift+]".into() }
fn default_cycle_backward() -> String { "cmd+shift+[".into() }
fn default_max_item_bytes() -> usize { MAX_ITEM_BYTES }
fn default_preview_length() -> usize { 80 }

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            picker_hotkey: "cmd+shift+v".into(),
            direct_slot_hotkeys: true,
            cycle_forward_hotkey: "cmd+shift+]".into(),
            cycle_backward_hotkey: "cmd+shift+[".into(),
            persist: true,
            max_item_bytes: MAX_ITEM_BYTES,
            launch_at_login: false,
            show_previews: true,
            preview_length: 80,
            sound_on_capture: false,
            auto_paste: true,
        }
    }
}

impl Config {
    /// Load config from the standard path, or return defaults.
    pub fn load() -> Result<Self, ClipStashError> {
        let path = Self::config_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| ClipStashError::ConfigError(format!("Failed to read config: {e}")))?;
            let mut config: Config = toml::from_str(&contents)
                .map_err(|e| ClipStashError::ConfigError(format!("Malformed config: {e}")))?;
            config.clamp();
            Ok(config)
        } else {
            log::info!("No config file at {}, using defaults", path.display());
            Ok(Self::default())
        }
    }

    /// Standard config file path: ~/.config/clipstash/config.toml
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("clipstash")
            .join("config.toml")
    }

    /// Standard data directory: ~/.local/share/clipstash/
    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("clipstash")
    }

    /// Clamp values to valid ranges.
    pub fn clamp(&mut self) {
        if self.capacity < 1 {
            log::warn!("Config capacity {} clamped to 1", self.capacity);
            self.capacity = 1;
        } else if self.capacity > MAX_CAPACITY {
            log::warn!("Config capacity {} clamped to {}", self.capacity, MAX_CAPACITY);
            self.capacity = MAX_CAPACITY;
        }
        if self.poll_interval_ms < MIN_POLL_INTERVAL_MS {
            log::warn!(
                "Config poll_interval_ms {} clamped to {}",
                self.poll_interval_ms,
                MIN_POLL_INTERVAL_MS
            );
            self.poll_interval_ms = MIN_POLL_INTERVAL_MS;
        } else if self.poll_interval_ms > 2000 {
            log::warn!("Config poll_interval_ms {} clamped to 2000", self.poll_interval_ms);
            self.poll_interval_ms = 2000;
        }
    }
}

// ── Errors ──

#[derive(Debug, thiserror::Error)]
pub enum ClipStashError {
    #[error("Pasteboard access failed: {0}")]
    PasteboardError(String),

    #[error("Item exceeds maximum size ({size} > {max})")]
    ItemTooLarge { size: usize, max: usize },

    #[error("Unsupported pasteboard type: {uti}")]
    UnsupportedType { uti: String },

    #[error("Persistence error: {0}")]
    StorageError(#[from] rusqlite::Error),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Hotkey registration failed: {0}")]
    HotkeyError(String),

    #[error("UI error: {0}")]
    UiError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_serde_roundtrip() {
        let content = ClipboardContent::Text("hello world".into());
        let serialized = serde_json::to_string(&content).unwrap();
        let deserialized: ClipboardContent = serde_json::from_str(&serialized).unwrap();
        assert_eq!(content, deserialized);
    }

    #[test]
    fn image_zero_byte_payload_valid() {
        let content = ClipboardContent::Image {
            bytes: vec![],
            format: ImageFormat::Png,
            width: 0,
            height: 0,
        };
        assert_eq!(content.byte_size(), 0);
    }

    #[test]
    fn config_capacity_clamp_zero() {
        let mut config = Config { capacity: 0, ..Config::default() };
        config.clamp();
        assert_eq!(config.capacity, 1);
    }

    #[test]
    fn config_capacity_clamp_high() {
        let mut config = Config { capacity: 99, ..Config::default() };
        config.clamp();
        assert_eq!(config.capacity, MAX_CAPACITY);
    }

    #[test]
    fn config_poll_interval_clamp_low() {
        let mut config = Config { poll_interval_ms: 50, ..Config::default() };
        config.clamp();
        assert_eq!(config.poll_interval_ms, MIN_POLL_INTERVAL_MS);
    }

    #[test]
    fn byte_size_correct_for_text() {
        let content = ClipboardContent::Text("hello".into());
        assert_eq!(content.byte_size(), 5);
    }

    #[test]
    fn byte_size_correct_for_richtext() {
        let content = ClipboardContent::RichText {
            plain: "hi".into(),
            rtf: vec![1, 2, 3],
        };
        assert_eq!(content.byte_size(), 5);
    }

    #[test]
    fn byte_size_correct_for_image() {
        let content = ClipboardContent::Image {
            bytes: vec![0; 100],
            format: ImageFormat::Jpeg,
            width: 10,
            height: 10,
        };
        assert_eq!(content.byte_size(), 100);
    }

    #[test]
    fn byte_size_correct_for_url() {
        let content = ClipboardContent::Url("https://example.com".into());
        assert_eq!(content.byte_size(), 19);
    }

    #[test]
    fn byte_size_correct_for_binary() {
        let content = ClipboardContent::Binary {
            uti: "com.test".into(),
            bytes: vec![0; 50],
        };
        assert_eq!(content.byte_size(), 58); // 8 + 50
    }

    #[test]
    fn identical_content_same_hash() {
        let a = ClipboardContent::Text("hello".into());
        let b = ClipboardContent::Text("hello".into());
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn text_vs_url_different_hash() {
        let text = ClipboardContent::Text("hello".into());
        let url = ClipboardContent::Url("hello".into());
        assert_ne!(text.content_hash(), url.content_hash());
    }

    #[test]
    fn clipboard_item_new_computes_fields() {
        let content = ClipboardContent::Text("test".into());
        let expected_size = content.byte_size();
        let expected_hash = content.content_hash();
        let item = ClipboardItem::new(1, content, Some("com.test".into()));
        assert_eq!(item.byte_size, expected_size);
        assert_eq!(item.content_hash, expected_hash);
        assert_eq!(item.id, 1);
    }

    #[test]
    fn messagepack_roundtrip() {
        let content = ClipboardContent::Image {
            bytes: vec![1, 2, 3, 4],
            format: ImageFormat::Png,
            width: 100,
            height: 200,
        };
        let packed = rmp_serde::to_vec(&content).unwrap();
        let unpacked: ClipboardContent = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(content, unpacked);
    }
}
