//! System-tray icon + in-app QR window for the PC server.
//!
//! ## What this owns
//! Two UI primitives on Windows that both need the main-thread Win32
//! message pump:
//!   * the tray icon (via `tray-icon`) — quick controls + tooltip,
//!   * an on-demand QR window (via `tao` + `wry`) — embedded WebView
//!     that loads `http://127.0.0.1:<port>/`. The page already runs
//!     in `qr_server.rs`; we just point WebView2 at it instead of
//!     handing the URL to the user's default browser.
//!
//! ## Threading
//! `tray-icon` and `wry`/`tao` both want their own message pump on the
//! main thread on Windows. We give them one shared `tao::EventLoop`
//! and process tray menu events, window events, and our own custom
//! user events inside its closure. The tokio runtime lives on a
//! worker thread (see `main.rs`).
//!
//! ## Shutdown
//! "Exit" calls `std::process::exit(0)`. The half-written-file
//! consequences are documented in the previous v1 — same compromise.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Window, WindowBuilder};
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::{WebView, WebViewBuilder};

use crate::pairing::PairingStore;

pub struct TrayState {
    /// Live phone-session counter. Kept as a freestanding `Arc` so the
    /// transports (`ws_server`, `relay_client`) can take a clone and
    /// bump it on each accept without depending on the rest of this
    /// struct. The tray tooltip reads it on its periodic refresh.
    pub peer_count: Arc<AtomicUsize>,
    /// Pairing store handle so the "refresh QR" menu item can rotate
    /// the code without going through the HTTP `/refresh` endpoint.
    pub pairing: Arc<PairingStore>,
    /// Port the local QR HTTP server is bound to. The in-app WebView
    /// loads `http://127.0.0.1:{port}/`.
    pub qr_http_port: u16,
    /// Set on tray "Exit" so any cooperative shutdown paths know to
    /// drop in-flight work. We still `process::exit(0)` after a brief
    /// grace period to ensure the tokio worker thread doesn't outlive
    /// the UI.
    pub shutdown: AtomicBool,
}

impl TrayState {
    pub fn new(pairing: Arc<PairingStore>, qr_http_port: u16) -> Arc<Self> {
        Arc::new(Self {
            peer_count: Arc::new(AtomicUsize::new(0)),
            pairing,
            qr_http_port,
            shutdown: AtomicBool::new(false),
        })
    }
}

/// Custom event posted onto the tao event loop. The user-event slot
/// lets us nudge the event loop from threads other than its own —
/// `OpenQrAtStartup` is fired by `run_tray_loop` itself one tick
/// after creating the loop so we can spawn the QR window inside the
/// closure where `EventLoopWindowTarget` is in scope.
#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    OpenQrAtStartup,
}

/// Build the tray icon + register the menu, then run the message pump
/// on the calling thread until the user picks Exit.
pub fn run_tray_loop(state: Arc<TrayState>) -> anyhow::Result<()> {
    let icon = make_icon()?;

    let menu = Menu::new();
    let item_title = MenuItem::new("RemoteControl 服务", false, None);
    let sep1 = PredefinedMenuItem::separator();
    let item_open_qr = MenuItem::new("📷 打开二维码页", true, None);
    let item_refresh = MenuItem::new("🔄 刷新二维码", true, None);
    let sep2 = PredefinedMenuItem::separator();
    let item_quit = MenuItem::new("❌ 退出", true, None);
    let id_open_qr = item_open_qr.id().clone();
    let id_refresh = item_refresh.id().clone();
    let id_quit = item_quit.id().clone();
    menu.append_items(&[
        &item_title,
        &sep1,
        &item_open_qr,
        &item_refresh,
        &sep2,
        &item_quit,
    ])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("RemoteControl: 启动中…")
        .build()?;

    info!("system tray icon installed");

    let event_loop = EventLoopBuilder::<TrayEvent>::with_user_event().build();
    let menu_channel = MenuEvent::receiver();
    let qr_url = format!("http://127.0.0.1:{}/", state.qr_http_port);

    // Show the QR window on first launch so the user sees it without
    // having to go hunt for the tray icon. Posted via the proxy so the
    // window-creation runs inside the event-loop closure where the
    // `EventLoopWindowTarget` is in scope.
    let _ = event_loop.create_proxy().send_event(TrayEvent::OpenQrAtStartup);

    // The on-demand QR window — created on first "open" click, dropped
    // on close. Holding both the Window and the WebView keeps them
    // alive; either being dropped tears the UI down on Windows.
    let mut qr_window: Option<QrWindow> = None;
    let mut last_tooltip = String::new();

    event_loop.run(move |event, window_target, control_flow| {
        // The cadence at which we wake to refresh the tooltip from the
        // shared peer counter. Short enough to feel live when a phone
        // connects, long enough to spend most cycles asleep.
        *control_flow =
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1500));

        // Tooltip from `peer_count`. Only re-sets when the string
        // changes — set_tooltip is a real Shell_NotifyIcon round-trip.
        let peers = state.peer_count.load(Ordering::Relaxed);
        let tip = match peers {
            0 => "RemoteControl: 等待手机连接".to_string(),
            1 => "RemoteControl: 1 部手机已连接".to_string(),
            n => format!("RemoteControl: {n} 部手机已连接"),
        };
        if tip != last_tooltip {
            if let Err(e) = tray.set_tooltip(Some(&tip)) {
                warn!("tray tooltip update failed: {e}");
            }
            last_tooltip = tip;
        }

        // Tray menu clicks. The channel is global so we poll every
        // tick rather than relying on tao events.
        while let Ok(menu_event) = menu_channel.try_recv() {
            match menu_event.id() {
                id if *id == id_open_qr => {
                    // Reuse the existing window if there is one;
                    // otherwise spin up a fresh tao+wry pair pointed
                    // at the local HTTP server.
                    if let Some(w) = &qr_window {
                        w.window.set_focus();
                    } else {
                        match QrWindow::open(window_target, &qr_url) {
                            Ok(w) => qr_window = Some(w),
                            Err(e) => warn!("open QR window: {e:#}"),
                        }
                    }
                }
                id if *id == id_refresh => {
                    state.pairing.rotate();
                    let (code, _) = state.pairing.current_qr_fields();
                    info!("tray: QR refreshed → new code={code}");
                    // If the WebView is showing the QR page, reload it
                    // so the new code is visible without the user
                    // having to refresh manually.
                    if let Some(w) = &qr_window {
                        if let Err(e) = w.webview.load_url(&qr_url) {
                            warn!("reload QR webview: {e}");
                        }
                    }
                }
                id if *id == id_quit => {
                    info!("tray: exit requested");
                    state.shutdown.store(true, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(150));
                    std::process::exit(0);
                }
                _ => {}
            }
        }

        match &event {
            // Drop the QR window when the user closes its X (or
            // Alt+F4 etc.). One symmetric close path regardless of
            // how the close was triggered.
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                if let Some(w) = &qr_window {
                    if w.window.id() == *window_id {
                        qr_window = None;
                    }
                }
            }
            // Startup auto-open. Other future user events go here too.
            Event::UserEvent(TrayEvent::OpenQrAtStartup) => {
                if qr_window.is_none() {
                    match QrWindow::open(window_target, &qr_url) {
                        Ok(w) => qr_window = Some(w),
                        Err(e) => warn!("auto-open QR window: {e:#}"),
                    }
                }
            }
            _ => {}
        }
    })
}

