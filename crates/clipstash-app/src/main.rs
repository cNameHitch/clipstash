use clipstash_hotkey::{HotkeyAction, HotkeyManager};
use clipstash_pb::PasteboardMonitor;
use clipstash_store::ClipStore;
use clipstash_types::*;
use clipstash_ui::{AppUI, MenuAction};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use std::{fs, process, thread};

// ── Raw FFI for POSIX signal handling and kill(2) ──

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

// ── Application events ──

enum AppEvent {
    NewItem(ClipboardItem),
    Hotkey(HotkeyAction),
    Menu(MenuAction),
    Quit,
}

fn main() {
    env_logger::init();
    log::info!("ClipStash starting");

    // 0. Initialize NSApplication (required before any AppKit usage).
    let mtm = MainThreadMarker::new()
        .expect("main() must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);

    // 1. Load configuration.
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to load configuration: {e}");
            process::exit(1);
        }
    };
    log::info!(
        "Config loaded: capacity={}, poll_interval={}ms",
        config.capacity,
        config.poll_interval_ms
    );

    // 2. Check / create PID file.
    let data_dir = Config::data_dir();
    let pid_file = data_dir.join("clipstash.pid");

    if let Err(e) = check_pid_file(&pid_file) {
        eprintln!("{e}");
        process::exit(1);
    }

    // 3. Create data directory if needed.
    if let Err(e) = fs::create_dir_all(&data_dir) {
        eprintln!("Error: cannot create data directory: {e}");
        process::exit(1);
    }

    // Write our PID.
    if let Err(e) = fs::write(&pid_file, process::id().to_string()) {
        eprintln!("Error: cannot write PID file: {e}");
        process::exit(1);
    }

    // 4. Initialize ClipStore.
    let store = match ClipStore::new(&config) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            eprintln!("Error: failed to initialize clipboard store: {e}");
            cleanup_pid_file(&pid_file);
            process::exit(1);
        }
    };

    // 5. Initialize PasteboardMonitor.
    let poll_interval = Duration::from_millis(config.poll_interval_ms);
    let monitor = Arc::new(Mutex::new(PasteboardMonitor::new(
        poll_interval,
        config.max_item_bytes,
    )));

    // 6. Sync monitor's next_id from store.
    {
        let st = store.lock().unwrap();
        let mon = monitor.lock().unwrap();
        mon.set_next_id(st.next_id());
    }

    // 7. Read current pasteboard and push to store.
    {
        let mon = monitor.lock().unwrap();
        match mon.read_current() {
            Ok(Some(item)) => {
                log::info!("Initial pasteboard item captured (id={})", item.id);
                let mut st = store.lock().unwrap();
                if let Err(e) = st.push(item) {
                    log::warn!("Failed to push initial item: {e}");
                }
            }
            Ok(None) => {
                log::info!("Pasteboard is empty at startup");
            }
            Err(e) => {
                log::warn!("Failed to read initial pasteboard: {e}");
            }
        }
    }

    // 8. Set up event channel.
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // 9. Check accessibility permissions.
    if !HotkeyManager::has_accessibility_permission() {
        log::warn!("Accessibility permission not granted -- hotkeys will not work");
        HotkeyManager::request_accessibility_permission();
    }

    // 10. Initialize and register HotkeyManager.
    let mut hotkey_manager = HotkeyManager::new();
    let hotkey_tx = tx.clone();
    let hotkey_callback: clipstash_hotkey::HotkeyCallback =
        Box::new(move |action: HotkeyAction| {
            hotkey_tx.send(AppEvent::Hotkey(action)).ok();
        });

    if let Err(e) = hotkey_manager.register(&config, hotkey_callback) {
        log::error!("Failed to register hotkeys: {e}");
        eprintln!("Warning: hotkeys may not be available: {e}");
    }

    // 11. Set up menu action callback and initialize AppUI.
    let menu_tx = tx.clone();
    clipstash_ui::set_menu_callback(move |action| {
        menu_tx.send(AppEvent::Menu(action)).ok();
    });

    let mut ui = AppUI::new();
    ui.setup(store.clone(), &config);

    // 12. Shared shutdown flag.
    let shutdown = Arc::new(AtomicBool::new(false));

    // 13. Spawn background thread for pasteboard polling.
    let poll_shutdown = Arc::clone(&shutdown);
    let poll_monitor = Arc::clone(&monitor);
    let poll_tx = tx.clone();
    thread::spawn(move || {
        log::info!("Pasteboard polling thread started");
        loop {
            thread::sleep(poll_interval);
            if poll_shutdown.load(Ordering::Relaxed) {
                break;
            }
            let mut mon = poll_monitor.lock().unwrap();
            match mon.poll() {
                Ok(Some(item)) => {
                    poll_tx.send(AppEvent::NewItem(item)).ok();
                }
                Ok(None) => {}
                Err(e) => log::error!("Poll error: {e}"),
            }
        }
        log::info!("Pasteboard polling thread exiting");
    });

    // 14. Set up signal handlers.
    let signal_shutdown = Arc::clone(&shutdown);
    let signal_tx = tx.clone();
    setup_signal_handlers(signal_shutdown, signal_tx);

    // 15. Main event loop.
    //     We use recv_timeout and pump the NSApp run loop each iteration
    //     so that AppKit events (menu clicks, overlay interaction) are serviced.
    log::info!("Entering main event loop");
    loop {
        // Pump the NSApplication run loop for pending UI events.
        loop {
            let event = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    objc2_app_kit::NSEventMask::Any,
                    None, // no wait
                    objc2_foundation::NSDefaultRunLoopMode,
                    true,
                )
            };
            match event {
                Some(ev) => app.sendEvent(&ev),
                None => break,
            }
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AppEvent::NewItem(item)) => {
                log::debug!("New clipboard item captured (id={})", item.id);
                let mut st = store.lock().unwrap();
                if let Err(e) = st.push(item) {
                    log::error!("Failed to push item: {e}");
                }
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Hotkey(HotkeyAction::OpenPicker)) => {
                let st = store.lock().unwrap();
                ui.toggle_overlay(&st, &config);
            }
            Ok(AppEvent::Hotkey(HotkeyAction::CycleForward)) => {
                {
                    let mut st = store.lock().unwrap();
                    st.cycle_forward();
                }
                write_active_to_pasteboard(&store, &monitor);
                let st = store.lock().unwrap();
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Hotkey(HotkeyAction::CycleBackward)) => {
                {
                    let mut st = store.lock().unwrap();
                    st.cycle_backward();
                }
                write_active_to_pasteboard(&store, &monitor);
                let st = store.lock().unwrap();
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Hotkey(HotkeyAction::DirectSlot(n))) => {
                {
                    let mut st = store.lock().unwrap();
                    st.set_active(n);
                }
                write_active_to_pasteboard(&store, &monitor);
                let st = store.lock().unwrap();
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Menu(MenuAction::SelectSlot(n))) => {
                log::debug!("Menu: select slot {n}");
                {
                    let mut st = store.lock().unwrap();
                    st.set_active(n);
                }
                write_active_to_pasteboard(&store, &monitor);
                let st = store.lock().unwrap();
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Menu(MenuAction::ClearHistory)) => {
                log::info!("Menu: clear history");
                let mut st = store.lock().unwrap();
                if let Err(e) = st.clear() {
                    log::error!("Failed to clear store: {e}");
                }
                ui.refresh(&st, &config);
            }
            Ok(AppEvent::Menu(MenuAction::Quit))
            | Ok(AppEvent::Hotkey(HotkeyAction::Quit))
            | Ok(AppEvent::Quit) => {
                log::info!("Quit event received");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Relaxed) {
                    log::info!("Shutdown flag detected");
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log::info!("Event channel disconnected");
                break;
            }
        }
    }

    // 16. Shutdown sequence.
    log::info!("ClipStash shutting down");
    shutdown.store(true, Ordering::SeqCst);

    if let Err(e) = hotkey_manager.unregister() {
        log::error!("Error unregistering hotkeys: {e}");
    }

    {
        let mut st = store.lock().unwrap();
        if let Err(e) = st.flush() {
            log::error!("Error flushing store: {e}");
        }
    }

    ui.cleanup();
    cleanup_pid_file(&pid_file);
    log::info!("ClipStash exited cleanly");
}

