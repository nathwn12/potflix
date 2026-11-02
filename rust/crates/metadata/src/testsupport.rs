//! Test support: the fake HTTP server the provider tests point their sources at,
//! the temp directory the cache tests own, and the fake [`Source`] the resolver
//! tests drive the chain with.
//!
//! Go's tests use `net/http/httptest` plus `t.TempDir()`; the workspace pins no
//! HTTP-server crate and no temp-dir crate, so both are small hand-rolled
//! stand-ins here. NOTHING in this module reaches the network: every server
//! binds `127.0.0.1:0` and every address is thrown away with its listener.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;
use metadata_http::http::HttpClient;

use crate::Error;
use crate::resolver::{Done, Options, Resolver, Source};
use crate::wikipedia::WikiClient;

// --- HTTP fakes -------------------------------------------------------------

/// One parsed request.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) raw_query: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    /// `r.URL.Query().Get(key)`: the first value for `key`, percent-decoded with
    /// `+` as a space.
    pub(crate) fn query(&self, key: &str) -> Option<String> {
        for pair in self.raw_query.split('&') {
            let (name, value) = match pair.split_once('=') {
                Some((name, value)) => (name, value),
                None => (pair, ""),
            };
            if decode_query(name) == key {
                return Some(decode_query(value));
            }
        }
        None
    }

    /// A header, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> String {
        self.headers
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }
}

fn decode_query(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// One canned response.
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) content_type: String,
    pub(crate) body: Vec<u8>,
    pub(crate) headers: Vec<(String, String)>,
}

impl Response {
    /// A JSON body with `Content-Type: application/json`.
    pub(crate) fn json(body: &str) -> Response {
        Response {
            status: 200,
            content_type: "application/json".to_string(),
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    /// `http.Error(w, body, status)`.
    pub(crate) fn error(status: u16, body: &str) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: format!("{body}\n").into_bytes(),
            headers: Vec::new(),
        }
    }

    /// An extra header (e.g. `Retry-After`) on the canned response.
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// A `httptest.NewServer` stand-in: HTTP/1.1, keep-alive, one thread per
/// connection, and a connection counter (which is what
/// `TestDecodeCappedReusesConnection` asserts on).
pub(crate) struct FakeServer {
    url: String,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeServer {
    /// Starts a server on an ephemeral loopback port.
    pub(crate) fn start<F>(handler: F) -> FakeServer
    where
        F: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let (thread_stop, thread_conns, thread_handler) = (
            Arc::clone(&stop),
            Arc::clone(&connections),
            Arc::clone(&handler),
        );
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_conns.fetch_add(1, Ordering::SeqCst);
                        let handler = Arc::clone(&thread_handler);
                        std::thread::spawn(move || serve_connection(stream, handler));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });
        FakeServer {
            url: format!("http://{addr}"),
            stop,
            connections,
            handle: Some(handle),
        }
    }

    /// The server's base URL, as `srv.URL`.
    pub(crate) fn url(&self) -> String {
        self.url.clone()
    }

    /// Accepted connections, as `httptest`'s `ConnState` counter counts them.
    pub(crate) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// `httptest.NewServer(...)` then `Close()`: an address that refuses every
/// connection, so any wire attempt surfaces as a transport error.
pub(crate) fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    format!("http://{addr}")
}

fn serve_connection<F>(stream: TcpStream, handler: Arc<F>)
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
{
    // An accepted socket does not reliably inherit the listener's non-blocking
    // mode; a non-blocking read would fail instantly and close the connection
    // right after the first response, breaking keep-alive reuse.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(read_side) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_side);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or("/").to_string();

        let mut headers: HashMap<String, String> = HashMap::new();
        loop {
            let mut raw = String::new();
            if reader.read_line(&mut raw).is_err() {
                return;
            }
            let raw = raw.trim_end_matches(['\r', '\n']).to_string();
            if raw.is_empty() {
                break;
            }
            if let Some((name, value)) = raw.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length: usize = headers
            .get("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if length > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let (path, raw_query) = match target.split_once('?') {
            Some((path, query)) => (path.to_string(), query.to_string()),
            None => (target.clone(), String::new()),
        };
        let request = Request {
            method,
            path,
            raw_query,
            headers: headers.clone(),
            body,
        };
        let response = handler(&request);
        let reason = if response.status == 200 {
            "OK"
        } else {
            "Error"
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            response.status, reason, response.content_type, response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("Connection: keep-alive\r\n\r\n");
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(&response.body).is_err() {
            return;
        }
        if writer.flush().is_err() {
            return;
        }
        if headers
            .get("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"))
        {
            return;
        }
    }
}

