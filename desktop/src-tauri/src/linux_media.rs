//! Linux-only: enable media capture (`getUserMedia`) in the WebKitGTK webview.
//!
//! On macOS (WKWebView) and Windows (WebView2) the media-permission prompt is
//! routed to the OS automatically, so microphone/camera capture "just works".
//! WebKitGTK is different on two counts, and both must be handled or capture
//! fails on Linux only:
//!
//! * `enable-media-stream` is **off by default**, so `navigator.mediaDevices`
//!   never exposes a working `getUserMedia`; and
//! * the default `permission-request` handler **denies every request**, so even
//!   with media-stream on, the call rejects with `NotAllowedError`.
//!
//! This module reaches the underlying `webkit2gtk::WebView` via
//! [`tauri::Webview::with_webview`], enables media-stream, and installs a
//! `permission-request` handler that is **deny-by-default**: a `UserMedia`
//! request is allowed only when it comes from a trusted app origin and asks for
//! an audio and/or video device. Tauri does not restrict navigation by default,
//! so without the origin check any document that ended up in this webview would
//! inherit silent mic/camera access for the process lifetime.
//!
//! Buzz's AppImage pins `GDK_BACKEND=x11` (see [`crate::webkit_rendering`]),
//! which is the backend WebKitGTK media capture is reliable on.

/// The origin Tauri serves the packaged app from on Linux.
/// Consumed only by linux-gated [`enable_media_capture`]; kept compiling on all
/// platforms so the unit tests run everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PROD_ORIGIN: &str = "tauri://localhost";

/// The Vite dev-server origin (`devUrl` in `tauri.conf.json`, `strictPort`
/// 1420 in `vite.config.ts`). Only trusted in debug builds.
#[cfg(debug_assertions)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DEV_ORIGIN: &str = "http://localhost:1420";

/// Whether `uri` (the webview's current document URI) is a trusted app origin
/// allowed to use mic/camera. Matches the origin exactly or as a path prefix so
/// `tauri://localhost.evil.com` and `http://localhost:14200` do not slip
/// through. Pure and platform-independent so it can be unit-tested everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_trusted_media_origin(uri: &str) -> bool {
    fn matches(uri: &str, origin: &str) -> bool {
        uri == origin
            || uri
                .strip_prefix(origin)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    if matches(uri, PROD_ORIGIN) {
        return true;
    }
    #[cfg(debug_assertions)]
    if matches(uri, DEV_ORIGIN) {
        return true;
    }
    false
}

/// Enable microphone/camera capture for `webview` if it is running on
/// WebKitGTK. A no-op on every non-Linux target, so callers can invoke it
/// unconditionally from shared startup code.
#[cfg(target_os = "linux")]
pub fn enable_media_capture<R: tauri::Runtime>(webview: &tauri::Webview<R>) {
    use webkit2gtk::{
        glib::prelude::Cast, PermissionRequestExt, SettingsExt, UserMediaPermissionRequest,
        UserMediaPermissionRequestExt, WebViewExt,
    };

    // `with_webview` runs the closure on the UI thread, which GTK calls
    // require. It errors only if the platform webview is unavailable.
    let result = webview.with_webview(|platform_webview| {
        // On Linux this is the underlying `webkit2gtk::WebView`.
        let webview = platform_webview.inner();

        if let Some(settings) = WebViewExt::settings(&webview) {
            settings.set_enable_media_stream(true);
        }

        // Deny-by-default: allow only mic/camera requests from a trusted app
        // origin; deny everything else (still returning `true` so WebKit's
        // auto-deny default does not also run). Non-`UserMedia` requests return
        // `false` and keep their default handling.
        webview.connect_permission_request(|wv, request| {
            let Some(request) = request.downcast_ref::<UserMediaPermissionRequest>() else {
                return false;
            };

            let uri = wv.uri().map(|u| u.to_string()).unwrap_or_default();
            let for_device = request.is_for_audio_device() || request.is_for_video_device();

            if for_device && is_trusted_media_origin(&uri) {
                request.allow();
            } else {
                request.deny();
            }
            true
        });
    });

    if let Err(error) = result {
        eprintln!("buzz-desktop: could not enable WebKitGTK media capture: {error}");
    }
}

/// Maximum number of times the renderer may be auto-reloaded after a crash
/// before giving up, to avoid an auto-reload crash-loop when the crash is
/// deterministic on load (as in #4358's startup-enumeration segfault).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MAX_RENDERER_AUTO_RELOADS: usize = 3;

/// Sliding window (seconds) within which `MAX_RENDERER_AUTO_RELOADS` reloads
/// are permitted. Older crashes age out, so a renderer that crashes rarely is
/// still recovered each time.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const RENDERER_RELOAD_WINDOW_SECS: u64 = 120;

/// Decide whether the renderer may be auto-reloaded given the timestamps of
/// recent crashes. `attempt` (seconds since some epoch) is the time of the
/// crash currently being handled. Pure and platform-independent so it can be
/// unit-tested everywhere; the caller prunes/out-dates entries itself by
/// passing only crashes within the window.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn should_auto_reload_renderer(recent_crashes: &[u64], attempt: u64) -> bool {
    recent_crashes
        .iter()
        .filter(|&&t| attempt.saturating_sub(t) < RENDERER_RELOAD_WINDOW_SECS)
        .count()
        < MAX_RENDERER_AUTO_RELOADS
}

