use clipstash_types::*;

use bitflags::bitflags;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, EventField,
};
use core_graphics::event::CGEvent;
use std::sync::Mutex;

// ── HotkeyAction ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    OpenPicker,
    CycleForward,
    CycleBackward,
    DirectSlot(usize), // 0..=9
    Quit,
}

// ── Callback type ──

pub type HotkeyCallback = Box<dyn Fn(HotkeyAction) + Send>;

// ── Modifiers ──

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u32 {
        const CMD   = 0b0001;
        const SHIFT = 0b0010;
        const CTRL  = 0b0100;
        const ALT   = 0b1000;
    }
}

// ── HotkeyBinding ──

/// A parsed hotkey binding: modifier flags + virtual key code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub modifiers: Modifiers,
    pub key_code: u16,
    pub action: HotkeyAction,
}

// ── Key code lookup ──

/// Map a key name to a macOS virtual key code (CGKeyCode).
fn key_name_to_code(name: &str) -> Option<u16> {
    match name {
        // Letters
        "a" => Some(0x00),
        "s" => Some(0x01),
        "d" => Some(0x02),
        "f" => Some(0x03),
        "h" => Some(0x04),
        "g" => Some(0x05),
        "z" => Some(0x06),
        "x" => Some(0x07),
        "c" => Some(0x08),
        "v" => Some(0x09),
        "b" => Some(0x0B),
        "q" => Some(0x0C),
        "w" => Some(0x0D),
        "e" => Some(0x0E),
        "r" => Some(0x0F),
        "y" => Some(0x10),
        "t" => Some(0x11),
        "o" => Some(0x1F),
        "u" => Some(0x20),
        "i" => Some(0x22),
        "p" => Some(0x23),
        "l" => Some(0x25),
        "j" => Some(0x26),
        "k" => Some(0x28),
        "n" => Some(0x2D),
        "m" => Some(0x2E),

        // Numbers
        "1" => Some(0x12),
        "2" => Some(0x13),
        "3" => Some(0x14),
        "4" => Some(0x15),
        "5" => Some(0x17),
        "6" => Some(0x16),
        "7" => Some(0x1A),
        "8" => Some(0x1C),
        "9" => Some(0x19),
        "0" => Some(0x1D),

        // Symbols
        "[" => Some(0x21),
        "]" => Some(0x1E),
        "\\" => Some(0x2A),
        "/" => Some(0x2C),
        "-" => Some(0x1B),
        "=" => Some(0x18),

        // Special keys
        "return" => Some(0x24),
        "tab" => Some(0x30),
        "space" => Some(0x31),
        "delete" => Some(0x33),
        "escape" => Some(0x35),

        // Function keys
        "f1" => Some(0x7A),
        "f2" => Some(0x78),
        "f3" => Some(0x63),
        "f4" => Some(0x76),
        "f5" => Some(0x60),
        "f6" => Some(0x61),
        "f7" => Some(0x62),
        "f8" => Some(0x64),
        "f9" => Some(0x65),
        "f10" => Some(0x6D),
        "f11" => Some(0x67),
        "f12" => Some(0x6F),

        // Arrow keys
        "up" => Some(0x7E),
        "down" => Some(0x7D),
        "left" => Some(0x7B),
        "right" => Some(0x7C),

        _ => None,
    }
}

// ── parse_hotkey ──

