
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// One scripted reply. The list is consumed in order; a request past the end
/// of the list gets the last reply again.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub reason: &'static str,
    pub retry_after: Option<String>,
    pub body: String,
}

impl Reply {
    pub fn ok_json(body: impl Into<String>) -> Self {
        Reply {
            status: 200,
            reason: "OK",
            retry_after: None,
            body: body.into(),
        }
    }

    pub fn too_many_requests(retry_after: Option<&str>) -> Self {
        Reply {
            status: 429,
            reason: "Too Many Requests",
            retry_after: retry_after.map(str::to_string),
            body: "{\"error\":\"rate limited\"}".to_string(),
        }
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    /// Requests served, in order.
    pub hits: Arc<AtomicUsize>,
    /// Accepted sockets.
    pub connections: Arc<AtomicUsize>,
    /// Raw request heads, in order.
    pub heads: Arc<Mutex<Vec<String>>>,
    /// Request bodies (the bytes past `Content-Length`), in order.
    pub bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(replies: Vec<Reply>) -> Self {
        assert!(
            !replies.is_empty(),
            "a test server needs at least one reply"
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let replies = Arc::new(Mutex::new(replies));

        let handle = {
            let hits = Arc::clone(&hits);
            let connections = Arc::clone(&connections);
            let heads = Arc::clone(&heads);
            let bodies = Arc::clone(&bodies);
            let stop = Arc::clone(&stop);
            thread::spawn(move || loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        connections.fetch_add(1, Ordering::SeqCst);
                        serve_connection(stream, &replies, &hits, &heads, &bodies, &stop);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            })
        };

        TestServer {
            addr,
            hits,
            connections,
            heads,
            bodies,
            stop,
            handle: Some(handle),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn heads(&self) -> Vec<String> {
        self.heads.lock().expect("heads").clone()
    }

    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("bodies").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(
    mut stream: TcpStream,
    replies: &Mutex<Vec<Reply>>,
    hits: &AtomicUsize,
    heads: &Mutex<Vec<String>>,
    bodies: &Mutex<Vec<String>>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return, // client closed the connection
                Ok(_) => {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        heads.lock().expect("heads").push(head.clone());

        let mut body = Vec::new();
        if let Some(length) = content_length(&head) {
            body.resize(length, 0);
            if stream.read_exact(&mut body).is_err() {
                return;
            }
        }
        bodies
            .lock()
            .expect("bodies")
            .push(String::from_utf8_lossy(&body).into_owned());

        let index = hits.fetch_add(1, Ordering::SeqCst);
        let reply = {
            let scripted = replies.lock().expect("replies");
            match scripted.get(index) {
                Some(reply) => reply.clone(),
                None => scripted.last().expect("a reply").clone(),
            }
        };

        let mut response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            reply.status,
            reply.reason,
            reply.body.len()
        );
        if let Some(retry_after) = &reply.retry_after {
            response.push_str(&format!("Retry-After: {retry_after}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&reply.body);
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}
