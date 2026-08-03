//! Browser-level CDP client for methods `headless_chrome` does not expose.
//!
//! `Browser::new_context()` hardcodes `proxy_server: None`, and `Browser::call_method` is
//! private, so a browser-level method that needs parameters cannot be reached through the
//! crate at all. This is a second CDP client attached to the same endpoint the crate uses
//! (`Browser::get_ws_url()`), which is what makes per-context proxy hosts possible without
//! forking the dependency.
//!
//! **Why a second client is safe.** CDP allows several clients on the browser endpoint; each has
//! its own message-id space, and a response is delivered to the connection that sent the request —
//! so this client's `id: 1` and the crate's `id: 1` never collide. Contexts created here are
//! browser state, not client state, so the id returned by `Target.createBrowserContext` can be
//! handed straight to the crate's own `Context::new` and every later operation (tab creation,
//! navigation, Fetch auth) runs through the crate's transport as usual. Verified against
//! Brave 150 before this was written.
//!
//! # Invariants
//!
//! Running two CDP clients against one browser is safe *because of these rules*, not inherently.
//! Each one is cheap to violate and expensive to debug, so check them before changing anything
//! here.
//!
//! 1. **One call at a time on this socket.** The mutex is held across *send and read*, not just
//!    send. The read loop discards frames that are not the answer to its own id, so two concurrent
//!    calls would let one consume the other's response and then block until the deadline.
//! 2. **Never hold the socket mutex across an `.await`.** It is a `std::sync::Mutex`; every call
//!    here is a short blocking round-trip, which is also how the rest of the pool drives CDP.
//! 3. **Never create a target (tab) here.** Tab and target lifetime has exactly one owner —
//!    `headless_chrome`, which keeps a registry of the tabs it created. A target created on this
//!    socket would exist outside that registry and never be closed by the pool's cleanup paths.
//! 4. **Leave `disposeOnDetach` unset (i.e. `false`).** With it set, contexts would be destroyed
//!    when *this* socket detaches — every reconnect and every `Drop` would tear down live contexts
//!    together with the requests running inside them. Contexts must outlive this client.
//! 5. **Never enable a CDP domain here** (`Runtime`, `Log`, `Network`, …). Domains are enabled on
//!    the tab by the code that owns the tab; enabling one here would both add a fingerprinting
//!    surface and flood this socket's read loop with events it has to skip.
//! 6. **A context must be fully acknowledged before the crate is asked to put a tab in it.** That
//!    holds today because the call is synchronous: the browser's CDP dispatcher is single-threaded,
//!    so a received response means the context exists. Making this fire-and-forget would introduce
//!    the race that does not currently exist.
//! 7. **Do not add methods the crate already exposes.** One owner per operation; this client is
//!    the exception for browser-level calls that are unreachable, not a parallel CDP layer.

use anyhow::{anyhow, bail, Context, Result};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Bound on a single browser-level call, including the reconnect attempt.
///
/// `Target.createBrowserContext` is a local, in-process operation that answers in
/// milliseconds; a wait beyond this means the browser is wedged, and failing the context
/// creation is better than holding a request slot until the gRPC deadline.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Guard against an unbounded event stream starving the response we are waiting for.
const MAX_FRAMES_PER_CALL: usize = 512;

type BrowserSocket = WebSocket<MaybeTlsStream<TcpStream>>;

pub struct BrowserCdpClient {
    ws_url: String,
    /// `None` while disconnected; the next call reconnects.
    ///
    /// A `std::sync::Mutex` is deliberate: every call is a short blocking round-trip and is
    /// never held across an `.await`, matching how the rest of the pool drives CDP.
    socket: Mutex<Option<BrowserSocket>>,
    next_id: AtomicU64,
}