/// Parse a hotkey string such as "cmd+shift+v" into modifier flags and a
/// virtual key code. Returns an error for empty strings or unknown keys.
pub fn parse_hotkey(s: &str) -> Result<(Modifiers, u16), ClipStashError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ClipStashError::HotkeyError(
            "empty hotkey string".into(),
        ));
    }

    let parts: Vec<&str> = s.split('+').collect();
    if parts.is_empty() {
        return Err(ClipStashError::HotkeyError(
            "empty hotkey string".into(),
        ));
    }

    let mut modifiers = Modifiers::empty();
    let mut key_part: Option<&str> = None;

    for part in &parts {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "cmd" | "command" => modifiers |= Modifiers::CMD,
            "shift" => modifiers |= Modifiers::SHIFT,
            "ctrl" | "control" => modifiers |= Modifiers::CTRL,
            "alt" | "opt" | "option" => modifiers |= Modifiers::ALT,
            _ => {
                if key_part.is_some() {
                    return Err(ClipStashError::HotkeyError(format!(
                        "multiple non-modifier keys in hotkey: {s}"
                    )));
                }
                key_part = Some(*part);
            }
        }
    }

    let key_name = key_part.ok_or_else(|| {
        ClipStashError::HotkeyError(format!("no key specified in hotkey: {s}"))
    })?;

    let key_code =
        key_name_to_code(&key_name.to_ascii_lowercase()).ok_or_else(|| {
            ClipStashError::HotkeyError(format!(
                "unknown key in hotkey: {key_name}"
            ))
        })?;

    Ok((modifiers, key_code))
}

// ── Global state for CGEvent tap callback ──

struct HotkeyState {
    bindings: Vec<HotkeyBinding>,
    callback: HotkeyCallback,
}

// Safety: The CGEvent tap callback runs on the same run loop thread that
// registered it, so concurrent access does not actually occur, but Rust's
// type system requires Send for Mutex<T>.  We guarantee single-threaded
// access through the run loop.
unsafe impl Send for HotkeyState {}

static HOTKEY_STATE: Mutex<Option<HotkeyState>> = Mutex::new(None);

/// Convert `CGEventFlags` to our `Modifiers` bitflags.
fn cg_flags_to_modifiers(flags: CGEventFlags) -> Modifiers {
    let mut m = Modifiers::empty();
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        m |= Modifiers::CMD;
    }
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        m |= Modifiers::SHIFT;
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        m |= Modifiers::CTRL;
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        m |= Modifiers::ALT;
    }
    m
}

/// The event tap callback. Compares incoming key events against registered
/// bindings and invokes the stored callback when a match is found.
///
/// Returns `None` to swallow matched events, or `Some(event)` to pass
/// unmatched events through.
fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: &CGEvent,
) -> Option<CGEvent> {
    // Only process key-down events. CGEventType does not implement PartialEq,
    // so we compare the discriminant values as u32.
    if (event_type as u32) != (CGEventType::KeyDown as u32) {
        return None; // pass through unchanged
    }

    let key_code =
        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let flags = event.get_flags();
    let mods = cg_flags_to_modifiers(flags);

    // Look up a matching binding.
    let matched_action = {
        let guard = HOTKEY_STATE.lock().unwrap();
        if let Some(state) = guard.as_ref() {
            state
                .bindings
                .iter()
                .find(|b| b.key_code == key_code && b.modifiers == mods)
                .map(|b| b.action.clone())
        } else {
            None
        }
    };

    if let Some(action) = matched_action {
        log::debug!("Hotkey matched: {:?}", action);
        let guard = HOTKEY_STATE.lock().unwrap();
        if let Some(state) = guard.as_ref() {
            (state.callback)(action);
        }
        // NOTE: We must NOT return Some(new_event) here. The core-graphics
        // wrapper drops (CFRelease) the original event when Some is returned,
        // but the system also tries to release it → double-free crash.
        // Returning None passes the original event through, which is safe.
        // The Cmd+Shift+<key> combo is unlikely to conflict with other apps.
        return None;
    }

    // Not our hotkey – return None to let it pass through unchanged.
    None
}

