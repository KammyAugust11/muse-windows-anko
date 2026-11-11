// Muse for Windows — standalone Rust + WebView2 (WebUI) app.
// Not an Edge "--app" shortcut: this is a real native .exe with its own
// window, its own WebView2 profile, single-instance lock, window-state
// persistence, external-link handling and system-tray residency.
//
//   cargo build --release            (output: target\release\anko.exe)
//
// Runtime requirement: WebView2 Runtime (preinstalled on Windows 11 and
// on updated Windows 10). Login data lives in %APPDATA%\Muse\WebView2,
// completely separate from Edge/Chrome profiles.
//
// Tray behaviour:
//   - Close (X) hides the window to the tray instead of quitting.
//   - Tray icon click toggles the window; tray menu has Open / Home /
//     Reload / MCP tools / "Start with Windows" toggle / Quit.
//   - Launched with --tray (the autostart entry uses this) it starts
//     hidden, resident only.
//   - Launching a second copy brings the running one to the front.
//   - --mcp additionally opens the MCP tools window at startup.
//
// MCP: see src/mcp.rs. A separate local window lists the tools of the MCP
// servers declared in %APPDATA%\Muse\mcp.json and runs them on request.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod mcp;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::{WebContext, WebView, WebViewBuilder};

const MUSE_URL: &str = "https://muse.ai";
const APP_DIR_NAME: &str = "Muse";
// Materialized by build.rs (from assets/icon.png or its .base64 form).
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon.png"));
const MCP_UI_HTML: &str = include_str!("mcp_ui.html");

#[derive(Debug)]
enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    /// A second launch asked the running instance to come to the front.
    ShowWindow,
    /// Open (or raise) the local MCP tools window.
    OpenMcp,
    /// Load this (already vetted, internal) URL in the main webview.
    Navigate(String),
    /// JSON reply for the MCP window: `{id, ok, data | error}`.
    McpReply(String),
    /// Paste tool output into the Muse page's message box.
    McpInsert { id: Value, text: String },
}

// ---------- window state persistence ----------

/// Outer position and inner size, both in physical pixels, of the window in
/// its normal (not maximized / minimized / fullscreen) state.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
struct WindowState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    #[serde(default)]
    maximized: bool,
}