// ── Helper: write the active item to the system pasteboard ──

fn write_active_to_pasteboard(
    store: &Arc<Mutex<ClipStore>>,
    monitor: &Arc<Mutex<PasteboardMonitor>>,
) {
    let st = store.lock().unwrap();
    if let Some(item) = st.active() {
        let mut mon = monitor.lock().unwrap();
        if let Err(e) = mon.write_to_pasteboard(item) {
            log::error!("Failed to write active item to pasteboard: {e}");
        }
    }
}

// ── PID file management ──

fn check_pid_file(pid_file: &std::path::Path) -> Result<(), String> {
    if !pid_file.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(pid_file)
        .map_err(|e| format!("Failed to read PID file: {e}"))?;

    let pid: i32 = match contents.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            log::warn!("Corrupt PID file, overwriting");
            return Ok(());
        }
    };

    let result = unsafe { kill(pid, 0) };
    if result == 0 {
        return Err(format!("ClipStash is already running (PID: {pid})"));
    }

    log::info!("Stale PID file found (PID {pid} not running), overwriting");
    Ok(())
}

fn cleanup_pid_file(pid_file: &std::path::Path) {
    if let Err(e) = fs::remove_file(pid_file) {
        log::warn!("Failed to remove PID file: {e}");
    }
}

// ── Signal handling ──

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: i32) {
    SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
}

fn setup_signal_handlers(shutdown: Arc<AtomicBool>, tx: mpsc::Sender<AppEvent>) {
    unsafe {
        signal(SIGINT, signal_handler);
        signal(SIGTERM, signal_handler);
    }

    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
                log::info!("Signal received, initiating shutdown");
                shutdown.store(true, Ordering::SeqCst);
                tx.send(AppEvent::Quit).ok();
                break;
            }
        }
    });
}
