//! `/api/screen/*` — read-only screen capture of the machine amux-server-rs
//! runs on (Ethan's request, 2026-09-15: "how can i enable amux to control my
//! actual computer... i want to be able to see/read ChatGPT app desktop").
//!
//! SAME "hard invariant" shape as `/api/browser` (see that module's header):
//! this always captures the SERVER machine's screen, never asks a remote
//! dashboard viewer to do anything, and artifacts are served through the API
//! rather than as a raw filesystem path a phone across the network cannot
//! read.
//!
//! WHY THE SYSTEM PERMISSION PROMPT, NOT A MANUALLY-ADDED ENTRY: this binary
//! is signed with a stable LOCAL identity (`amux-dev`, see
//! scripts/create-codesign-identity.sh) that has no Apple Developer Team ID.
//! macOS's Screen Recording pane in System Settings routinely refuses to
//! persist an entry added via its own "+" file picker for a binary with no
//! Team ID — confirmed live, 2026-09-15: `codesign -dv` on amux-server-rs
//! shows `TeamIdentifier=not set`, and manually adding it did not stick. The
//! path that DOES work for a self-signed binary is the OS's OWN native
//! prompt, triggered by an actual capture attempt — and because the signing
//! identity is stable across rebuilds, one "Allow" on that prompt persists
//! forever, which is the exact property `amux-dev` exists to provide.
//!
//! SCOPE, deliberately narrow for a first cut: loopback-only (`peer.is_
//! loopback()`), so the tailnet/tunnel-exposed dashboard cannot reach this
//! even with a valid token — a screen capture is far more personal than
//! anything else this server hands out, and "reachable by anything already
//! running on this machine" is a real boundary, not just "did nothing stop
//! me". Whether every one of the fleet's workers should be able to call this
//! (vs. an owner-only gate) is a separate decision Ethan has not made yet —
//! named here rather than guessed at.

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::net::SocketAddr;

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/capture", get(capture))
        .route("/capture/file", get(capture_file))
}

fn capture_path() -> std::path::PathBuf {
    crate::config::amux_home()
        .join("screen-captures")
        .join("latest.png")
}

fn loopback_only(peer: Option<Extension<ConnectInfo<SocketAddr>>>) -> Option<Response> {
    let is_loopback = peer.map(|Extension(ConnectInfo(addr))| addr.ip().is_loopback());
    if is_loopback == Some(true) {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "screen capture is loopback-only — this request did not come from the server machine itself",
                "code": "screen_capture_not_loopback",
            })),
        )
            .into_response(),
    )
}