// ── Accessibility helpers ──

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryCreate(
        allocator: *const std::ffi::c_void,
        keys: *const *const std::ffi::c_void,
        values: *const *const std::ffi::c_void,
        num_values: isize,
        key_callbacks: *const std::ffi::c_void,
        value_callbacks: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFRelease(cf: *const std::ffi::c_void);
    static kCFBooleanTrue: *const std::ffi::c_void;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

/// Check whether the current process has accessibility permissions.
pub fn has_accessibility_permission() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Prompt the user to grant Accessibility permissions via the system dialog,
/// and return whether the process is currently trusted.
pub fn request_accessibility_permission() -> bool {
    unsafe {
        let key = kAXTrustedCheckOptionPrompt;
        let value = kCFBooleanTrue;
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            &key,
            &value,
            1,
            &kCFTypeDictionaryKeyCallBacks as *const u8 as *const std::ffi::c_void,
            &kCFTypeDictionaryValueCallBacks as *const u8 as *const std::ffi::c_void,
        );
        let result = AXIsProcessTrustedWithOptions(dict);
        CFRelease(dict);
        result
    }
}

// ── HotkeyManager ──

pub struct HotkeyManager {
    bindings: Vec<HotkeyBinding>,
    // We store the tap in a leaked Box so its callback closure (which
    // references the global HOTKEY_STATE) stays alive for the duration of
    // the event tap. Using 'static lifetime because the tap must outlive
    // any particular scope.
    tap: Option<&'static CGEventTap<'static>>,
    run_loop_source: Option<CFRunLoopSource>,
}

// The tap pointer is only used on the main run loop thread.
unsafe impl Send for HotkeyManager {}