fn app_dir() -> PathBuf {
    // MUSE_DATA_DIR relocates everything (profile, window state, mcp.json),
    // e.g. for a portable copy or a throwaway test profile.
    if let Some(dir) = std::env::var_os("MUSE_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

fn state_path() -> PathBuf {
    app_dir().join("window.json")
}

fn load_state() -> Option<WindowState> {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|s: &WindowState| s.width >= 320 && s.height >= 240 && s.width <= 16384 && s.height <= 16384)
}

fn save_state(s: &WindowState) {
    let _ = std::fs::create_dir_all(app_dir());
    if let Ok(json) = serde_json::to_string(s) {
        let _ = std::fs::write(state_path(), json);
    }
}

/// True if enough of the saved window lies on `monitor` (origin + size) for
/// the title bar to be reachable. Guards against a disconnected display.
fn fits_monitor(s: &WindowState, mon_pos: (i32, i32), mon_size: (u32, u32)) -> bool {
    let (mx, my) = (mon_pos.0 as i64, mon_pos.1 as i64);
    let (mw, mh) = (mon_size.0 as i64, mon_size.1 as i64);
    let (x, y, w) = (s.x as i64, s.y as i64, s.width as i64);
    x + w > mx + 80 && x < mx + mw - 80 && y >= my - 16 && y < my + mh - 80
}

/// Records the window geometry, but only while it is in its normal state.
/// Minimized windows report (-32000, -32000) and maximized / fullscreen ones
/// report the monitor size — persisting those would restore the window
/// off-screen or oversized next time.
fn capture_state(window: &Window, state: &mut Option<WindowState>) {
    if window.is_minimized() {
        return;
    }
    let maximized = window.is_maximized();
    if maximized || window.fullscreen().is_some() {
        if let Some(s) = state.as_mut() {
            s.maximized = maximized;
        }
        return;
    }
    let size = window.inner_size();
    let Ok(pos) = window.outer_position() else { return };
    if size.width == 0 || size.height == 0 {
        return;
    }
    *state = Some(WindowState { x: pos.x, y: pos.y, width: size.width, height: size.height, maximized: false });
}

// ---------- icon ----------

fn load_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(ICON_PNG).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

// ---------- Win32 bits (hand-written, no binding crates) ----------

#[cfg(target_os = "windows")]
mod win32 {
    use std::ffi::c_void;
    // Minimal hand-written Win32 bindings: keeps the dependency tree free of
    // generated binding crates for the few calls this app needs.
    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        pub fn CreateEventW(attrs: *const c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> *mut c_void;
        pub fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        pub fn SetEvent(handle: *mut c_void) -> i32;
        pub fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        pub fn CloseHandle(handle: *mut c_void) -> i32;
        pub fn GetLastError() -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        pub fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, flags: u32) -> i32;
        pub fn AllowSetForegroundWindow(process_id: u32) -> i32;
    }
    #[link(name = "advapi32")]
    extern "system" {
        pub fn RegCreateKeyExW(
            hkey: *mut c_void, subkey: *const u16, reserved: u32, class: *const u16,
            options: u32, sam: u32, sa: *const c_void, out_key: *mut *mut c_void,
            disposition: *mut u32,
        ) -> i32;
        pub fn RegOpenKeyExW(
            hkey: *mut c_void, subkey: *const u16, options: u32, sam: u32,
            out_key: *mut *mut c_void,
        ) -> i32;
        pub fn RegSetValueExW(
            hkey: *mut c_void, name: *const u16, reserved: u32, value_type: u32,
            data: *const u8, size: u32,
        ) -> i32;
        pub fn RegQueryValueExW(
            hkey: *mut c_void, name: *const u16, reserved: *const u32,
            value_type: *mut u32, data: *mut u8, size: *mut u32,
        ) -> i32;
        pub fn RegDeleteValueW(hkey: *mut c_void, name: *const u16) -> i32;
        pub fn RegCloseKey(hkey: *mut c_void) -> i32;
    }
    pub const ERROR_ALREADY_EXISTS: u32 = 183;
    pub const MB_OK: u32 = 0x0;
    pub const MB_ICONERROR: u32 = 0x10;
    pub const MB_ICONINFORMATION: u32 = 0x40;
    pub const EVENT_MODIFY_STATE: u32 = 0x0002;
    pub const WAIT_OBJECT_0: u32 = 0;
    pub const INFINITE: u32 = 0xFFFF_FFFF;
    pub const ASFW_ANY: u32 = 0xFFFF_FFFF;
    // HKEY_CURRENT_USER, sign-extended predefined handle.
    pub const HKCU: *mut c_void = -2147483647isize as *mut c_void;
    pub const KEY_QUERY_VALUE: u32 = 0x0001;
    pub const KEY_SET_VALUE: u32 = 0x0002;
    pub const REG_SZ: u32 = 1;
}

#[cfg(target_os = "windows")]
fn wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn message_box(text: &str, error: bool) {
    let text = wide(text);
    let cap = wide("Muse");
    let icon = if error { win32::MB_ICONERROR } else { win32::MB_ICONINFORMATION };
    unsafe {
        win32::MessageBoxW(std::ptr::null_mut(), text.as_ptr(), cap.as_ptr(), win32::MB_OK | icon);
    }
}

#[cfg(not(target_os = "windows"))]
fn message_box(text: &str, _error: bool) {
    eprintln!("{text}");
}

// ---------- single instance ----------

#[cfg(target_os = "windows")]
const MUTEX_NAME: &str = "MuseApp_Anko_SingleInstance";
#[cfg(target_os = "windows")]
const SHOW_EVENT_NAME: &str = "MuseApp_Anko_ShowWindow";

#[cfg(target_os = "windows")]
fn acquire_single_instance() -> bool {
    let name = wide(MUTEX_NAME);
    unsafe {
        // The handle is deliberately leaked: it must live as long as the process.
        let handle = win32::CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return true; // if the mutex itself fails, don't block startup
        }
        win32::GetLastError() != win32::ERROR_ALREADY_EXISTS
    }
}

