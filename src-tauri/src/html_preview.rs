// Scripts-enabled HTML preview (§16): serve a previewed file from its OWN origin via the
// `buoyhtml:` URI scheme, so the file's inline scripts can run WITHOUT loosening the app's CSP.
//
// Why a custom protocol instead of just allowing inline script in the viewer iframe:
// a `srcdoc` iframe INHERITS the parent document's CSP, and CSP can only ever be intersected,
// never relaxed, by a child. So making srcdoc content scriptable would mean putting
// 'unsafe-inline' on the APP's own script-src — and the app renders untrusted terminal output,
// which would turn a contained problem into an app-origin XSS surface. A custom scheme is a
// separate origin with a per-response CSP, so the permission applies to the previewed file only.
//
// The isolation that makes this safe (all four verified in a real WKWebView, see tests + §16):
//   1. The frame is `sandbox="allow-scripts"` WITHOUT `allow-same-origin`, so the document gets an
//      OPAQUE origin: no access to our DOM, no localStorage, no reading our page.
//   2. wry injects the Tauri IPC bootstrap into the MAIN FRAME ONLY, so `window.__TAURI__`,
//      `__TAURI_INTERNALS__` and `window.ipc` are all undefined in the frame — measured, and an
//      attack page's direct `invoke()` attempt throws instead of reaching a command.
//   3. No `allow-top-navigation`/`allow-popups`, so the page can't navigate the app away or spawn
//      windows; `top.location` access is denied.
//   4. The response CSP has `frame-src 'none'` and no `'self'` anywhere, so the page can't reload
//      the app's own origin into a subframe or fetch it.
//
// The bytes come from the RENDERER (already fetched via read_remote_file) rather than being re-read
// here: the file may be remote, and re-fetching it would need a second ssh round-trip and risk
// serving different content than the user chose to enable. Registration is one-shot per file with
// a random token in the URL, so a stale/guessed URL can't pull a document the user didn't opt into.

use std::collections::HashMap;
use std::sync::Mutex;

/// The URI scheme registered for scripted previews. Must match the `frame-src` entry in
/// tauri.conf.json's CSP, and the scheme used by the renderer when it sets the iframe src.
pub const SCHEME: &str = "buoyhtml";

/// Per-response CSP for a scripted preview. Scoped to THIS document only.
///
/// - `'unsafe-inline'`/`'unsafe-eval'` in script-src: the whole point — the file's own inline
///   `<script>`/`<script type="module">` must run, and bundler-less ESM shims commonly eval.
/// - `https:` for script/style/font/img/connect: real exported HTML pulls React/mermaid/etc from a
///   CDN (esm.sh, unpkg, jsdelivr) and fonts from Google. Allowing https: broadly matches what a
///   browser would do for the same file, and avoids a per-CDN allowlist that silently breaks files.
/// - `default-src 'none'` so anything not named above is denied, and NO `'self'`: 'self' here means
///   the buoyhtml origin, but being explicit keeps a future edit from accidentally granting the
///   app's origin. `frame-src 'none'` blocks re-framing anything, including the app.
/// - No `http:` — plaintext fetches are refused; a downgraded CDN just fails to load.
const PREVIEW_CSP: &str = "default-src 'none'; \
     script-src 'unsafe-inline' 'unsafe-eval' https:; \
     style-src 'unsafe-inline' https:; \
     font-src https: data:; \
     img-src https: data: blob:; \
     media-src https: data: blob:; \
     connect-src https:; \
     frame-src 'none'; \
     form-action 'none'; \
     base-uri 'none'";

/// Documents the user has opted into running scripts for, keyed by a single-use random token.
/// Held in Tauri state; the protocol handler looks up the token from the request path.
#[derive(Default)]
pub struct PreviewStore {
    docs: Mutex<HashMap<String, Vec<u8>>>,
}

impl PreviewStore {
    /// Register `bytes` under a fresh token and return it. The renderer then loads
    /// `buoyhtml://localhost/<token>`.
    pub fn put(&self, bytes: Vec<u8>) -> String {
        let token = new_token();
        let mut docs = self.docs.lock().unwrap();
        // One live scripted preview at a time. Dropping older entries keeps a long session from
        // accumulating multi-MB documents, and means a closed tab's URL stops resolving.
        docs.clear();
        docs.insert(token.clone(), bytes);
        token
    }