impl BrowserCdpClient {
    /// Attach to a running browser's CDP endpoint.
    ///
    /// Connecting eagerly turns a misconfigured endpoint into a startup failure rather than
    /// into a failure of the first scrape request.
    pub fn connect(ws_url: impl Into<String>) -> Result<Self> {
        let ws_url = ws_url.into();
        let socket = Self::open(&ws_url)?;

        Ok(Self {
            ws_url,
            socket: Mutex::new(Some(socket)),
            next_id: AtomicU64::new(1),
        })
    }

    /// Create a CDP BrowserContext bound to its own proxy.
    ///
    /// `proxy_server` is a `scheme://host:port` URL **without credentials** — Chromium rejects
    /// embedded credentials on a proxy setting, exactly as it does for the `--proxy-server`
    /// switch. Credentials are answered separately over `Fetch.authRequired`.
    ///
    /// `proxyServer` is the only parameter sent, which is what keeps invariant 4 above: an
    /// unspecified `disposeOnDetach` means the context outlives this client's socket. Adding it
    /// here would make every reconnect destroy the contexts currently serving requests.
    pub fn create_browser_context(&self, proxy_server: &str) -> Result<String> {
        let response = self.call(
            "Target.createBrowserContext",
            serde_json::json!({ "proxyServer": proxy_server }),
        )?;

        response
            .pointer("/result/browserContextId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("createBrowserContext returned no browserContextId"))
    }

    /// Issue one method call, reconnecting once if the socket turned out to be dead.
    ///
    /// A browser that lives for hours will outlive individual sockets (proxy resets, the
    /// browser's own idle handling). Reconnecting here keeps that from surfacing as a failed
    /// scrape, while a second failure is reported so the pool's dead-browser recovery can run.
    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        match self.call_once(method, &params) {
            Ok(response) => Ok(response),
            Err(first) => {
                tracing::debug!("Browser CDP call '{method}' failed ({first:#}) - reconnecting");
                self.drop_socket();
                self.call_once(method, &params)
                    .with_context(|| format!("browser CDP call '{method}' failed after reconnect"))
            }
        }
    }

    fn call_once(&self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = serde_json::json!({ "id": id, "method": method, "params": params });

        let mut guard = self
            .socket
            .lock()
            .map_err(|_| anyhow!("browser CDP client is poisoned"))?;

        let socket = match guard.as_mut() {
            Some(socket) => socket,
            None => guard.insert(Self::open(&self.ws_url)?),
        };

        socket
            .send(Message::Text(request.to_string().into()))
            .context("send")?;

        let deadline = Instant::now() + CALL_TIMEOUT;
        for _ in 0..MAX_FRAMES_PER_CALL {
            if Instant::now() >= deadline {
                bail!("timed out waiting for a response to '{method}'");
            }

            // Other clients' traffic and CDP events share this socket; skip anything that is
            // not the answer to this call.
            let Message::Text(text) = socket.read().context("read")? else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).context("parse")?;
            if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                bail!("{method} rejected by the browser: {error}");
            }
            return Ok(value);
        }

        bail!("no response to '{method}' within {MAX_FRAMES_PER_CALL} frames")
    }

    fn drop_socket(&self) {
        if let Ok(mut guard) = self.socket.lock() {
            if let Some(mut socket) = guard.take() {
                let _ = socket.close(None);
            }
        }
    }

    fn open(ws_url: &str) -> Result<BrowserSocket> {
        let (mut socket, _response) = tungstenite::connect(ws_url)
            .with_context(|| format!("connect to the browser CDP endpoint at {ws_url}"))?;

        // Without these, a browser that stops answering blocks the calling task forever:
        // tungstenite's read is a blocking socket read with no deadline of its own.
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream
                .set_read_timeout(Some(CALL_TIMEOUT))
                .context("set read timeout")?;
            stream
                .set_write_timeout(Some(CALL_TIMEOUT))
                .context("set write timeout")?;
        }

        Ok(socket)
    }
}

impl Drop for BrowserCdpClient {
    fn drop(&mut self) {
        self.drop_socket();
    }
}