/// Second launch: ask the running instance to show its window.
/// Returns false if it could not be reached.
#[cfg(target_os = "windows")]
fn signal_running_instance() -> bool {
    let name = wide(SHOW_EVENT_NAME);
    unsafe {
        let handle = win32::OpenEventW(win32::EVENT_MODIFY_STATE, 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        // Let the other process take the foreground; otherwise Windows only
        // flashes its taskbar button.
        win32::AllowSetForegroundWindow(win32::ASFW_ANY);
        let ok = win32::SetEvent(handle) != 0;
        win32::CloseHandle(handle);
        ok
    }
}

/// First launch: wait for `signal_running_instance` calls on a helper thread.
#[cfg(target_os = "windows")]
fn listen_for_show_requests(proxy: EventLoopProxy<UserEvent>) {
    let name = wide(SHOW_EVENT_NAME);
    let handle = unsafe { win32::CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        return;
    }
    let handle = handle as usize; // raw handles are not Send; the value is
    std::thread::spawn(move || loop {
        let rc = unsafe { win32::WaitForSingleObject(handle as *mut std::ffi::c_void, win32::INFINITE) };
        if rc != win32::WAIT_OBJECT_0 || proxy.send_event(UserEvent::ShowWindow).is_err() {
            break;
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn acquire_single_instance() -> bool {
    true
}
#[cfg(not(target_os = "windows"))]
fn signal_running_instance() -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
fn listen_for_show_requests(_proxy: EventLoopProxy<UserEvent>) {}

// ---------- autostart (HKCU Run key) ----------

#[cfg(target_os = "windows")]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
#[cfg(target_os = "windows")]
const RUN_VALUE: &str = "Muse";

#[cfg(target_os = "windows")]
fn get_autostart() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    unsafe {
        let mut hkey: *mut std::ffi::c_void = std::ptr::null_mut();
        if win32::RegOpenKeyExW(win32::HKCU, key.as_ptr(), 0, win32::KEY_QUERY_VALUE, &mut hkey) != 0 {
            return false;
        }
        let mut size: u32 = 0;
        let rc = win32::RegQueryValueExW(hkey, name.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), &mut size);
        win32::RegCloseKey(hkey);
        rc == 0
    }
}

#[cfg(target_os = "windows")]
fn set_autostart(enable: bool) {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    unsafe {
        let mut hkey: *mut std::ffi::c_void = std::ptr::null_mut();
        if win32::RegCreateKeyExW(
            win32::HKCU, key.as_ptr(), 0, std::ptr::null(), 0,
            win32::KEY_SET_VALUE | win32::KEY_QUERY_VALUE,
            std::ptr::null(), &mut hkey, std::ptr::null_mut(),
        ) != 0 {
            return;
        }
        if enable {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = format!("\"{}\" --tray", exe.display());
                let data = wide(&cmd);
                win32::RegSetValueExW(
                    hkey, name.as_ptr(), 0, win32::REG_SZ,
                    data.as_ptr() as *const u8, (data.len() * 2) as u32,
                );
            }
        } else {
            win32::RegDeleteValueW(hkey, name.as_ptr());
        }
        win32::RegCloseKey(hkey);
    }
}

#[cfg(not(target_os = "windows"))]
fn get_autostart() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn set_autostart(_enable: bool) {}

// ---------- link routing ----------

#[derive(Debug, PartialEq, Clone, Copy)]
enum Route {
    /// Stays inside the app: Muse itself and the sign-in pages it redirects to.
    Internal,
    /// Ordinary web / mail link: hand to the user's default browser.
    External,
    /// Harmless in-page navigation (about:blank and friends): let it happen.
    Passive,
    /// Anything else — file:, javascript:, ms-*: and other protocol handlers.
    /// A web page must never be able to make this app launch those.
    Blocked,
}

fn route(url_str: &str) -> Route {
    let Ok(u) = url::Url::parse(url_str) else { return Route::Blocked };
    match u.scheme() {
        "https" | "http" => {}
        "mailto" => return Route::External,
        "about" => return Route::Passive,
        _ => return Route::Blocked,
    }
    let Some(host) = u.host_str() else { return Route::Blocked };
    let host = host.to_ascii_lowercase();
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    let internal = is("muse.ai")
        || is("meta.ai")
        // Meta sign-in (Muse's login form posts to these).
        || is("auth.meta.com")
        || host == "accountscenter.meta.com"
        || matches!(host.as_str(), "www.facebook.com" | "web.facebook.com" | "m.facebook.com")
        || host == "www.instagram.com"
        // Google / Apple sign-in.
        || is("google.com")
        || is("apple.com");
    if internal { Route::Internal } else { Route::External }
}

fn open_external(url: &str) {
    let _ = open::that(url);
}

const INIT_SCRIPT: &str = r#"
(() => {
  // F11 toggles fullscreen via the native side (WebView2 swallows it otherwise).
  window.addEventListener('keydown', (e) => {
    if (e.key === 'F11') { e.preventDefault(); window.ipc.postMessage('toggle-fullscreen'); }
  });
})();
"#;

/// Types `text` into the page's message box (the focused one, else the first
/// editable element). Evaluates to true if a box was found.
fn insert_text_script(text: &str) -> String {
    let literal = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function (t) {{
  var ok = function (e) {{
    return e && (e.tagName === 'TEXTAREA' || e.isContentEditable ||
      (e.tagName === 'INPUT' && (e.type === 'text' || e.type === 'search')));
  }};
  var el = ok(document.activeElement) ? document.activeElement
    : document.querySelector('textarea:not([disabled]), [contenteditable="true"], [role="textbox"]');
  if (!el) return false;
  el.focus();
  return document.execCommand('insertText', false, t);
}})({literal})"#
    )
}