    /// Fetch a registered document. Returns None for an unknown/expired token.
    fn get(&self, token: &str) -> Option<Vec<u8>> {
        self.docs.lock().unwrap().get(token).cloned()
    }
}

/// Random hex token, from the OS RNG via `getrandom(2)`-backed `/dev/urandom`. Not guessable, so a
/// page can't enumerate `buoyhtml://` URLs to read a document the user never opted into.
fn new_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    // Fallback: still unique per call, just not cryptographically random. Combined with the
    // opaque-origin + no-IPC isolation this is not the load-bearing control.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:032x}")
}

/// Extract the token from a `buoyhtml://.../<token>` request URL, ignoring any query/fragment.
/// Pure so it can be unit-tested without a webview.
pub fn token_from_path(path: &str) -> &str {
    let p = path.split(['?', '#']).next().unwrap_or("");
    p.rsplit('/').next().unwrap_or("")
}

/// Build the HTTP response for a preview request: the opted-in document under `PREVIEW_CSP`, or a
/// 404 when the token is unknown (tab closed, app restarted, URL guessed).
pub fn respond(store: &PreviewStore, url_path: &str) -> (u16, &'static str, Vec<u8>) {
    match store.get(token_from_path(url_path)) {
        Some(bytes) => (200, PREVIEW_CSP, bytes),
        // Deny-by-default body, and still CSP'd so even the error page can't run anything.
        None => (404, "default-src 'none'", b"<!doctype html><title>404</title>not found".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TC-HP1 a registered doc is served under the scripts-enabled CSP; an unknown token 404s.
    #[test]
    fn tc_hp1_serves_registered_token_only() {
        let store = PreviewStore::default();
        let token = store.put(b"<h1>hi</h1>".to_vec());

        let (status, csp, body) = respond(&store, &format!("/{token}"));
        assert_eq!(status, 200);
        assert_eq!(body, b"<h1>hi</h1>");
        assert!(csp.contains("script-src 'unsafe-inline'"), "scripts enabled for the doc");

        let (status, csp, _) = respond(&store, "/deadbeef");
        assert_eq!(status, 404, "unknown token is not served");
        assert_eq!(csp, "default-src 'none'", "404 page runs nothing");
    }

    // TC-HP2 the preview CSP must never grant the app's own origin, and must keep scripts
    // confined to the preview document. These are the properties that make the separate origin
    // worth having, so they are asserted rather than left to review.
    #[test]
    fn tc_hp2_preview_csp_does_not_grant_app_origin() {
        assert!(PREVIEW_CSP.starts_with("default-src 'none'"), "deny by default");
        assert!(!PREVIEW_CSP.contains("'self'"), "no 'self' — never reach the app origin");
        assert!(PREVIEW_CSP.contains("frame-src 'none'"), "cannot re-frame the app");
        assert!(PREVIEW_CSP.contains("base-uri 'none'"), "cannot rewrite relative URL resolution");
        assert!(PREVIEW_CSP.contains("form-action 'none'"), "cannot exfiltrate via form post");
        // http: would let a MITM'd CDN inject script into the preview
        assert!(!PREVIEW_CSP.contains("http:;") && !PREVIEW_CSP.contains(" http: "),
            "no plaintext http: sources");
    }

    // TC-HP3 tokens are unique and unguessable-ish, and registering a new doc retires the old URL
    // (so a closed preview tab's URL stops resolving).
    #[test]
    fn tc_hp3_tokens_unique_and_single_live_doc() {
        let store = PreviewStore::default();
        let a = store.put(b"first".to_vec());
        let b = store.put(b"second".to_vec());
        assert_ne!(a, b, "each registration gets a fresh token");
        assert_eq!(a.len(), 32, "128-bit hex token");
        assert_eq!(respond(&store, &format!("/{b}")).2, b"second");
        assert_eq!(respond(&store, &format!("/{a}")).0, 404, "previous doc is retired");
    }

    // TC-HP4 token parsing tolerates the URL shapes a webview actually sends.
    #[test]
    fn tc_hp4_token_from_path() {
        assert_eq!(token_from_path("/abc123"), "abc123");
        assert_eq!(token_from_path("abc123"), "abc123");
        assert_eq!(token_from_path("/abc123?v=1"), "abc123");
        assert_eq!(token_from_path("/abc123#frag"), "abc123");
        assert_eq!(token_from_path("/some/path/abc123"), "abc123");
        assert_eq!(token_from_path(""), "");
    }
}
