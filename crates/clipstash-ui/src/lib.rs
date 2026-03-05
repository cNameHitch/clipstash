use clipstash_store::ClipStore;
use clipstash_types::*;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker};
use objc2_app_kit::{
    NSMenu, NSMenuItem, NSPanel, NSStatusBar, NSStatusItem, NSVisualEffectView,
};
use objc2_foundation::NSString;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn truncate_preview(text: &str, max_len: usize) -> String {
    let single_line: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let mut end = max_len;
        while !trimmed.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &trimmed[..end])
    }
}

pub fn content_type_label(content: &ClipboardContent) -> &'static str {
    match content {
        ClipboardContent::Text(_) => "Text",
        ClipboardContent::RichText { .. } => "Rich Text",
        ClipboardContent::Image { .. } => "Image",
        ClipboardContent::FileRef(_) => "File",
        ClipboardContent::Url(_) => "URL",
        ClipboardContent::Binary { .. } => "Binary",
    }
}

fn preview_for_item(item: &ClipboardItem, max_len: usize) -> String {
    match &item.content {
        ClipboardContent::Text(s) => truncate_preview(s, max_len),
        ClipboardContent::RichText { plain, .. } => truncate_preview(plain, max_len),
        ClipboardContent::Image { width, height, format, .. } => {
            format!("[{:?} image {}x{}]", format, width, height)
        }
        ClipboardContent::FileRef(paths) => {
            if paths.len() == 1 {
                truncate_preview(&paths[0].to_string_lossy(), max_len)
            } else {
                format!("[{} files]", paths.len())
            }
        }
        ClipboardContent::Url(u) => truncate_preview(u, max_len),
        ClipboardContent::Binary { uti, bytes } => {
            format!("[Binary: {} ({} bytes)]", uti, bytes.len())
        }
    }
}

// ---------------------------------------------------------------------------
// CGEvent helpers
// ---------------------------------------------------------------------------

pub fn synthesize_paste() {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    const KEY_V: u16 = 0x09;

    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(src) => src,
        Err(_) => {
            log::error!("synthesize_paste: failed to create CGEventSource");
            return;
        }
    };

    let key_down = match CGEvent::new_keyboard_event(source.clone(), KEY_V, true) {
        Ok(ev) => ev,
        Err(_) => {
            log::error!("synthesize_paste: failed to create key-down event");
            return;
        }
    };

    let key_up = match CGEvent::new_keyboard_event(source, KEY_V, false) {
        Ok(ev) => ev,
        Err(_) => {
            log::error!("synthesize_paste: failed to create key-up event");
            return;
        }
    };

    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    key_up.set_flags(CGEventFlags::CGEventFlagCommand);
    key_down.post(CGEventTapLocation::HID);
    key_up.post(CGEventTapLocation::HID);

    log::debug!("synthesize_paste: Cmd+V posted");
}

// ---------------------------------------------------------------------------
// Global menu action callback
// ---------------------------------------------------------------------------

/// Actions dispatched by menu item clicks.
#[derive(Debug, Clone)]
pub enum MenuAction {
    SelectSlot(usize),
    ClearHistory,
    Quit,
}

type MenuCallback = Box<dyn Fn(MenuAction) + Send>;

static MENU_CALLBACK: Mutex<Option<MenuCallback>> = Mutex::new(None);

/// Set the global callback invoked when any menu item is clicked.
/// Must be called before setting up the UI.
pub fn set_menu_callback(cb: impl Fn(MenuAction) + Send + 'static) {
    *MENU_CALLBACK.lock().unwrap() = Some(Box::new(cb));
}

fn dispatch_menu_action(action: MenuAction) {
    if let Some(ref cb) = *MENU_CALLBACK.lock().unwrap() {
        cb(action);
    }
}

// ---------------------------------------------------------------------------
// MenuHandler — Objective-C target for menu item actions
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "CSMenuHandler"]
    pub struct MenuHandler;

    impl MenuHandler {
        #[unsafe(method(slotClicked:))]
        fn slot_clicked(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            log::debug!("Menu: slot {} clicked", tag);
            dispatch_menu_action(MenuAction::SelectSlot(tag as usize));
        }

        #[unsafe(method(clearHistory:))]
        fn clear_history(&self, _sender: &NSMenuItem) {
            log::debug!("Menu: clear history");
            dispatch_menu_action(MenuAction::ClearHistory);
        }

        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: &NSMenuItem) {
            log::debug!("Menu: quit");
            dispatch_menu_action(MenuAction::Quit);
        }
    }
);