// --- temp directories -------------------------------------------------------

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// `t.TempDir()`: a fresh directory under the approved temp root, removed when
/// the test drops it -- including the store file that lives BESIDE it
/// (`<dir>.idx`), which `t.TempDir()`'s root cleanup would have swept.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new() -> TempDir {
        let n = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("potflix-metadata-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    /// The directory as a path string, which is what the cache functions take.
    pub(crate) fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// A child path (`filepath.Join(t.TempDir(), name)`).
    pub(crate) fn child(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let index = PathBuf::from(format!("{}.idx", self.path.to_string_lossy()));
        let _ = std::fs::remove_file(index);
    }
}

// --- fake sources -----------------------------------------------------------

/// The shared test client (`srv.Client()`).
pub(crate) fn http_client() -> HttpClient {
    HttpClient::new(Duration::from_secs(5))
}

/// The shared Wikipedia test client.
pub(crate) fn wiki_client() -> WikiClient {
    WikiClient::new(Duration::from_secs(5))
}

/// Go's `fakeSource`: a [`Source`] whose answer, failure, blocking gate and
/// delay are all the test's to set, with a call counter.
pub(crate) struct FakeSource {
    pub(crate) name: String,
    pub(crate) calls: Arc<AtomicI32>,
    pub(crate) media: Option<Media>,
    pub(crate) err: Option<Error>,
    pub(crate) block: Option<Arc<Done>>,
    pub(crate) delay: Duration,
}

impl FakeSource {
    pub(crate) fn new(name: &str) -> FakeSource {
        FakeSource {
            name: name.to_string(),
            calls: Arc::new(AtomicI32::new(0)),
            media: None,
            err: None,
            block: None,
            delay: Duration::ZERO,
        }
    }

    pub(crate) fn with_media(name: &str, media: Media) -> FakeSource {
        FakeSource {
            media: Some(media),
            ..FakeSource::new(name)
        }
    }

    pub(crate) fn with_error(name: &str, message: &str) -> FakeSource {
        FakeSource {
            err: Some(Error::other(message)),
            ..FakeSource::new(name)
        }
    }

    /// The call counter, so a test that moves the source into the chain can
    /// still read `fake.calls.Load()` afterwards.
    pub(crate) fn counter(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.calls)
    }

    /// The source wrapped as the trait object a [`Resolver`] takes.
    pub(crate) fn shared(self) -> Arc<dyn Source> {
        Arc::new(self)
    }
}

impl Source for FakeSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn find(&self, cancel: &Cancel, _q: &Query) -> Result<Option<Media>, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(block) = &self.block {
            block.wait(cancel);
            if let Some(reason) = cancel.err() {
                return Err(Error::Cancelled(reason));
            }
        }
        if !self.delay.is_zero() {
            cancel.wait(self.delay).map_err(Error::Cancelled)?;
        }
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        Ok(self.media.clone())
    }
}

/// `seriesMedia`: a found series with one episode, "Pilot" at S1E2.
pub(crate) fn series_media() -> Media {
    Media {
        source: "fake".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Show".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 2,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

/// `mislabeledMedia` mimics live AniList 21355: `streamingEpisodes` numbered
/// 51..66 (a later season) served for an absolute episode-1 query.
pub(crate) fn mislabeled_media() -> Media {
    Media {
        source: "fake-first".to_string(),
        source_id: "21355".to_string(),
        kind: Kind::SERIES,
        title: "Re Zero".to_string(),
        episodes: vec![
            Episode {
                season: 0,
                number: 51,
                title: "Theatrical Malice".to_string(),
                ..Episode::default()
            },
            Episode {
                season: 0,
                number: 66,
                title: "The Result of the Battle for Priestella".to_string(),
                ..Episode::default()
            },
        ],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

/// `correctMedia`: the source that does carry absolute episode 1.
pub(crate) fn correct_media() -> Media {
    Media {
        source: "fake-second".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Re Zero".to_string(),
        episodes: vec![Episode {
            season: 0,
            number: 1,
            title: "True S1E1".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

/// A resolver over `dir` with the given sources and a 5 s timeout.
pub(crate) fn resolver_with(dir: &str, sources: Vec<Arc<dyn Source>>) -> Resolver {
    Resolver::new(Options {
        dir: dir.to_string(),
        timeout: Duration::from_secs(5),
        sources: Some(sources),
        ..Options::default()
    })
}