/// The two halves of the on-demand QR window. Dropping the struct
/// drops both the window and its WebView, which is the only way to
/// close the window cleanly with tao+wry.
struct QrWindow {
    window: Window,
    webview: WebView,
}

impl QrWindow {
    fn open(
        target: &tao::event_loop::EventLoopWindowTarget<TrayEvent>,
        url: &str,
    ) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("RemoteControl 二维码 / 文件传输")
            .with_inner_size(LogicalSize::new(720.0, 920.0))
            .with_min_inner_size(LogicalSize::new(480.0, 640.0))
            .build(target)?;
        // wry uses the window's native handle. On Windows this routes
        // to WebView2; on macOS it'd be WKWebView; on Linux WebKitGTK.
        let webview = WebViewBuilder::new(&window)
            .with_url(url)
            .build()?;
        Ok(Self { window, webview })
    }
}

/// 16×16 hand-painted RGBA icon, modeled on a real-phone silhouette
/// rather than the earlier "blue square with a dot" placeholder.
///
/// Design constraints learned from the failed earlier attempts:
///   * Windows renders tray icons at ~16 px and bilinear-downscales
///     anything larger; thin features in a 64×64 source disappear in
///     the blur. So we hand-place pixels at the actual 16×16 grid.
///   * The phone shape has to read as a phone at this size, which
///     means real-phone aspect (≈1 : 2, NOT 1 : 1) and a clearly
///     **dark** screen with bright content — the previous "solid
///     white screen" looked like a hollow square.
///
/// Layout (cols 4–10, rows 1–14 = 7 px × 14 px, ≈ 9:18 aspect):
///   * outer 1 px blue frame (`B`),
///   * dark screen fill (`K`, slate-900),
///   * three white "content lines" (`W`) in the upper half: two
///     full-width then a shorter one, mirroring the Android launcher
///     icon's screen text.
///
/// Palette matches `app-android/.../ic_launcher_foreground.xml` so the
/// PC tray icon and the phone app are visually a set:
///   * blue-500  `#3B82F6` — frame
///   * slate-900 `#0F172A` — screen
///   * slate-50  `#F8FAFC` — content lines
///   * transparent elsewhere (taskbar blends through, so the icon
///     reads on both light and dark themes).
fn make_icon() -> anyhow::Result<Icon> {
    const SIZE: u32 = 16;
    // 16 chars × 16 rows. Tabs/spaces inside the strings are stripped
    // by the parser; the visible glyphs carry the design.
    const PIXELS: &str = "\
        ................\
        .....BBBBB......\
        ....BBBBBBB.....\
        ....BKKKKKB.....\
        ....BKWWWKB.....\
        ....BKKKKKB.....\
        ....BKWWWKB.....\
        ....BKKKKKB.....\
        ....BKWWKKB.....\
        ....BKKKKKB.....\
        ....BKKKKKB.....\
        ....BKKKKKB.....\
        ....BKKKKKB.....\
        ....BBBBBBB.....\
        .....BBBBB......\
        ................\
    ";
    const BLUE: [u8; 4] = [0x3B, 0x82, 0xF6, 0xFF];
    const SCREEN: [u8; 4] = [0x0F, 0x17, 0x2A, 0xFF];
    const WHITE: [u8; 4] = [0xF8, 0xFA, 0xFC, 0xFF];
    const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

    let bytes = PIXELS.as_bytes();
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for &p in bytes {
        let rgba_pixel = match p as char {
            'B' => BLUE,
            'K' => SCREEN,
            'W' => WHITE,
            '.' => TRANSPARENT,
            // Strip any whitespace the multi-line literal might leak.
            _ => continue,
        };
        rgba.extend_from_slice(&rgba_pixel);
    }
    debug_assert_eq!(
        rgba.len() as u32,
        SIZE * SIZE * 4,
        "icon pixel grid must be exactly {SIZE}×{SIZE}",
    );
    Icon::from_rgba(rgba, SIZE, SIZE).map_err(anyhow::Error::from)
}