// ---------------------------------------------------------------------------
// NSMenu construction helpers
// ---------------------------------------------------------------------------

fn ns_string(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn make_action_item(
    title: &str,
    action: objc2::runtime::Sel,
    target: &NSObject,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let mi = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &ns_string(title),
            Some(action),
            &ns_string(""),
        )
    };
    let _: () = unsafe { msg_send![&mi, setTarget: target] };
    mi
}

fn make_separator(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    NSMenuItem::separatorItem(mtm)
}

// ---------------------------------------------------------------------------
// MenuBarController
// ---------------------------------------------------------------------------

pub struct MenuBarController {
    status_item: Option<Retained<NSStatusItem>>,
    handler: Option<Retained<MenuHandler>>,
}

impl MenuBarController {
    pub fn new() -> Self {
        Self {
            status_item: None,
            handler: None,
        }
    }

    pub fn setup(&mut self, store: Arc<Mutex<ClipStore>>, config: &Config) {
        let mtm = MainThreadMarker::new()
            .expect("MenuBarController::setup must be called from the main thread");

        // Create the menu handler (Objective-C target for actions).
        let handler: Retained<MenuHandler> = unsafe { msg_send![MenuHandler::class(), new] };
        self.handler = Some(handler);

        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(-1.0);

        if let Some(button) = item.button(mtm) {
            let icon_name = ns_string("doc.on.clipboard");
            let image = objc2_app_kit::NSImage::imageNamed(&icon_name);
            if let Some(img) = image {
                button.setImage(Some(&img));
            } else {
                button.setTitle(&ns_string("CS"));
            }
        }

        self.status_item = Some(item);

        let guard = store.lock().expect("store lock poisoned");
        self.refresh(&guard, config);
    }

    pub fn refresh(&mut self, store: &ClipStore, config: &Config) {
        let Some(ref status_item) = self.status_item else {
            return;
        };
        let Some(ref handler) = self.handler else {
            return;
        };

        let mtm = MainThreadMarker::new()
            .expect("MenuBarController::refresh must be called from the main thread");

        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let active_idx = store.active_index();
        let target: &NSObject = handler.as_ref();

        if store.is_empty() {
            let empty = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    mtm.alloc::<NSMenuItem>(),
                    &ns_string("No clipboard history"),
                    None,
                    &ns_string(""),
                )
            };
            empty.setEnabled(false);
            menu.addItem(&empty);
        } else {
            for (i, item) in store.items() {
                let type_label = content_type_label(&item.content);
                let preview = preview_for_item(item, config.preview_length);
                let title = format!("{}: [{}] {}", i + 1, type_label, preview);
                let mi = make_action_item(&title, sel!(slotClicked:), target, mtm);
                mi.setTag(i as isize);

                if i == active_idx {
                    mi.setState(1); // NSControlStateValueOn
                }

                menu.addItem(&mi);
            }
        }

        menu.addItem(&make_separator(mtm));
        menu.addItem(&make_action_item("Clear History", sel!(clearHistory:), target, mtm));

        // Preferences — disabled placeholder for now.
        let prefs = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &ns_string("Preferences..."),
                None,
                &ns_string(""),
            )
        };
        prefs.setEnabled(false);
        menu.addItem(&prefs);

        menu.addItem(&make_separator(mtm));

        // About — disabled placeholder for now.
        let about = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &ns_string("About ClipStash"),
                None,
                &ns_string(""),
            )
        };
        about.setEnabled(false);
        menu.addItem(&about);

        menu.addItem(&make_action_item("Quit", sel!(quitApp:), target, mtm));

        status_item.setMenu(Some(&menu));
    }

    pub fn cleanup(&mut self) {
        if let Some(ref item) = self.status_item {
            let bar = NSStatusBar::systemStatusBar();
            bar.removeStatusItem(item);
        }
        self.status_item = None;
        self.handler = None;
        log::debug!("MenuBarController cleaned up");
    }
}