// ---------- windows ----------

fn show_window(window: &Window, webview: &WebView) {
    window.set_visible(true);
    if window.is_minimized() {
        window.set_minimized(false);
    }
    window.set_focus();
    // Hand focus into the WebView2 as well, not just to the OS window.
    // Without this, after a tray restore the page's input field and the
    // IME can be left out of sync: composition starts with no caret
    // rectangle and Windows parks the candidate window at the window's
    // top-left corner (seen with a Japanese IME on v1.2.0).
    let _ = webview.focus();
}

fn mcp_reply(id: &Value, result: Result<Value, String>) -> String {
    match result {
        Ok(data) => json!({"id": id, "ok": true, "data": data}),
        Err(error) => json!({"id": id, "ok": false, "error": error}),
    }
    .to_string()
}

fn open_config_in_notepad(manager: &mcp::Manager) -> Result<Value, String> {
    manager.ensure_config_file()?;
    std::process::Command::new("notepad.exe")
        .arg(manager.config_path())
        .spawn()
        .map_err(|e| format!("メモ帳を起動できません: {e}"))?;
    Ok(Value::Null)
}

/// The local "MCP ツール" window. Field order matters: the webview must be
/// dropped before the window that hosts it.
struct McpWindow {
    webview: WebView,
    window: Window,
}

fn open_mcp_window(
    target: &EventLoopWindowTarget<UserEvent>,
    web_context: &mut WebContext,
    icon: Option<tao::window::Icon>,
    manager: Arc<mcp::Manager>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<McpWindow, String> {
    let window = WindowBuilder::new()
        .with_title("MCP ツール — Muse")
        .with_window_icon(icon)
        .with_inner_size(LogicalSize::new(920.0, 640.0))
        .with_min_inner_size(LogicalSize::new(560.0, 360.0))
        .build(target)
        .map_err(|e| e.to_string())?;

    let webview = WebViewBuilder::new(&window)
        .with_web_context(web_context)
        .with_html(MCP_UI_HTML)
        .with_ipc_handler(move |req| {
            let Ok(msg) = serde_json::from_str::<Value>(req.body()) else { return };
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("cmd").and_then(Value::as_str) {
                Some("insert") => {
                    let text = msg.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let _ = proxy.send_event(UserEvent::McpInsert { id, text });
                }
                Some("open_config") => {
                    let reply = mcp_reply(&id, open_config_in_notepad(&manager));
                    let _ = proxy.send_event(UserEvent::McpReply(reply));
                }
                // Everything else may block on a server process: run it off
                // the UI thread and post the answer back through the loop.
                _ => {
                    let (manager, proxy) = (manager.clone(), proxy.clone());
                    std::thread::spawn(move || {
                        let reply = mcp_reply(&id, manager.handle(&msg));
                        let _ = proxy.send_event(UserEvent::McpReply(reply));
                    });
                }
            }
        })
        // This window only ever shows the embedded page. It can start local
        // tools, so it must never navigate to (or open) web content.
        .with_navigation_handler(|url: String| url.starts_with("about:") || url.starts_with("data:text/html"))
        .with_new_window_req_handler(|_url: String| false)
        .build()
        .map_err(|e| e.to_string())?;

    Ok(McpWindow { webview, window })
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        message_box(&format!("Muse で内部エラーが発生しました。\n\n{info}"), true);
    }));

    let start_in_tray = std::env::args().any(|a| a == "--tray");
    if !acquire_single_instance() {
        // Autostart duplicates stay silent; a manual launch surfaces the app.
        if !start_in_tray && !signal_running_instance() {
            message_box("Muse は既に起動しています（タスクトレイに常駐中かもしれません）。", false);
        }
        return;
    }

    if let Err(e) = run(start_in_tray) {
        message_box(
            &format!(
                "Muse を起動できませんでした。\n\n{e}\n\n\
                 WebView2 ランタイムが入っていない可能性があります。\n\
                 Microsoft の「WebView2 Runtime（Evergreen）」をインストールしてから、もう一度お試しください。"
            ),
            true,
        );
    }
}