/// GET /api/screen/capture — full-screen capture via macOS's own
/// `screencapture`, saved once to a fixed path (this is a live look at the
/// desktop, not a gallery — each call replaces the last).
async fn capture(
    State(_state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Response {
    if let Some(refusal) = loopback_only(peer) {
        return refusal;
    }
    let path = capture_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("could not create capture dir: {e}")})),
            )
                .into_response();
        }
    }
    // This box runs 24/7 with nobody physically at it, so by the time
    // anyone calls this endpoint macOS's displaysleep (10 min idle here)
    // has almost always already fired. `screencapture` against a SLEEPING
    // display exits 0 and writes a real, valid, near-solid-black PNG --
    // no error text, nothing that looks like a failure. Confirmed live
    // 2026-09-15: 68823 bytes / all black while asleep, 7477013 bytes /
    // real content immediately after waking. `caffeinate -u` asserts the
    // same "user is active" signal a trackpad touch would, which is the
    // standard way to wake a display without synthesizing fake input.
    let _ = tokio::process::Command::new("/usr/bin/caffeinate")
        .args(["-u", "-t", "1"])
        .spawn();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    // -x: no camera-shutter sound, no cursor. This is a headless server
    // capture, not a person taking a screenshot of their own action.
    let out = tokio::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-t", "png"])
        .arg(&path)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() && path.exists() => {
            let meta = tokio::fs::metadata(&path).await.ok();
            let bytes = meta.map(|m| m.len()).unwrap_or(0);
            // Not a lie-detector, a cheap honest signal: a 2560x1440 PNG
            // that is actually solid (or near-solid) black -- asleep
            // display, or any other reason the display had nothing to show
            // -- compresses to well under 150KB every time we've measured
            // it; a real desktop or even just the macOS lock screen wallpaper
            // runs hundreds of KB to several MB. Below the line means "look
            // at this before you trust it shows what you wanted", not "this
            // definitely failed" -- ethos rule 4: the number that produced
            // the verdict travels WITH the verdict, not just the verdict.
            const SUSPICIOUSLY_SMALL_BYTES: u64 = 150_000;
            let likely_blank_or_locked = bytes < SUSPICIOUSLY_SMALL_BYTES;
            tracing::info!(
                target: "amux::screen", verdict = "capture_ok", measured = true, n_considered = 1,
                bytes, likely_blank_or_locked, "screen capture written"
            );
            Json(json!({
                "ok": true,
                "path": path.display().to_string(),
                "bytes": bytes,
                "likely_blank_or_locked": likely_blank_or_locked,
                "note": if likely_blank_or_locked {
                    "This capture is unusually small for a full-screen PNG, which every solid-black \
                     frame we've seen has been -- the display may still not be showing real content. \
                     Also note: macOS shows its OWN lock screen (not the real desktop) to ANY screen \
                     capture while the Mac is locked, regardless of permission -- that is intentional \
                     OS behavior with no app-level bypass. Fetch /api/screen/capture/file and look \
                     before trusting this as \"what's on screen\"."
                } else { "" },
                "serve": "/api/screen/capture/file",
            }))
            .into_response()
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            // "could not create image from display" is macOS's own text for
            // "Screen Recording permission not granted" — named explicitly so
            // this reads as an OS permission gap, not an amux bug, the first
            // time anyone hits it (two-fixes rule: the log line IS the fix's
            // other half).
            let permission_denied = stderr.contains("could not create image from display");
            tracing::warn!(
                target: "amux::screen", verdict = "capture_failed", measured = true, n_considered = 1,
                permission_denied, %stderr, "screen capture failed"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": stderr,
                    "permission_denied": permission_denied,
                    "what_to_do": if permission_denied {
                        "macOS Screen Recording permission has not been granted to amux-server-rs yet. \
                         Calling this endpoint is itself the trigger for the native permission prompt — \
                         check for it (it may be behind another window) and click Allow, then retry. \
                         Because amux-server-rs signs with a stable local identity (amux-dev), one Allow \
                         persists across every future rebuild."
                    } else {
                        "screencapture exited non-zero for a reason other than permissions; see error"
                    },
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("could not run screencapture: {e}")})),
        )
            .into_response(),
    }
}

/// GET /api/screen/capture/file — the PNG bytes from the last `/capture`,
/// served through the API so a remote (network) viewer can read them the
/// same way `/api/browser/screenshot/file` does — this endpoint itself is
/// still loopback-gated, so "remote" here means another process on the same
/// machine, not the tunnel.
async fn capture_file(peer: Option<Extension<ConnectInfo<SocketAddr>>>) -> Response {
    if let Some(refusal) = loopback_only(peer) {
        return refusal;
    }
    match tokio::fs::read(capture_path()).await {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "no capture yet — call GET /api/screen/capture first"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn peer(ip: std::net::IpAddr) -> Option<Extension<ConnectInfo<SocketAddr>>> {
        Some(Extension(ConnectInfo(SocketAddr::new(ip, 12345))))
    }

    #[tokio::test]
    async fn a_loopback_peer_is_let_through() {
        assert!(loopback_only(peer("127.0.0.1".parse().unwrap())).is_none());
        assert!(loopback_only(peer("::1".parse().unwrap())).is_none());
    }

    #[tokio::test]
    async fn a_lan_or_tunnel_peer_is_refused_before_touching_the_display() {
        let refusal = loopback_only(peer("10.0.0.7".parse().unwrap()))
            .expect("a non-loopback peer must be refused");
        assert_eq!(refusal.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn no_connect_info_at_all_is_refused_not_silently_allowed() {
        // The extractor is `Option<...>` so a misconfigured router (missing
        // `into_make_service_with_connect_info`) yields `None` rather than a
        // rejection — the fail-safe direction here is closed, not open.
        let refusal = loopback_only(None).expect("absent peer info must refuse, not allow");
        assert_eq!(refusal.status(), StatusCode::FORBIDDEN);
    }
}