impl Default for MenuBarController {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OverlayController
// ---------------------------------------------------------------------------

const PANEL_WIDTH: f64 = 400.0;
const PANEL_HEIGHT: f64 = 500.0;

pub struct OverlayController {
    panel: Option<Retained<NSPanel>>,
    visible: bool,
}

impl OverlayController {
    pub fn new() -> Self {
        Self {
            panel: None,
            visible: false,
        }
    }

    pub fn toggle(&mut self, store: &ClipStore, config: &Config) {
        if self.visible {
            self.hide();
        } else {
            self.show(store, config);
        }
    }

    pub fn show(&mut self, store: &ClipStore, config: &Config) {
        use objc2_app_kit::{
            NSBackingStoreType, NSColor, NSFont, NSScreen, NSTextField,
            NSWindowStyleMask,
        };
        use objc2_foundation::NSRect;

        let mtm = MainThreadMarker::new()
            .expect("OverlayController::show must be called from the main thread");

        if let Some(ref panel) = self.panel {
            panel.orderOut(None);
        }

        let screen_frame = {
            let mouse = objc2_app_kit::NSEvent::mouseLocation();
            let screens = NSScreen::screens(mtm);
            let mut frame = NSRect::new(
                objc2_foundation::NSPoint::new(0.0, 0.0),
                objc2_foundation::NSSize::new(1920.0, 1080.0),
            );
            let count = screens.count();
            for idx in 0..count {
                let screen = screens.objectAtIndex(idx);
                let sf = screen.frame();
                if mouse.x >= sf.origin.x
                    && mouse.x <= sf.origin.x + sf.size.width
                    && mouse.y >= sf.origin.y
                    && mouse.y <= sf.origin.y + sf.size.height
                {
                    frame = sf;
                    break;
                }
            }
            frame
        };

        let origin_x =
            screen_frame.origin.x + (screen_frame.size.width - PANEL_WIDTH) / 2.0;
        let origin_y =
            screen_frame.origin.y + (screen_frame.size.height - PANEL_HEIGHT) / 2.0;

        let content_rect = NSRect::new(
            objc2_foundation::NSPoint::new(origin_x, origin_y),
            objc2_foundation::NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
        );

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::NonactivatingPanel;

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSPanel>(),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        );

        panel.setLevel(3);
        panel.setTitle(&ns_string("ClipStash"));

        let effect_frame = NSRect::new(
            objc2_foundation::NSPoint::new(0.0, 0.0),
            objc2_foundation::NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
        );
        let effect_view = NSVisualEffectView::initWithFrame(
            mtm.alloc::<NSVisualEffectView>(),
            effect_frame,
        );
        effect_view.setMaterial(objc2_app_kit::NSVisualEffectMaterial::HUDWindow);
        effect_view.setBlendingMode(
            objc2_app_kit::NSVisualEffectBlendingMode::BehindWindow,
        );
        effect_view.setState(objc2_app_kit::NSVisualEffectState::Active);
        effect_view.setWantsLayer(true);

        panel.setContentView(Some(&effect_view));

        let active_idx = store.active_index();
        let mut y_offset = PANEL_HEIGHT - 50.0;
        let label_height = 24.0;
        let padding = 8.0;

        let title_label = NSTextField::labelWithString(&ns_string("ClipStash"), mtm);
        title_label.setFrame(NSRect::new(
            objc2_foundation::NSPoint::new(16.0, y_offset),
            objc2_foundation::NSSize::new(PANEL_WIDTH - 32.0, 28.0),
        ));
        let bold_font = NSFont::boldSystemFontOfSize(18.0);
        title_label.setFont(Some(&bold_font));
        title_label.setTextColor(Some(&NSColor::labelColor()));
        title_label.setDrawsBackground(false);
        title_label.setBezeled(false);
        title_label.setEditable(false);
        title_label.setSelectable(false);
        effect_view.addSubview(&title_label);

        y_offset -= 36.0;

