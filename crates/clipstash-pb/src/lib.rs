use clipstash_types::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeRTF,
    NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardTypeURL, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::{NSData, NSString, NSURL};

/// Helper to access the extern-static pasteboard type constants inside an
/// unsafe block, since objc2 extern statics require it.
macro_rules! pb_type {
    (PNG) => { unsafe { NSPasteboardTypePNG } };
    (TIFF) => { unsafe { NSPasteboardTypeTIFF } };
    (RTF) => { unsafe { NSPasteboardTypeRTF } };
    (URL) => { unsafe { NSPasteboardTypeURL } };
    (STRING) => { unsafe { NSPasteboardTypeString } };
    (FILE_URL) => { unsafe { NSPasteboardTypeFileURL } };
}

pub struct PasteboardMonitor {
    last_change_count: isize,
    interval: Duration,
    max_item_bytes: usize,
    next_id: AtomicU64,
}

impl PasteboardMonitor {
    /// Create a new monitor with the given poll interval and maximum item size in bytes.
    pub fn new(interval: Duration, max_item_bytes: usize) -> Self {
        let pb = NSPasteboard::generalPasteboard();
        let change_count = pb.changeCount();
        Self {
            last_change_count: change_count,
            interval,
            max_item_bytes,
            next_id: AtomicU64::new(1),
        }
    }

    /// Set the next ID that will be assigned to captured items.
    pub fn set_next_id(&self, id: u64) {
        self.next_id.store(id, Ordering::SeqCst);
    }

    /// Return the configured poll interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Poll the system pasteboard for changes. Returns `Ok(Some(item))` if new
    /// content was detected, `Ok(None)` if nothing changed (or the pasteboard
    /// was cleared), and `Err` on failure.
    pub fn poll(&mut self) -> Result<Option<ClipboardItem>, ClipStashError> {
        let pb = NSPasteboard::generalPasteboard();
        let change_count = pb.changeCount();

        if change_count == self.last_change_count {
            return Ok(None);
        }

        // Something changed -- update tracking first so that even if we bail
        // out below we don't keep re-processing the same change.
        self.last_change_count = change_count;

        self.read_item_from_pasteboard(&pb)
    }

    /// Write a `ClipboardItem` back to the system pasteboard.
    pub fn write_to_pasteboard(&mut self, item: &ClipboardItem) -> Result<(), ClipStashError> {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();

        match &item.content {
            ClipboardContent::Text(s) => {
                let ns_string = NSString::from_str(s);
                let ok = pb.setString_forType(&ns_string, pb_type!(STRING));
                if !ok {
                    return Err(ClipStashError::PasteboardError(
                        "Failed to write text to pasteboard".into(),
                    ));
                }
            }
            ClipboardContent::RichText { plain, rtf } => {
                // Write RTF data.
                let rtf_data = NSData::from_vec(rtf.clone());
                let ok = pb.setData_forType(Some(&rtf_data), pb_type!(RTF));
                if !ok {
                    return Err(ClipStashError::PasteboardError(
                        "Failed to write RTF to pasteboard".into(),
                    ));
                }
                // Also set the plain text fallback.
                let ns_plain = NSString::from_str(plain);
                pb.setString_forType(&ns_plain, pb_type!(STRING));
            }
            ClipboardContent::Image { bytes, format, .. } => {
                let jpeg_type = NSString::from_str("public.jpeg");
                let pasteboard_type = match format {
                    ImageFormat::Png => pb_type!(PNG),
                    ImageFormat::Tiff => pb_type!(TIFF),
                    ImageFormat::Jpeg => &*jpeg_type,
                    _ => pb_type!(PNG), // fallback
                };
                let data = NSData::from_vec(bytes.clone());
                let ok = pb.setData_forType(Some(&data), pasteboard_type);
                if !ok {
                    return Err(ClipStashError::PasteboardError(
                        "Failed to write image to pasteboard".into(),
                    ));
                }
            }
            ClipboardContent::Url(url_str) => {
                let ns_string = NSString::from_str(url_str);
                let ok = pb.setString_forType(&ns_string, pb_type!(URL));
                if !ok {
                    return Err(ClipStashError::PasteboardError(
                        "Failed to write URL to pasteboard".into(),
                    ));
                }
                // Also set as plain string for compatibility.
                pb.setString_forType(&ns_string, pb_type!(STRING));
            }
            ClipboardContent::FileRef(paths) => {
                let ns_urls: Vec<_> = paths
                    .iter()
                    .map(|p| {
                        let s = NSString::from_str(&p.to_string_lossy());
                        NSURL::fileURLWithPath(&s)
                    })
                    .collect();
                if ns_urls.is_empty() {
                    return Err(ClipStashError::PasteboardError(
                        "No valid file paths to write".into(),
                    ));
                }
                // Write the first file URL as a string.
                let first_url = &ns_urls[0];
                let url_string = first_url.absoluteString();
                if let Some(url_string) = url_string {
                    let ok = pb.setString_forType(&url_string, pb_type!(FILE_URL));
                    if !ok {
                        return Err(ClipStashError::PasteboardError(
                            "Failed to write file URL to pasteboard".into(),
                        ));
                    }
                }
            }
            ClipboardContent::Binary { uti, bytes } => {
                let pb_type = NSString::from_str(uti);
                let data = NSData::from_vec(bytes.clone());
                let ok = pb.setData_forType(Some(&data), &pb_type);
                if !ok {
                    return Err(ClipStashError::PasteboardError(
                        "Failed to write binary data to pasteboard".into(),
                    ));
                }
            }
        }

        // Update change count to avoid a feedback loop on next poll.
        self.last_change_count = pb.changeCount();
        Ok(())
    }