/// Log when the `WebKitWebProcess` (renderer) terminates, so a renderer crash
/// surfaces as a diagnosable log line instead of a silent dead window, and
/// auto-reload the renderer to recover — rate-limited so a deterministic
/// startup crash does not loop (see #4359).
///
/// Without a handler the renderer can die (e.g. an upstream PipeWire/GStreamer
/// segfault, OOM, or a WebKit bug) while the main `buzz-desktop` and
/// `WebKitNetworkProcess` processes stay alive, leaving a frozen window with no
/// log output. WebKitGTK emits `web-process-terminated` for exactly this case;
/// `WebProcessTerminationReason` distinguishes `Crashed` / `ExceededMemoryLimit`
/// / `TerminatedByApi`. A no-op on non-Linux targets.
#[cfg(target_os = "linux")]
pub fn install_web_process_terminated_handler<R: tauri::Runtime>(webview: &tauri::Webview<R>) {
    use std::{cell::RefCell, rc::Rc, time::Instant};
    use webkit2gtk::{glib::prelude::Cast, WebViewExt};

    // Crash timestamps, elapsed seconds since handler install, in a shared cell
    // the `Fn + 'static` signal closure can inspect and update. Single-threaded
    // (GTK fires on the UI thread), so `Rc<RefCell<..>>` is sufficient.
    let t0 = Instant::now();
    let crash_times: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

    let result = webview.with_webview(move |platform_webview| {
        let crash_times = Rc::clone(&crash_times);
        platform_webview
            .inner()
            .connect_web_process_terminated(move |webview, reason| {
                eprintln!(
                    "buzz-desktop: WebKitWebProcess terminated (reason: {reason:?}); \
                     the window may appear frozen. See #4359."
                );

                let attempt = t0.elapsed().as_secs();
                let mut crashes = crash_times.borrow_mut();
                let can_reload = should_auto_reload_renderer(&crashes, attempt);
                if can_reload {
                    crashes.push(attempt);
                    eprintln!(
                        "buzz-desktop: reloading renderer after termination \
                         ({}/{MAX_RENDERER_AUTO_RELOADS} within {RENDERER_RELOAD_WINDOW_SECS}s)"
                    );
                    webview.reload();
                } else {
                    eprintln!(
                        "buzz-desktop: not auto-reloading renderer — exceeded \
                         {MAX_RENDERER_AUTO_RELOADS} reloads within \
                         {RENDERER_RELOAD_WINDOW_SECS}s; restart the app to recover."
                    );
                }
            });
    });

    if let Err(error) = result {
        eprintln!("buzz-desktop: could not install web-process-terminated handler: {error}");
    }
}

/// No-op stub so shared startup code can call [`enable_media_capture`] on every
/// platform. macOS and Windows route media permissions through the OS.
#[cfg(not(target_os = "linux"))]
pub fn enable_media_capture<R: tauri::Runtime>(_webview: &tauri::Webview<R>) {}

/// No-op stub on non-Linux targets; there is no `WebKitWebProcess` to observe.
#[cfg(not(target_os = "linux"))]
pub fn install_web_process_terminated_handler<R: tauri::Runtime>(_webview: &tauri::Webview<R>) {}

#[cfg(test)]
mod tests {
    use super::{is_trusted_media_origin, should_auto_reload_renderer};

    #[test]
    fn allows_production_app_origin() {
        assert!(is_trusted_media_origin("tauri://localhost"));
        assert!(is_trusted_media_origin(
            "tauri://localhost/channels/general"
        ));
    }

    #[test]
    fn denies_untrusted_origins() {
        assert!(!is_trusted_media_origin(""));
        assert!(!is_trusted_media_origin("https://evil.example.com"));
        // Prefix look-alikes must not slip through.
        assert!(!is_trusted_media_origin("tauri://localhost.evil.com"));
        assert!(!is_trusted_media_origin("tauri://localhostfoo"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn allows_dev_origin_in_debug_only() {
        assert!(is_trusted_media_origin("http://localhost:1420"));
        assert!(is_trusted_media_origin("http://localhost:1420/"));
        // A different localhost port is still untrusted.
        assert!(!is_trusted_media_origin("http://localhost:14200"));
        assert!(!is_trusted_media_origin("http://localhost:3000"));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn denies_dev_origin_in_release() {
        assert!(!is_trusted_media_origin("http://localhost:1420"));
    }

    #[test]
    fn auto_reload_allowed_below_cap_within_window() {
        // Fewer than MAX crashes inside the window -> reload allowed.
        assert!(should_auto_reload_renderer(&[], 100));
        assert!(should_auto_reload_renderer(&[95, 96], 100));
        // Exactly at cap -> blocked.
        assert!(!should_auto_reload_renderer(&[94, 95, 96], 100));
    }

    #[test]
    fn auto_reload_ignores_crashes_older_than_window() {
        let window = 120;
        // Two old crashes (outside window) should not count -> reload allowed.
        assert!(should_auto_reload_renderer(&[0, 1], 0 + window + 1 + 100));
        // Saturating subtraction guards against attempt < logged time.
        assert!(should_auto_reload_renderer(&[5], 0));
    }
}