        if store.is_empty() {
            let empty_label = NSTextField::labelWithString(
                &ns_string("No clipboard history yet. Copy something to get started."),
                mtm,
            );
            empty_label.setFrame(NSRect::new(
                objc2_foundation::NSPoint::new(16.0, y_offset),
                objc2_foundation::NSSize::new(PANEL_WIDTH - 32.0, label_height),
            ));
            empty_label.setTextColor(Some(&NSColor::secondaryLabelColor()));
            empty_label.setDrawsBackground(false);
            empty_label.setBezeled(false);
            empty_label.setEditable(false);
            empty_label.setSelectable(false);
            effect_view.addSubview(&empty_label);
        } else {
            for (i, clip_item) in store.items() {
                if y_offset < padding {
                    break;
                }

                let type_label = content_type_label(&clip_item.content);
                let preview = preview_for_item(clip_item, config.preview_length);
                let active_marker = if i == active_idx { " *" } else { "" };
                let display = format!(
                    "{}: [{}] {}{}",
                    i + 1, type_label, preview, active_marker,
                );

                let label = NSTextField::labelWithString(&ns_string(&display), mtm);
                label.setFrame(NSRect::new(
                    objc2_foundation::NSPoint::new(16.0, y_offset),
                    objc2_foundation::NSSize::new(PANEL_WIDTH - 32.0, label_height),
                ));
                let item_font = NSFont::systemFontOfSize(13.0);
                label.setFont(Some(&item_font));

                if i == active_idx {
                    label.setTextColor(Some(&NSColor::controlAccentColor()));
                } else {
                    label.setTextColor(Some(&NSColor::labelColor()));
                }

                label.setDrawsBackground(false);
                label.setBezeled(false);
                label.setEditable(false);
                label.setSelectable(false);

                effect_view.addSubview(&label);
                y_offset -= label_height + padding;
            }
        }

        panel.makeKeyAndOrderFront(None);

        self.panel = Some(panel);
        self.visible = true;

        log::debug!("OverlayController: panel shown");
    }

    pub fn hide(&mut self) {
        if let Some(ref panel) = self.panel {
            panel.orderOut(None);
        }
        self.visible = false;
        log::debug!("OverlayController: panel hidden");
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn select_item(&mut self, index: usize) {
        log::info!("OverlayController: item {} selected", index);
        self.hide();
    }
}

impl Default for OverlayController {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// AppUI
// ---------------------------------------------------------------------------

pub struct AppUI {
    pub menu_bar: MenuBarController,
    pub overlay: OverlayController,
}

impl AppUI {
    pub fn new() -> Self {
        Self {
            menu_bar: MenuBarController::new(),
            overlay: OverlayController::new(),
        }
    }

    pub fn setup(&mut self, store: Arc<Mutex<ClipStore>>, config: &Config) {
        self.menu_bar.setup(store, config);
        log::info!("AppUI setup complete");
    }

    pub fn refresh(&mut self, store: &ClipStore, config: &Config) {
        self.menu_bar.refresh(store, config);
        if self.overlay.is_visible() {
            self.overlay.show(store, config);
        }
    }

    pub fn toggle_overlay(&mut self, store: &ClipStore, config: &Config) {
        self.overlay.toggle(store, config);
    }

    pub fn cleanup(&mut self) {
        self.overlay.hide();
        self.menu_bar.cleanup();
        log::info!("AppUI cleaned up");
    }
}

impl Default for AppUI {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate_preview("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_text() {
        let long = "a".repeat(100);
        let result = truncate_preview(&long, 10);
        assert_eq!(result, "aaaaaaaaaa...");
    }

    #[test]
    fn truncate_collapses_newlines() {
        let multi = "line1\nline2\nline3";
        let result = truncate_preview(multi, 100);
        assert_eq!(result, "line1 line2 line3");
    }

    #[test]
    fn truncate_handles_empty() {
        assert_eq!(truncate_preview("", 10), "");
    }

    #[test]
    fn truncate_multibyte_safe() {
        let emojis = "\u{1F600}\u{1F601}\u{1F602}";
        let result = truncate_preview(emojis, 5);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn content_type_labels() {
        assert_eq!(content_type_label(&ClipboardContent::Text("hi".into())), "Text");
        assert_eq!(
            content_type_label(&ClipboardContent::RichText { plain: String::new(), rtf: vec![] }),
            "Rich Text"
        );
        assert_eq!(
            content_type_label(&ClipboardContent::Image {
                bytes: vec![], format: ImageFormat::Png, width: 0, height: 0,
            }),
            "Image"
        );
        assert_eq!(content_type_label(&ClipboardContent::FileRef(vec![])), "File");
        assert_eq!(content_type_label(&ClipboardContent::Url(String::new())), "URL");
        assert_eq!(
            content_type_label(&ClipboardContent::Binary { uti: String::new(), bytes: vec![] }),
            "Binary"
        );
    }
}