    /// Read the current pasteboard content without updating change tracking.
    /// Useful for initial load or peek operations.
    pub fn read_current(&self) -> Result<Option<ClipboardItem>, ClipStashError> {
        let pb = NSPasteboard::generalPasteboard();
        self.read_item_from_pasteboard(&pb)
    }

    // -- Private helpers --

    /// Shared logic for reading an item from the given pasteboard reference.
    fn read_item_from_pasteboard(
        &self,
        pb: &NSPasteboard,
    ) -> Result<Option<ClipboardItem>, ClipStashError> {
        let types = pb.types();
        let types = match types {
            Some(t) => t,
            None => return Ok(None),
        };

        let content = self.select_richest_content(pb, &types)?;
        let content = match content {
            Some(c) => c,
            None => return Ok(None),
        };

        // Enforce size limit.
        let size = content.byte_size();
        if size > self.max_item_bytes {
            return Err(ClipStashError::ItemTooLarge {
                size,
                max: self.max_item_bytes,
            });
        }

        let source_app = Self::detect_source_app();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let item = ClipboardItem::new(id, content, source_app);

        Ok(Some(item))
    }

    /// Walk the available pasteboard types in richness order and return the
    /// best content variant we can construct.
    fn select_richest_content(
        &self,
        pb: &NSPasteboard,
        types: &objc2_foundation::NSArray<NSString>,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        let type_vec = types.to_vec();
        let has_type = |target: &NSString| -> bool {
            type_vec.iter().any(|t| t.isEqualToString(target))
        };

        // 1. Images: PNG > TIFF > JPEG
        if has_type(pb_type!(PNG)) {
            if let Some(content) = self.read_image(pb, pb_type!(PNG), ImageFormat::Png)? {
                return Ok(Some(content));
            }
        }
        if has_type(pb_type!(TIFF)) {
            if let Some(content) = self.read_image(pb, pb_type!(TIFF), ImageFormat::Tiff)? {
                return Ok(Some(content));
            }
        }
        let jpeg_type = NSString::from_str("public.jpeg");
        if has_type(&jpeg_type) {
            if let Some(content) = self.read_image(pb, &jpeg_type, ImageFormat::Jpeg)? {
                return Ok(Some(content));
            }
        }

        // 2. RTF
        if has_type(pb_type!(RTF)) {
            if let Some(content) = self.read_rtf(pb)? {
                return Ok(Some(content));
            }
        }

        // 3. URL
        if has_type(pb_type!(URL)) {
            if let Some(content) = self.read_url(pb)? {
                return Ok(Some(content));
            }
        }

        // 4. Plain string
        if has_type(pb_type!(STRING)) {
            if let Some(content) = self.read_plain_text(pb)? {
                return Ok(Some(content));
            }
        }

        // 5. File URLs
        if has_type(pb_type!(FILE_URL)) {
            if let Some(content) = self.read_file_urls(pb)? {
                return Ok(Some(content));
            }
        }

        // 6. Fallback: try to grab the first type as binary.
        if !type_vec.is_empty() {
            let first_type = &type_vec[0];
            let data = pb.dataForType(first_type);
            if let Some(data) = data {
                let bytes = data.to_vec();
                if !bytes.is_empty() {
                    return Ok(Some(ClipboardContent::Binary {
                        uti: first_type.to_string(),
                        bytes,
                    }));
                }
            }
        }

        Ok(None)
    }