fn run(start_in_tray: bool) -> wry::Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let proxy_tray = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        let _ = proxy_tray.send_event(UserEvent::Tray(ev));
    }));
    let proxy_menu = proxy.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        let _ = proxy_menu.send_event(UserEvent::Menu(ev));
    }));
    listen_for_show_requests(proxy.clone());

    let (rgba, iw, ih) = load_icon_rgba().expect("embedded icon.png must decode");
    let window_icon = tao::window::Icon::from_rgba(rgba.clone(), iw, ih).ok();
    let tray_icon = tray_icon::Icon::from_rgba(rgba, iw, ih).ok();

    // Restore the saved geometry in the same (physical) units it was saved
    // in, and only if that spot still exists on a connected monitor.
    let saved = load_state();
    let mut wb = WindowBuilder::new()
        .with_title("Muse")
        .with_window_icon(window_icon.clone())
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_visible(!start_in_tray);
    wb = match saved {
        Some(s) => {
            let on_screen = event_loop.available_monitors().any(|m| {
                let (p, sz) = (m.position(), m.size());
                fits_monitor(&s, (p.x, p.y), (sz.width, sz.height))
            });
            let wb = wb.with_inner_size(PhysicalSize::new(s.width, s.height)).with_maximized(s.maximized);
            if on_screen { wb.with_position(PhysicalPosition::new(s.x, s.y)) } else { wb }
        }
        None => wb.with_inner_size(LogicalSize::new(1280.0, 860.0)),
    };
    let window = Rc::new(wb.build(&event_loop).expect("failed to create window"));
    let state = Rc::new(RefCell::new(saved));

    // Dedicated WebView2 profile: %APPDATA%\Muse\WebView2
    let data_dir = app_dir().join("WebView2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut web_context = WebContext::new(Some(data_dir));

    let win_for_ipc = window.clone();
    let proxy_new_window = proxy.clone();
    let webview = WebViewBuilder::new(&*window)
        .with_web_context(&mut web_context)
        .with_url(MUSE_URL)
        .with_initialization_script(INIT_SCRIPT)
        .with_ipc_handler(move |req| {
            // The only message the (remote) Muse page may send. MCP is not
            // reachable from here by design.
            if req.body() == "toggle-fullscreen" {
                let next = match win_for_ipc.fullscreen() {
                    Some(_) => None,
                    None => Some(tao::window::Fullscreen::Borderless(None)),
                };
                win_for_ipc.set_fullscreen(next);
            }
        })
        .with_navigation_handler(|url: String| match route(&url) {
            Route::Internal | Route::Passive => true,
            Route::External => {
                open_external(&url);
                false
            }
            Route::Blocked => false,
        })
        .with_new_window_req_handler(move |url: String| {
            // Never spawn a second, unmanaged WebView2 popup window.
            match route(&url) {
                Route::Internal => {
                    let _ = proxy_new_window.send_event(UserEvent::Navigate(url));
                }
                Route::External => open_external(&url),
                Route::Passive | Route::Blocked => {}
            }
            false
        })
        .with_download_started_handler(|_url, _path| true)
        .with_download_completed_handler(|_url, _path, _success| {})
        .build()?;

    let mcp_manager = Arc::new(mcp::Manager::new(app_dir().join("mcp.json")));
    let mut mcp_window: Option<McpWindow> = None;

    // Tray menu items (created with the tray itself on Init, kept alive here).
    let open_item = MenuItem::with_id("open", "Muse を開く", true, None);
    let home_item = MenuItem::with_id("home", "ホームへ戻る", true, None);
    let reload_item = MenuItem::with_id("reload", "再読み込み", true, None);
    let mcp_item = MenuItem::with_id("mcp", "MCP ツール…", true, None);
    let autostart_item = CheckMenuItem::with_id(
        "autostart",
        "Windows 起動時に自動起動",
        true,
        get_autostart(),
        None,
    );
    let quit_item = MenuItem::with_id("quit", "終了", true, None);

    // `--mcp` opens the MCP tools window right away (handy for a shortcut).
    let open_mcp_at_start = std::env::args().any(|a| a == "--mcp");
    let main_id = window.id();
    let mut last_saved = saved;
    // Held only to keep the tray icon alive for the life of the event loop.
    let mut _tray: Option<TrayIcon> = None;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        let mut persist = |force: bool| {
            let current = *state.borrow();
            if let Some(s) = current.filter(|s| force || Some(*s) != last_saved) {
                save_state(&s);
                last_saved = Some(s);
            }
        };
        match event {
            Event::NewEvents(StartCause::Init) => {
                // Tray icon must be created inside the event loop (Windows).
                let menu = Menu::new();
                let _ = menu.append(&open_item);
                let _ = menu.append(&home_item);
                let _ = menu.append(&reload_item);
                let _ = menu.append(&PredefinedMenuItem::separator());
                let _ = menu.append(&mcp_item);
                let _ = menu.append(&PredefinedMenuItem::separator());
                let _ = menu.append(&autostart_item);
                let _ = menu.append(&PredefinedMenuItem::separator());
                let _ = menu.append(&quit_item);
                let mut b = TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("Muse（あんこ常駐中）")
                    .with_menu_on_left_click(false);
                if let Some(icon) = tray_icon.clone() {
                    b = b.with_icon(icon);
                }
                _tray = b.build().ok();
                if open_mcp_at_start {
                    let _ = proxy.send_event(UserEvent::OpenMcp);
                }
            }
            Event::UserEvent(UserEvent::Tray(ev)) => {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                    // Ask the window itself rather than tracking a flag: it
                    // may have been minimized or shown behind our back.
                    if window.is_visible() && !window.is_minimized() {
                        persist(false);
                        window.set_visible(false);
                    } else {
                        show_window(&window, &webview);
                    }
                }
            }
            Event::UserEvent(UserEvent::ShowWindow) => show_window(&window, &webview),
            Event::UserEvent(UserEvent::OpenMcp) => match &mcp_window {
                Some(mcp) => show_window(&mcp.window, &mcp.webview),
                None => {
                    match open_mcp_window(target, &mut web_context, window_icon.clone(), mcp_manager.clone(), proxy.clone()) {
                        Ok(mcp) => mcp_window = Some(mcp),
                        Err(e) => message_box(&format!("MCP ツールの画面を開けませんでした。\n\n{e}"), true),
                    }
                }
            },
            Event::UserEvent(UserEvent::Navigate(url)) => {
                let _ = webview.load_url(&url);
                show_window(&window, &webview);
            }
            Event::UserEvent(UserEvent::McpReply(reply)) => {
                if let Some(mcp) = &mcp_window {
                    let _ = mcp.webview.evaluate_script(&format!("window.__mcpReply({reply})"));
                }
            }
            Event::UserEvent(UserEvent::McpInsert { id, text }) => {
                show_window(&window, &webview);
                let reply_proxy = proxy.clone();
                let fail_proxy = proxy.clone();
                let fail_id = id.clone();
                let sent = webview.evaluate_script_with_callback(&insert_text_script(&text), move |result| {
                    let inserted = result.trim() == "true";
                    let _ = reply_proxy.send_event(UserEvent::McpReply(mcp_reply(&id, Ok(json!(inserted)))));
                });
                if let Err(e) = sent {
                    let _ = fail_proxy.send_event(UserEvent::McpReply(mcp_reply(&fail_id, Err(e.to_string()))));
                }
            }
            Event::UserEvent(UserEvent::Menu(ev)) => match ev.id().as_ref() {
                "open" => show_window(&window, &webview),
                "home" => {
                    let _ = webview.load_url(MUSE_URL);
                    show_window(&window, &webview);
                }
                "reload" => {
                    // wry 0.45 has no reload(); re-loading the current URL is equivalent.
                    if let Ok(u) = webview.url() {
                        let _ = webview.load_url(&u);
                    }
                }
                "mcp" => {
                    let _ = proxy.send_event(UserEvent::OpenMcp);
                }
                "autostart" => {
                    let next = !get_autostart();
                    set_autostart(next);
                    autostart_item.set_checked(get_autostart());
                }
                "quit" => {
                    persist(true);
                    // Stop MCP servers before the process goes away.
                    mcp_window = None;
                    mcp_manager.reload();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::WindowEvent { event, window_id, .. } if window_id != main_id => {
                // The MCP window: closing it frees its webview; servers keep
                // running until Quit so reopening is instant.
                if matches!(event, WindowEvent::CloseRequested) {
                    mcp_window = None;
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Resident behaviour: X hides to the tray, it does not quit.
                persist(false);
                window.set_visible(false);
            }
            Event::WindowEvent { event: WindowEvent::Resized(_) | WindowEvent::Moved(_), .. } => {
                capture_state(&window, &mut state.borrow_mut());
            }
            Event::WindowEvent { event: WindowEvent::Focused(true), .. } => {
                // Keep the WebView2's focus in step with the OS window, so
                // an IME never starts composition against a stale caret
                // (see the note in show_window).
                let _ = webview.focus();
            }
            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => {
                // Cheap checkpoint so a logoff / shutdown / crash while the
                // app sits in the tray does not lose the last geometry.
                persist(false);
            }
            Event::LoopDestroyed => persist(false),
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_links() {
        for url in [
            "https://muse.ai/",
            "https://www.muse.ai/chat/1",
            "https://auth.meta.com/login",
            "https://x.od.auth.meta.com/a",
            "https://www.facebook.com/login.php",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://appleid.apple.com/auth/authorize",
        ] {
            assert_eq!(route(url), Route::Internal, "{url}");
        }
        for url in [
            "https://example.com/",
            "https://notmuse.ai/",
            "https://muse.ai.evil.example/",
            "https://www.meta.com/quest/",
            "mailto:someone@example.com",
        ] {
            assert_eq!(route(url), Route::External, "{url}");
        }
        assert_eq!(route("about:blank"), Route::Passive);
        // Must never reach ShellExecute.
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "file://attacker/share/evil.exe",
            "ms-msdt:/id PCWDiagnostic",
            "javascript:alert(1)",
            "search-ms:query=x",
            "C:\\Windows\\System32\\calc.exe",
            "not a url",
        ] {
            assert_eq!(route(url), Route::Blocked, "{url}");
        }
    }

    #[test]
    fn rejects_off_screen_geometry() {
        let mon = ((0, 0), (1920, 1080));
        let s = |x, y| WindowState { x, y, width: 1280, height: 860, maximized: false };
        assert!(fits_monitor(&s(120, 80), mon.0, mon.1));
        assert!(fits_monitor(&s(-8, -8), mon.0, mon.1)); // maximized-style overhang
        assert!(!fits_monitor(&s(-32000, -32000), mon.0, mon.1)); // minimized
        assert!(!fits_monitor(&s(2500, 100), mon.0, mon.1)); // unplugged 2nd display
        assert!(!fits_monitor(&s(100, 1070), mon.0, mon.1));
        assert!(fits_monitor(&s(2500, 100), (1920, 0), (1920, 1080)));
    }

    #[test]
    fn old_state_files_still_load() {
        let s: WindowState = serde_json::from_str(r#"{"x":1,"y":2,"width":800,"height":600}"#).unwrap();
        assert_eq!(s, WindowState { x: 1, y: 2, width: 800, height: 600, maximized: false });
    }

    #[test]
    fn insert_script_escapes_text() {
        let js = insert_text_script("a\"b\n</script>\\");
        assert!(js.ends_with(r#"("a\"b\n</script>\\")"#), "{js}");
    }
}
