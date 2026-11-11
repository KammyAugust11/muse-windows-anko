# Building a Windows client for Muse — implementation notes

Notes from a working, user-side implementation (built 2026-11-11 by the user's
Muse, "Anko", at the user's request after an Edge `--app` shortcut proved to
be the only option another assistant could offer). Shared in case they are
useful for an official Windows app.

## Result

- Single native executable, ~1.5 MB, no installer framework required.
- Starts instantly because the web engine is the OS-shared WebView2 runtime
  (preinstalled on Windows 11; evergreen runtime on updated Windows 10).
  No Electron/Chromium bundle is shipped with the app.

## Architecture that worked

- **Shell**: Rust. `tao` for the native window/event loop, `wry` for WebView2
  hosting (the same two crates Tauri is built on, minus the framework),
  `tray-icon`/`muda` for the notification-area icon and menu.
- **Content**: the existing `https://muse.ai` web app, unchanged. Sign-in
  (including Google/Apple redirects) works inside the webview.
- **Profile separation**: WebView2 user-data folder under
  `%APPDATA%\Muse\WebView2`, so login state never mixes with the user's
  Edge/Chrome profile and survives browser resets.
- **Tray residency** (the feature the user cared about most):
  close button hides to the tray instead of quitting; tray left-click toggles
  the window; tray menu = Open / Reload / "Start with Windows" (checkbox) /
  Quit. Autostart uses the HKCU `Run` key with a `--tray` flag that starts
  the app hidden.
- **Single instance**: named mutex (`CreateMutexW`); a second launch shows a
  notice instead of opening another window.
- **Window state**: position/size persisted to a small JSON file and restored.
- **Link routing**: navigation handler allows `muse.ai` and identity-provider
  hosts (Google/Apple/Meta sign-in) inside the app; everything else opens in
  the user's default browser. New-window requests are routed the same way.
- **Fullscreen**: F11 is intercepted by an injected script, posted over the
  webview IPC channel, and toggled natively.
- **Small dependency discipline**: the few Win32 calls needed (mutex,
  autostart registry, message box) are declared directly with
  `extern "system"` instead of pulling in generated binding crates.
- **Companion file**: the GNU-target build links against Microsoft's
  redistributable `WebView2Loader.dll` (~160 KB), shipped next to the exe.

## Trade-offs / findings

- WebView2 is the only realistic rendering path: the service is a web app, so
  a "pure native UI" would mean reimplementing the client. Borrowing the OS
  runtime keeps the binary tiny and the UX identical to the web app.
- An Edge `--app` shortcut (the naive approach) fails on exactly the points
  above: no tray residency, profile mixed with the browser, no single-instance
  control, no autostart-hidden mode.
- Cross-compiling from Linux works via `cargo-zigbuild`
  (`x86_64-pc-windows-gnu`), with two gotchas: the `windows` crates' build
  scripts need a `dlltool` (zig provides one), and `winres`-produced resource
  objects were not linked under that pipeline, so the exe file carries no
  embedded `.rsrc` icon — window/tray/shortcut icons are set at runtime
  instead. On a Windows machine with the MSVC toolchain both issues disappear.
- Runtime verification note: the first build was only compile- and
  PE-verified (in the build VM, Wine 9.0 could not start any PE at all).
  v1.2.0 was built with the MSVC toolchain and launched on Windows 11: the
  window, the dedicated profile, the second-launch hand-off and the MCP tools
  window were exercised there. Signing in and the tray menu were not.
  Update 2026-11-11 (evening): the owner ran v1.2.0 on his own Windows PC —
  launch and sign-in both work, and the app is in daily use there. One known
  cosmetic issue from that run: with a Japanese IME, the composition /
  candidate window occasionally appears at the top-left of the window
  instead of at the text caret. This is the WebView2/Chromium IME
  positioning path (the caret rectangle reported to the OS sometimes goes
  missing right after focus changes, and Windows falls back to the
  window's top-left); it does not affect the committed text.
  v1.2.1 (trial fix, verification on the owner's machine pending): every
  show/restore path now also moves focus into the WebView2 itself, not
  just to the OS window, and regaining window focus re-asserts the
  webview's focus — the tray-restore path previously skipped that
  hand-off, which left composition without a caret to anchor to.
- Findings from that review, fixed in v1.2.0: the site's CSP names
  `auth.meta.com` as its login form target, a host the link router would have
  sent to the external browser (inferred from the CSP, not observed); window
  size was saved in physical pixels but restored as logical ones (the window
  grew on every start at >100% scaling); a minimized window saved
  (-32000, -32000) as its position; and links were handed to the shell
  without a scheme check, so a page could launch `file:` / protocol-handler
  URLs.

## Offer

The full Rust source (one `main.rs`, ~500 lines) and these notes can be shared
with the Muse team on request.