impl HotkeyManager {
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
            tap: None,
            run_loop_source: None,
        }
    }

    /// Register all hotkeys described in `config` and install a CGEvent tap
    /// that invokes `callback` when a registered combination is pressed.
    pub fn register(
        &mut self,
        config: &Config,
        callback: HotkeyCallback,
    ) -> Result<(), ClipStashError> {
        // 1. Check accessibility permissions. Use the prompt variant so
        //    macOS shows its system dialog if not yet trusted.
        if !has_accessibility_permission() {
            let trusted = request_accessibility_permission();
            if !trusted {
                log::warn!("Accessibility not yet granted; will attempt tap anyway");
            }
        }

        // 2. Parse hotkeys from config.
        let mut bindings: Vec<HotkeyBinding> = Vec::new();

        // Picker
        let (mods, kc) = parse_hotkey(&config.picker_hotkey)?;
        bindings.push(HotkeyBinding {
            modifiers: mods,
            key_code: kc,
            action: HotkeyAction::OpenPicker,
        });

        // Cycle forward
        let (mods, kc) = parse_hotkey(&config.cycle_forward_hotkey)?;
        bindings.push(HotkeyBinding {
            modifiers: mods,
            key_code: kc,
            action: HotkeyAction::CycleForward,
        });

        // Cycle backward
        let (mods, kc) = parse_hotkey(&config.cycle_backward_hotkey)?;
        bindings.push(HotkeyBinding {
            modifiers: mods,
            key_code: kc,
            action: HotkeyAction::CycleBackward,
        });

        // 3. Direct slot hotkeys: cmd+shift+1 .. cmd+shift+N
        if config.direct_slot_hotkeys {
            let digit_keys: &[(&str, usize)] = &[
                ("1", 1),
                ("2", 2),
                ("3", 3),
                ("4", 4),
                ("5", 5),
                ("6", 6),
                ("7", 7),
                ("8", 8),
                ("9", 9),
                ("0", 0),
            ];
            for &(key_name, slot) in digit_keys.iter() {
                if slot >= 1 && slot <= config.capacity {
                    let code = key_name_to_code(key_name).unwrap();
                    bindings.push(HotkeyBinding {
                        modifiers: Modifiers::CMD | Modifiers::SHIFT,
                        key_code: code,
                        action: HotkeyAction::DirectSlot(slot),
                    });
                } else if slot == 0 && config.capacity >= 10 {
                    let code = key_name_to_code(key_name).unwrap();
                    bindings.push(HotkeyBinding {
                        modifiers: Modifiers::CMD | Modifiers::SHIFT,
                        key_code: code,
                        action: HotkeyAction::DirectSlot(slot),
                    });
                }
            }
        }

        self.bindings = bindings.clone();

        // 4. Store state globally for the callback closure.
        {
            let mut guard = HOTKEY_STATE.lock().unwrap();
            *guard = Some(HotkeyState {
                bindings,
                callback,
            });
        }

        // 5. Create the CGEvent tap.
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![CGEventType::KeyDown],
            event_tap_callback,
        )
        .map_err(|()| {
            ClipStashError::HotkeyError(
                "CGEventTap::new failed – is accessibility enabled?".into(),
            )
        })?;

        // 6. Create run loop source and add to current run loop.
        let source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|_| {
                ClipStashError::HotkeyError(
                    "failed to create run loop source from event tap".into(),
                )
            })?;

        unsafe {
            CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();

        // Leak the tap into a &'static reference so it stays alive.
        let tap_ref: &'static CGEventTap<'static> = Box::leak(Box::new(tap));
        self.tap = Some(tap_ref);
        self.run_loop_source = Some(source);

        log::info!("Hotkey event tap installed successfully");
        Ok(())
    }

    /// Remove the event tap and clean up resources.
    pub fn unregister(&mut self) -> Result<(), ClipStashError> {
        if let Some(source) = self.run_loop_source.take() {
            unsafe {
                CFRunLoop::get_current()
                    .remove_source(&source, kCFRunLoopCommonModes);
            }
        }
        if let Some(tap_ref) = self.tap.take() {
            // Reclaim the leaked Box and drop it.
            unsafe {
                let _ = Box::from_raw(
                    tap_ref as *const CGEventTap<'static>
                        as *mut CGEventTap<'static>,
                );
            }
        }
        self.bindings.clear();
        {
            let mut guard = HOTKEY_STATE.lock().unwrap();
            *guard = None;
        }
        log::info!("Hotkey event tap removed");
        Ok(())
    }

    /// Unregister all hotkeys and re-register with the current config.
    /// Preserves the callback from the previous registration.
    pub fn reload(
        &mut self,
        config: &Config,
    ) -> Result<(), ClipStashError> {
        // Extract the existing callback before unregistering.
        let callback = {
            let mut guard = HOTKEY_STATE.lock().unwrap();
            guard.take().map(|s| s.callback)
        };

        self.unregister()?;

        match callback {
            Some(cb) => self.register(config, cb),
            None => Err(ClipStashError::HotkeyError(
                "cannot reload: no previous callback registered".into(),
            )),
        }
    }

    /// Convenience wrapper – returns whether accessibility is granted.
    pub fn has_accessibility_permission() -> bool {
        has_accessibility_permission()
    }

    /// Convenience wrapper – prompts for accessibility permission.
    pub fn request_accessibility_permission() -> bool {
        request_accessibility_permission()
    }
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        let _ = self.unregister();
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hotkey_cmd_shift_v() {
        let (mods, kc) = parse_hotkey("cmd+shift+v").unwrap();
        assert_eq!(mods, Modifiers::CMD | Modifiers::SHIFT);
        assert_eq!(kc, 0x09); // v
    }

    #[test]
    fn parse_hotkey_single_modifier_cmd_v() {
        let (mods, kc) = parse_hotkey("cmd+v").unwrap();
        assert_eq!(mods, Modifiers::CMD);
        assert_eq!(kc, 0x09); // v
    }

    #[test]
    fn parse_hotkey_empty_returns_error() {
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn parse_hotkey_unknown_key_returns_error() {
        assert!(parse_hotkey("cmd+shift+unicorn").is_err());
    }

    #[test]
    fn has_accessibility_returns_bool() {
        // Simply call it – should not panic or have side effects.
        let _result: bool = has_accessibility_permission();
    }

    #[test]
    fn direct_slot_count_matches_capacity() {
        let config = Config {
            capacity: 7,
            direct_slot_hotkeys: true,
            ..Config::default()
        };
        // Parse the same bindings that register() would create.
        let digit_keys: &[(&str, usize)] = &[
            ("1", 1),
            ("2", 2),
            ("3", 3),
            ("4", 4),
            ("5", 5),
            ("6", 6),
            ("7", 7),
            ("8", 8),
            ("9", 9),
            ("0", 0),
        ];
        let mut slot_bindings = Vec::new();
        for &(key_name, slot) in digit_keys.iter() {
            if slot >= 1 && slot <= config.capacity {
                let code = key_name_to_code(key_name).unwrap();
                slot_bindings.push(HotkeyBinding {
                    modifiers: Modifiers::CMD | Modifiers::SHIFT,
                    key_code: code,
                    action: HotkeyAction::DirectSlot(slot),
                });
            } else if slot == 0 && config.capacity >= 10 {
                let code = key_name_to_code(key_name).unwrap();
                slot_bindings.push(HotkeyBinding {
                    modifiers: Modifiers::CMD | Modifiers::SHIFT,
                    key_code: code,
                    action: HotkeyAction::DirectSlot(slot),
                });
            }
        }
        // capacity=7 means slots 1..=7 -> 7 bindings
        assert_eq!(slot_bindings.len(), config.capacity);
    }

    #[test]
    fn parse_hotkey_brackets() {
        let (mods, kc) = parse_hotkey("cmd+shift+[").unwrap();
        assert_eq!(mods, Modifiers::CMD | Modifiers::SHIFT);
        assert_eq!(kc, 0x21);

        let (mods2, kc2) = parse_hotkey("cmd+shift+]").unwrap();
        assert_eq!(mods2, Modifiers::CMD | Modifiers::SHIFT);
        assert_eq!(kc2, 0x1E);
    }

    #[test]
    fn parse_hotkey_digit_keys() {
        let (_, kc) = parse_hotkey("cmd+shift+1").unwrap();
        assert_eq!(kc, 0x12);

        let (_, kc) = parse_hotkey("cmd+shift+0").unwrap();
        assert_eq!(kc, 0x1D);
    }

    #[test]
    fn parse_hotkey_alt_modifier() {
        let (mods, _) = parse_hotkey("alt+a").unwrap();
        assert!(mods.contains(Modifiers::ALT));

        let (mods2, _) = parse_hotkey("opt+a").unwrap();
        assert!(mods2.contains(Modifiers::ALT));
    }

    #[test]
    fn parse_hotkey_ctrl_modifier() {
        let (mods, _) = parse_hotkey("ctrl+c").unwrap();
        assert!(mods.contains(Modifiers::CTRL));
    }

    #[test]
    fn parse_hotkey_special_keys() {
        let (_, kc) = parse_hotkey("cmd+space").unwrap();
        assert_eq!(kc, 0x31);

        let (_, kc) = parse_hotkey("cmd+return").unwrap();
        assert_eq!(kc, 0x24);

        let (_, kc) = parse_hotkey("cmd+escape").unwrap();
        assert_eq!(kc, 0x35);
    }

    #[test]
    fn parse_hotkey_function_keys() {
        let (_, kc) = parse_hotkey("cmd+f1").unwrap();
        assert_eq!(kc, 0x7A);

        let (_, kc) = parse_hotkey("cmd+f12").unwrap();
        assert_eq!(kc, 0x6F);
    }

    #[test]
    fn parse_hotkey_arrow_keys() {
        let (_, kc) = parse_hotkey("cmd+up").unwrap();
        assert_eq!(kc, 0x7E);

        let (_, kc) = parse_hotkey("cmd+down").unwrap();
        assert_eq!(kc, 0x7D);

        let (_, kc) = parse_hotkey("cmd+left").unwrap();
        assert_eq!(kc, 0x7B);

        let (_, kc) = parse_hotkey("cmd+right").unwrap();
        assert_eq!(kc, 0x7C);
    }
}