    fn read_image(
        &self,
        pb: &NSPasteboard,
        pb_type: &NSString,
        format: ImageFormat,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        let data = pb.dataForType(pb_type);
        let data = match data {
            Some(d) => d,
            None => return Ok(None),
        };
        let bytes = data.to_vec();
        if bytes.is_empty() {
            return Ok(None);
        }

        // We don't decode the image to get dimensions here -- that would
        // require an image decoding library. Store 0x0 and let the UI
        // layer decode lazily if it needs to display the image.
        Ok(Some(ClipboardContent::Image {
            bytes,
            format,
            width: 0,
            height: 0,
        }))
    }

    fn read_rtf(
        &self,
        pb: &NSPasteboard,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        let rtf_data = pb.dataForType(pb_type!(RTF));
        let rtf_data = match rtf_data {
            Some(d) => d,
            None => return Ok(None),
        };
        let rtf_bytes = rtf_data.to_vec();
        if rtf_bytes.is_empty() {
            return Ok(None);
        }

        // Also grab the plain-text representation if available.
        let plain = pb.stringForType(pb_type!(STRING))
            .map(|s| s.to_string())
            .unwrap_or_default();

        Ok(Some(ClipboardContent::RichText {
            plain,
            rtf: rtf_bytes,
        }))
    }

    fn read_url(
        &self,
        pb: &NSPasteboard,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        let url_string = pb.stringForType(pb_type!(URL));
        match url_string {
            Some(s) => {
                let s = s.to_string();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ClipboardContent::Url(s)))
                }
            }
            None => Ok(None),
        }
    }

    fn read_plain_text(
        &self,
        pb: &NSPasteboard,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        let string = pb.stringForType(pb_type!(STRING));
        match string {
            Some(s) => {
                let s = s.to_string();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ClipboardContent::Text(s)))
                }
            }
            None => Ok(None),
        }
    }

    fn read_file_urls(
        &self,
        pb: &NSPasteboard,
    ) -> Result<Option<ClipboardContent>, ClipStashError> {
        // The pasteboard stores file URLs as strings with the NSPasteboardTypeFileURL type.
        // Each pasteboard item can carry one file URL; iterate all items to collect them.
        let items = pb.pasteboardItems();
        let items = match items {
            Some(i) => i,
            None => return Ok(None),
        };

        let item_vec = items.to_vec();
        let mut paths: Vec<PathBuf> = Vec::new();
        for item in &item_vec {
            let url_str = item.stringForType(pb_type!(FILE_URL));
            if let Some(url_ns) = url_str {
                let url_str: String = url_ns.to_string();
                // File URLs come as file:///... -- convert to a local path.
                if let Some(path_str) = url_str.strip_prefix("file://") {
                    let decoded = percent_decode(path_str);
                    paths.push(PathBuf::from(decoded));
                } else {
                    paths.push(PathBuf::from(&url_str));
                }
            }
        }

        if paths.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ClipboardContent::FileRef(paths)))
        }
    }

    /// Detect the frontmost application's bundle identifier.
    fn detect_source_app() -> Option<String> {
        let workspace = NSWorkspace::sharedWorkspace();
        let app: Option<objc2::rc::Retained<NSRunningApplication>> =
            workspace.frontmostApplication();
        let app = app?;
        let bundle_id = app.bundleIdentifier();
        bundle_id.map(|id| id.to_string())
    }
}

/// Simple percent-decoding for file URL paths.
fn percent_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(hi), Some(lo)) = (hi, lo) {
                let hex = [hi, lo];
                if let Ok(s) = std::str::from_utf8(&hex) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        result.push(val as char);
                        continue;
                    }
                }
            }
            // Malformed percent-encoding -- just push the percent sign.
            result.push('%');
        } else {
            result.push(b as char);
        }
    }
    result
}
