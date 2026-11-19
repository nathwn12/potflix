
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::win::NamedPipe;
use crate::{read_frame, write_frame, Opcode};

/// The default per-direction pipe buffer.
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// Keeps two servers created in the same clock tick apart under Rust's parallel
/// test threads; the Go original relies on a nanosecond timestamp alone.
static SERVER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Creates one named-pipe instance per `accept` under `base + suffix`.
pub struct FakeServer {
    /// Prefix; the full name is `base + suffix`.
    pub base: String,
    pub suffix: i32,
    /// Overrides the per-direction pipe buffer size (0 = `PIPE_BUFFER_BYTES`). A
    /// tiny value lets a test make the client's write block against a server
    /// that has stopped draining.
    pub buffer_bytes: u32,
}

impl FakeServer {
    /// Returns a server bound to a process-unique pipe namespace. `suffix`
    /// selects which `discord-ipc-N` index it occupies, so a test can force the
    /// client to fall back past lower suffixes.
    pub fn new(suffix: i32) -> FakeServer {
        FakeServer {
            base: format!(
                r"\\.\pipe\potflix-fake-{}-{}-{}-",
                std::process::id(),
                nanos(),
                SERVER_SEQ.fetch_add(1, Ordering::SeqCst)
            ),
            suffix,
            buffer_bytes: 0,
        }
    }

    /// The full pipe path this server listens on.
    pub fn name(&self) -> String {
        format!("{}{}", self.base, self.suffix)
    }

    /// Creates a fresh pipe instance and blocks until a client opens it. A second
    /// call after a drop accepts the reconnect on a new instance.
    pub fn accept(&self) -> io::Result<FakeConn> {
        let buf = if self.buffer_bytes == 0 {
            PIPE_BUFFER_BYTES
        } else {
            self.buffer_bytes
        };
        let pipe = NamedPipe::create_server(&self.name(), buf)?;
        Ok(FakeConn { pipe })
    }
}

/// One accepted server-side connection.
pub struct FakeConn {
    pipe: NamedPipe,
}

impl FakeConn {
    /// Reads the next client frame.
    pub fn read_frame(&self) -> io::Result<(Opcode, Vec<u8>)> {
        read_frame(&self.pipe)
    }

    /// Writes one server frame.
    pub fn write_frame(&self, op: Opcode, payload: &[u8]) -> io::Result<()> {
        write_frame(&self.pipe, op, payload)
    }

    /// Drops the connection, which the client must detect and reconnect from.
    /// (Go returns `os.File.Close`'s error; the Win32 handle close reports none.)
    pub fn close(&self) {
        self.pipe.close();
    }
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}
