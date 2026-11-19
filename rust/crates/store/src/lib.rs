//! `store` -- mirrors the Go package `go\internal\store`.
//!
//! A crash-safe single-file key/value store for the Potflix library index and
//! the metadata cache. It replaces two shapes that both paid full cost for a
//! small change: a 565,380-byte JSON index rewritten in full on every dirty
//! checkpoint, and 1,118 loose one-JSON-per-key cache files. Here ~25,000
//! records of ~150 bytes live in one append-only frame log behind a 24-byte
//! header: [`open`] reads the file sequentially and rebuilds the map in tens of
//! milliseconds, a checkpoint appends only the keys that changed, and the delta
//! folds back into a snapshot once it grows past [`Options::compact_threshold`].
//!
//! # File format (version 1, locked)
//!
//! ```text
//! header, 24 bytes:
//!   bytes  0..8   magic "PFLXST01"
//!   u32    8..12  formatVersion = 1
//!   u32   12..16  flags (0)
//!   u32   16..20  crc32-IEEE over bytes 0..16
//!   u32   20..24  reserved (0)
//!
//! frames, appended after the header, one per mutation:
//!   u8    tag (1 = put, 2 = delete/tombstone)
//!   u32   payloadLen
//!   u32   crc32-IEEE(payload)
//!   payload
//!
//!   tag 1 payload: uvarint keyLen, key, uvarint valueLen, value
//!   tag 2 payload: uvarint keyLen, key
//! ```
//!
//! Every integer is little-endian; lengths are LEB128 unsigned varints. The CRC
//! is hand-rolled and table-driven ([`crc32`], no crate: the workspace
//! dependency policy). Bytes are bytes: keys and values are `&[u8]`, not
//! `&str`, because the Go original stores and returns arbitrary byte strings
//! (`TestRoundTrip` round-trips a key with `\xff\xfe` in it) and also the empty
//! key. The format is proved against files WRITTEN BY THE GO IMPLEMENTATION --
//! `tests/fixtures/go-*.pflx`, reproduced byte for byte in this crate's tests.
//!
//! # Crash safety
//!
//! A frame is accepted only when its tag is known, its payloadLen fits in the
//! bytes left in the file, its CRC matches, and its payload decodes exactly. The
//! first frame that fails any of those checks ends replay: every earlier record
//! is kept, the invalid tail is discarded, and the file is truncated back to the
//! last valid frame boundary. A torn tail is the normal outcome of an abrupt
//! process kill and is never reported as an error. A file whose header is wrong
//! -- bad magic, unexpected version, or a header CRC mismatch -- IS an error: it
//! is not this store's file. A file shorter than the header (a header write that
//! never finished) is started over, because the store owns the file.
//!
//! # Compaction
//!
//! [`Db::compact`] writes a fresh file (header plus one put frame per live key,
//! in SORTED key order, so the snapshot of a given state is byte-identical
//! whatever order the mutations arrived in) to a temp file in the same
//! directory, syncs it, and renames it over the original; the temp file is
//! removed on failure. The delta accounting resets on every successful open
//! repair and compaction.
//!
//! # Concurrency and process model
//!
//! One mutex guards the in-memory map and the append file, so [`Db::get`],
//! [`Db::put`], [`Db::delete`], [`Db::len`], [`Db::for_each`], [`Db::commit`],
//! [`Db::compact`] and [`Db::stats`] are safe from concurrent threads. Unlike
//! the Go original, [`Db::for_each`] takes `&self`, but it does hold that mutex
//! for the whole iteration, so **the callback must not call back into the same
//! `Db`: `std::sync::Mutex` is not re-entrant and the call will deadlock**. Go
//! returns a slice aliasing stored memory from `Get`; here it is an
//! `Arc<[u8]>`: a refcount bump, not a copy, and still read-only by type.
//!
//! The file is held open read-write for the `Db`'s lifetime and the store
//! assumes a single writing process -- the app holds a single-instance lock on
//! `127.0.0.1:17843` -- so opening the same file from a second process is
//! unsupported.
//!
//! # Removal while open
//!
//! The file is opened with a share mode that lets another party delete it while
//! this process still holds it (`FILE_SHARE_DELETE` on Windows, [`sys`]), so
//! removing the store file -- or the directory holding it -- never fails because
//! this `Db` is open. The removal is reported rather than papered over: reads
//! ([`Db::get`], [`Db::len`], [`Db::for_each`]) keep serving the in-memory
//! records, because they claim no durability, and the next durability claim --
//! [`Db::commit`], a mutation under `Options::sync_on_commit`, or [`Db::close`]
//! -- returns [`Error::Removed`] once the file is no longer at the store's path.
//! Bytes appended before that report went into the orphaned file and are not
//! visible at the path; the error is the only signal, and the in-memory records
//! stay authoritative. From then on every mutation and `commit` keeps reporting
//! it, like any other append failure, until [`Db::compact`] writes a fresh
//! snapshot at the path (recreating the file, when its directory still exists)
//! and clears the failure.
//!
//! # Rust-specific notes
//!
//! * Keys and values are byte strings; every key parameter takes
//!   `impl AsRef<[u8]>`, so `db.get("series|breaking bad|2008|1")` and
//!   `db.get(b"k1")` both compile.
//! * `Drop` flushes and releases like [`Db::close`], ignoring the result: there
//!   is no caller left to report to. The Go original leaves a forgotten
//!   `defer db.Close()` to the GC instead, losing the buffered tail.

mod crc32;
mod sys;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crc32::crc32;

const MAGIC: [u8; 8] = *b"PFLXST01";
const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: usize = 24;
const TAG_PUT: u8 = 1;
const TAG_DELETE: u8 = 2;

/// 1 MiB of delta before an automatic compact.
const DEFAULT_COMPACT_THRESHOLD: i64 = 1 << 20;
/// The append buffer size, and so also the streaming size of a snapshot, as the
/// Go original's `bufio` sizes.
const APPEND_BUF_SIZE: usize = 64 << 10;
/// The replay read-ahead size.
const REPLAY_BUF_SIZE: usize = 1 << 20;

/// The map behind the store: byte keys, shared byte values.
type Records = HashMap<Vec<u8>, Arc<[u8]>>;

/// Options for [`open`]. [`Options::default`] is usable as it is: no per-write
/// fsync, a 1 MiB delta budget, and no size limit on values.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Makes every `put` and `delete` fsync before returning, as if each were
    /// followed by [`Db::commit`]. Off by default; an abrupt process kill is
    /// already survivable without it, since a torn tail is repaired on the next
    /// [`open`]. Turn it on only for host crashes or power loss.
    pub sync_on_commit: bool,

    /// The delta budget in bytes: once that many bytes have been appended since
    /// the last snapshot, the next `put` or `delete` rewrites the file as a
    /// fresh snapshot (an O(records) rewrite inside that one call). Zero or
    /// negative means the 1 MiB default.
    pub compact_threshold: i64,

    /// Rejects a longer value with [`Error::ValueTooLarge`]. Zero or negative
    /// means unlimited.
    pub max_value_size: i64,
}

impl Options {
    /// `normalizeOptions`: the zero value is usable.
    fn normalized(self) -> Options {
        Options {
            sync_on_commit: self.sync_on_commit,
            compact_threshold: if self.compact_threshold <= 0 {
                DEFAULT_COMPACT_THRESHOLD
            } else {
                self.compact_threshold
            },
            max_value_size: if self.max_value_size < 0 {
                0
            } else {
                self.max_value_size
            },
        }
    }
}

/// A point-in-time snapshot of the store's accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live keys.
    pub records: i64,
    /// File bytes at the last baseline (`open` or `compact`).
    pub snapshot_bytes: i64,
    /// Frame bytes appended since that baseline.
    pub delta_bytes: i64,
    /// Rewrites this session (not persisted).
    pub compactions: i64,
}

/// The errors a store operation reports.
///
/// The three sentinels of the Go original are the first three variants, and
/// [`Error::is_removed`] is `errors.Is(err, ErrRemoved)`: it also sees through
/// the sticky wrapper, so every caller sees one predicate instead of the
/// wrapping shape.
#[derive(Clone, Debug)]
pub enum Error {
    /// `ErrClosed`: every mutating method after [`Db::close`].
    Closed,
    /// `ErrValueTooLarge`: a value longer than `Options::max_value_size`.
    ValueTooLarge,
    /// `ErrRemoved`: a durability claim (`commit`, a mutation under
    /// `sync_on_commit`, `close`) after another party removed the store file or
    /// its directory.
    Removed(PathBuf),
    /// The sticky append/flush failure recorded first, reported by every
    /// mutation and `commit` until [`Db::compact`] repairs the file. The Go
    /// original wraps its sticky error the same way.
    EarlierAppendFailed(Arc<Error>),
    /// An OS call failed.
    Io {
        op: &'static str,
        path: PathBuf,
        /// `Arc` so the error stays copyable into the sticky slot.
        source: Arc<io::Error>,
    },
    /// The file does not start with `PFLXST01`, so it is not a store file.
    BadMagic,
    /// The header's format version is not 1.
    UnsupportedVersion(u32),
    /// The header CRC does not cover the header's first 16 bytes.
    HeaderCrcMismatch { got: u32, want: u32 },
}

impl Error {
    fn io(op: &'static str, path: &Path, source: io::Error) -> Error {
        Error::Io {
            op,
            path: path.to_path_buf(),
            source: Arc::new(source),
        }
    }

    /// `errors.Is(err, ErrClosed)`.
    pub fn is_closed(&self) -> bool {
        matches!(self, Error::Closed)
    }

    /// `errors.Is(err, ErrValueTooLarge)`.
    pub fn is_value_too_large(&self) -> bool {
        matches!(self, Error::ValueTooLarge)
    }

    /// `errors.Is(err, ErrRemoved)`, through the sticky wrapper.
    pub fn is_removed(&self) -> bool {
        match self {
            Error::Removed(_) => true,
            Error::EarlierAppendFailed(inner) => inner.is_removed(),
            _ => false,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Closed => write!(f, "store: database is closed"),
            Error::ValueTooLarge => write!(f, "store: value exceeds MaxValueSize"),
            Error::Removed(path) => {
                write!(f, "{}: store: file was removed while open", path.display())
            }
            Error::EarlierAppendFailed(inner) => {
                write!(
                    f,
                    "store: earlier append failed, Compact to repair: {inner}"
                )
            }
            Error::Io { op, path, source } => write!(f, "store: {op} {}: {source}", path.display()),
            Error::BadMagic => write!(f, "store: bad magic, not a store file"),
            Error::UnsupportedVersion(v) => write!(f, "store: unsupported format version {v}"),
            Error::HeaderCrcMismatch { got, want } => {
                write!(
                    f,
                    "store: header CRC mismatch: got {got:08x} want {want:08x}"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::EarlierAppendFailed(inner) => Some(&**inner),
            Error::Io { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

/// One open store file. It is safe for concurrent use.
///
/// The path lives inside the mutex (the Go original reads `db.path` while
/// holding its own mutex), so every helper below reaches it under the lock.
pub struct Db {
    opts: Options,
    inner: Mutex<Inner>,
}

struct Inner {
    path: PathBuf,
    /// The append file, held open read-write until `close`.
    f: Option<File>,
    /// Pending appends, flushed by `commit`/`close`/a full buffer, the
    /// equivalent of the Go original's `bufio.Writer`.
    buf: Vec<u8>,
    /// Authoritative in-memory state.
    records: Records,

    snapshot_bytes: i64,
    delta_bytes: i64,
    compactions: i64,
    closed: bool,
    /// Sticky append/flush failure; `compact` clears it.
    fail_err: Option<Arc<Error>>,

    /// Scratch for one frame payload.
    payload_buf: Vec<u8>,
    /// Scratch for one whole frame.
    frame_buf: Vec<u8>,
}

/// Opens or creates the store at `path`, loading the snapshot and replaying the
/// delta log. A truncated or corrupt tail is discarded and the file is
/// truncated back to the last valid frame boundary, which is not an error. A
/// file whose header is wrong -- bad magic, unexpected version, or a header CRC
/// mismatch -- is an error: it is not this store's file.
///
/// The file is opened so that another party may still remove it while this `Db`
/// holds it; the durability claims then report [`Error::Removed`] (see the
/// crate doc's "Removal while open").
pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Db, Error> {
    let opts = opts.normalized();
    let path = path.as_ref().to_path_buf();
    let file = sys::open_store_file(&path).map_err(|e| Error::io("open", &path, e))?;
    let mut inner = Inner {
        path,
        f: Some(file),
        buf: Vec::with_capacity(APPEND_BUF_SIZE),
        records: Records::new(),
        snapshot_bytes: 0,
        delta_bytes: 0,
        compactions: 0,
        closed: false,
        fail_err: None,
        payload_buf: Vec::new(),
        frame_buf: Vec::new(),
    };
    let valid = inner.replay()?; // a bad header drops the handle with `inner`
    {
        let Inner { path, f, .. } = &mut inner;
        let f = f.as_mut().ok_or(Error::Closed)?;
        f.seek(SeekFrom::Start(valid))
            .map_err(|e| Error::io("seek", path, e))?;
    }
    inner.snapshot_bytes = valid as i64;
    Ok(Db {
        opts,
        inner: Mutex::new(inner),
    })
}

/// A read-only inspection of a store file: its size, the offset of the last
/// valid frame boundary, and every live record as a `(key, value)` pair in
/// sorted key order.
pub struct Inspection {
    /// The file's size in bytes.
    pub size: u64,
    /// The offset of the last valid frame boundary.
    pub valid: u64,
    /// Live records, sorted by key.
    pub records: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Inspects the store file at `path` WITHOUT modifying it: the handle is opened
/// read-only and the file is never written, truncated, or created. Unlike
/// [`open`], a torn tail is reported through [`Inspection::valid`] instead of
/// being repaired in place, so this is the probe path for an owner's real cache.
pub fn inspect(path: impl AsRef<Path>) -> Result<Inspection, Error> {
    let path = path.as_ref();
    let mut f = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let size = f.metadata().map_err(|e| Error::io("stat", path, e))?.len();
    let mut hdr = [0u8; HEADER_SIZE];
    read_header(path, &mut f, &mut hdr)?;
    f.seek(SeekFrom::Start(HEADER_SIZE as u64))
        .map_err(|e| Error::io("seek", path, e))?;
    let mut records = Records::new();
    let mut payload_buf = Vec::new();
    let valid = {
        let mut r = BufReader::with_capacity(REPLAY_BUF_SIZE, &mut f);
        replay_frames(&mut r, size, &mut records, &mut payload_buf)
    };
    let mut out: Vec<(Vec<u8>, Vec<u8>)> =
        records.into_iter().map(|(k, v)| (k, v.to_vec())).collect();
    out.sort();
    Ok(Inspection {
        size,
        valid,
        records: out,
    })
}

impl Db {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panicking `for_each` callback must not poison the store for every
        // later caller: the in-memory map and the file are still consistent.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reports the value stored under `key`, or `None`. The Go original returns
    /// a slice aliasing stored memory; this returns a shared handle to those
    /// bytes, so it is still read-only by type and still no copy.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Option<Arc<[u8]>> {
        self.lock().records.get(key.as_ref()).cloned()
    }

    /// The number of live keys. It remains valid after [`Db::close`].
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    /// Whether no key is live. Rust-side convenience over [`Db::len`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stores `value` under `key`, appending one frame. The value is copied, so
    /// the caller may reuse it; a zero-length value is legal. If the delta
    /// budget is exhausted the call also rewrites the file as a fresh snapshot.
    pub fn put(&self, key: impl AsRef<[u8]>, value: &[u8]) -> Result<(), Error> {
        if self.opts.max_value_size > 0 && value.len() as i64 > self.opts.max_value_size {
            return Err(Error::ValueTooLarge);
        }
        let mut inner = self.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.fail_err.is_some() {
            return Err(inner.failed());
        }
        // Update memory first so a compaction triggered by this very append
        // already sees the new record.
        inner
            .records
            .insert(key.as_ref().to_vec(), Arc::from(value));
        if let Err(e) = inner.append_put(key.as_ref(), value, self.opts.compact_threshold) {
            return Err(inner.fail(e));
        }
        inner.after_append(self.opts.sync_on_commit)
    }

    /// Removes `key` with a tombstone frame. Deleting an absent key is a no-op
    /// and writes nothing.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<(), Error> {
        let key = key.as_ref();
        let mut inner = self.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if !inner.records.contains_key(key) {
            return Ok(());
        }
        if inner.fail_err.is_some() {
            return Err(inner.failed());
        }
        inner.records.remove(key);
        if let Err(e) = inner.append_delete(key, self.opts.compact_threshold) {
            return Err(inner.fail(e));
        }
        inner.after_append(self.opts.sync_on_commit)
    }

    /// Calls `f` for every live record in unspecified order. `f` returns false
    /// to stop early. It runs with the store locked: **`f` must not call any
    /// method of this `Db`, or it deadlocks** (`std::sync::Mutex` is not
    /// re-entrant).
    pub fn for_each<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> bool,
    {
        let inner = self.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        for (key, value) in &inner.records {
            if !f(key, value) {
                break;
            }
        }
        Ok(())
    }

    /// Flushes buffered appends and fsyncs them: after `commit` returns, the
    /// records are on disk. It is the explicit durability barrier and always
    /// fsyncs, whatever `sync_on_commit` says (that option only makes every
    /// `put` and `delete` do the same). It reports [`Error::Removed`] when the
    /// file was removed while this `Db` held it open.
    pub fn commit(&self) -> Result<(), Error> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.fail_err.is_some() {
            return Err(inner.failed());
        }
        if let Err(e) = inner.flush_locked(true) {
            return Err(inner.fail(e));
        }
        Ok(())
    }

    /// Rewrites the file as a fresh snapshot of the live records: a temp file in
    /// the same directory, synced, then renamed over the original. It also
    /// clears a sticky append failure. `delta_bytes` resets to zero.
    pub fn compact(&self) -> Result<(), Error> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        inner.compact()
    }

    /// The current counters.
    pub fn stats(&self) -> Stats {
        let inner = self.lock();
        Stats {
            records: inner.records.len() as i64,
            snapshot_bytes: inner.snapshot_bytes,
            delta_bytes: inner.delta_bytes,
            compactions: inner.compactions,
        }
    }

    /// Flushes and fsyncs the pending appends and releases the file. A second
    /// `close` is a no-op. The in-memory records remain readable through
    /// `get`/`len` (the Go doc comment on `ErrClosed` claims otherwise; the
    /// code and `TestRoundTrip` are the authority). Like `commit`, it reports
    /// [`Error::Removed`] when the file is no longer at its path.
    pub fn close(&self) -> Result<(), Error> {
        let mut inner = self.lock();
        if inner.closed {
            return Ok(());
        }
        inner.closed = true;
        let err = inner.flush_locked(true);
        inner.f = None;
        err
    }
}

impl Drop for Db {
    /// Flushes and releases like [`Db::close`], ignoring the result: the Go
    /// original has no equivalent (a forgotten `Close` loses the buffered tail
    /// to the GC), and Rust's RAII contract is better served by not dropping
    /// the pending records on the floor.
    fn drop(&mut self) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        inner.closed = true;
        let _ = inner.flush_locked(true);
        inner.f = None;
    }
}

impl Inner {
    /// `replay`: rebuilds the map from the file and returns the offset of the
    /// last valid frame boundary, truncating any invalid tail.
    fn replay(&mut self) -> Result<u64, Error> {
        let Inner {
            path,
            f,
            records,
            payload_buf,
            ..
        } = self;
        let path: &Path = path;
        let f = f.as_mut().ok_or(Error::Closed)?;
        let size = f.metadata().map_err(|e| Error::io("stat", path, e))?.len();

        if size == 0 {
            write_header(path, f)?;
            return Ok(HEADER_SIZE as u64);
        }
        if size < HEADER_SIZE as u64 {
            // A header write that never finished. The store owns the file, so
            // start it over rather than fail.
            f.set_len(0).map_err(|e| Error::io("truncate", path, e))?;
            write_header(path, f)?;
            return Ok(HEADER_SIZE as u64);
        }

        let mut hdr = [0u8; HEADER_SIZE];
        read_header(path, f, &mut hdr)?;
        f.seek(SeekFrom::Start(HEADER_SIZE as u64))
            .map_err(|e| Error::io("seek", path, e))?;
        let valid = {
            let mut r = BufReader::with_capacity(REPLAY_BUF_SIZE, &mut *f);
            replay_frames(&mut r, size, records, payload_buf)
        };

        if valid != size {
            f.set_len(valid)
                .map_err(|e| Error::io("truncate torn tail", path, e))?;
        }
        Ok(valid)
    }

    /// `appendPutLocked`: builds the put payload.
    fn append_put(&mut self, key: &[u8], value: &[u8], threshold: i64) -> Result<(), Error> {
        {
            let Inner { payload_buf, .. } = &mut *self;
            payload_buf.clear();
            append_uvarint(payload_buf, key.len() as u64);
            payload_buf.extend_from_slice(key);
            append_uvarint(payload_buf, value.len() as u64);
            payload_buf.extend_from_slice(value);
        }
        self.append_frame(TAG_PUT, threshold)
    }

    /// `afterAppendLocked`: honours `sync_on_commit` for one mutation.
    fn after_append(&mut self, sync: bool) -> Result<(), Error> {
        if !sync {
            return Ok(());
        }
        if let Err(e) = self.flush_locked(true) {
            return Err(self.fail(e));
        }
        Ok(())
    }

    /// `flushLocked`: the bytes reached the buffer, not `path`.
    fn flush_locked(&mut self, sync: bool) -> Result<(), Error> {
        self.flush_buffer()?;
        if sync {
            let Inner { path, f, .. } = self;
            let f = f.as_mut().ok_or(Error::Closed)?;
            f.sync_all().map_err(|e| Error::io("fsync", path, e))?;
        }
        self.removed()
    }

    /// Writes the pending appends through to the handle, leaving the buffer
    /// intact when the write fails, as `bufio.Writer` does.
    fn flush_buffer(&mut self) -> Result<(), Error> {
        let Inner { path, f, buf, .. } = self;
        if buf.is_empty() {
            return Ok(());
        }
        let f = f.as_mut().ok_or(Error::Closed)?;
        if let Err(e) = f.write_all(buf) {
            return Err(Error::io("flush", path, e));
        }
        buf.clear();
        Ok(())
    }

    /// `removedLocked`: reports [`Error::Removed`] when the store file is no
    /// longer at the store's path, because the bytes just flushed went somewhere
    /// no reader can reach and the caller must not be told they are durable.
    fn removed(&self) -> Result<(), Error> {
        match std::fs::metadata(&self.path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::Removed(self.path.clone())),
            Err(e) => Err(Error::io("stat", &self.path, e)),
        }
    }

    /// Records the first append/flush failure; returns the error to report now
    /// (the raw one first, then the sticky wrapper, exactly like the Go
    /// original's `setFailLocked` / `failedLocked`).
    fn fail(&mut self, err: Error) -> Error {
        match &self.fail_err {
            Some(sticky) => Error::EarlierAppendFailed(Arc::clone(sticky)),
            None => {
                self.fail_err = Some(Arc::new(err.clone()));
                err
            }
        }
    }

    fn failed(&self) -> Error {
        match &self.fail_err {
            Some(sticky) => Error::EarlierAppendFailed(Arc::clone(sticky)),
            None => Error::Closed, // unreachable: callers check `is_some` first
        }
    }

    /// `appendDeleteLocked`: builds the tombstone payload.
    fn append_delete(&mut self, key: &[u8], threshold: i64) -> Result<(), Error> {
        {
            let Inner { payload_buf, .. } = &mut *self;
            payload_buf.clear();
            append_uvarint(payload_buf, key.len() as u64);
            payload_buf.extend_from_slice(key);
        }
        self.append_frame(TAG_DELETE, threshold)
    }

    /// `appendFrameLocked`: writes one frame through the buffer, then compacts
    /// if the delta budget is now exhausted.
    fn append_frame(&mut self, tag: u8, threshold: i64) -> Result<(), Error> {
        let frame_len = {
            let Inner {
                frame_buf,
                payload_buf,
                ..
            } = &mut *self;
            frame_buf.clear();
            frame_buf.push(tag);
            frame_buf.extend_from_slice(&(payload_buf.len() as u32).to_le_bytes());
            frame_buf.extend_from_slice(&crc32(payload_buf).to_le_bytes());
            frame_buf.extend_from_slice(payload_buf);
            frame_buf.len()
        };
        // A full buffer flushes itself, exactly where `bufio.Writer` does: an
        // OS error here is the frame's error, before any accounting.
        let full = {
            let Inner { buf, frame_buf, .. } = &mut *self;
            buf.extend_from_slice(frame_buf);
            buf.len() >= APPEND_BUF_SIZE
        };
        if full {
            self.flush_buffer()?;
        }
        self.delta_bytes += frame_len as i64;
        if self.delta_bytes >= threshold {
            return self.compact();
        }
        Ok(())
    }

    /// `compactLocked`: rewrites the whole file from the live map.
    fn compact(&mut self) -> Result<(), Error> {
        let path = self.path.clone();
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (mut tmp, tmp_path) = match create_temp(&dir, &base) {
            Ok(pair) => pair,
            Err(e) => return Err(Error::io("compact temp", &path, e)),
        };

        let written = {
            let mut out = io::BufWriter::with_capacity(APPEND_BUF_SIZE, &mut tmp);
            let result = (|| -> io::Result<i64> {
                let n = write_snapshot(&mut out, &self.records)?;
                out.flush()?;
                Ok(n)
            })();
            drop(out);
            match result {
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(Error::io("compact frame", &path, e));
                }
            }
        };
        if let Err(e) = tmp.sync_all() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(Error::io("compact sync", &path, e));
        }
        drop(tmp); // go of the temp handle: the rename needs it closed

        // Swap the file: get our pending bytes out, drop the handle, rename,
        // reopen. The temp file is removed on every failure path.
        if let Err(e) = self.flush_buffer() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(self.fail(e));
        }
        self.f = None;
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            // The swap failed: the file was closed, so the handle has to come
            // back before this can be reported as a compact failure.
            self.reopen()?;
            return Err(Error::io("compact rename", &path, e));
        }
        self.reopen()?;

        self.snapshot_bytes = written;
        self.delta_bytes = 0;
        self.compactions += 1;
        self.fail_err = None;
        Ok(())
    }

    /// `reopenLocked`: reopens the store's path for appending after the handle
    /// was dropped.
    fn reopen(&mut self) -> Result<(), Error> {
        let file = match sys::open_store_file(&self.path) {
            Ok(f) => f,
            Err(e) => {
                self.f = None;
                return Err(Error::io("reopen", &self.path, e));
            }
        };
        let mut file = file;
        if let Err(e) = file.seek(SeekFrom::End(0)) {
            self.f = None;
            return Err(Error::io("seek", &self.path, e));
        }
        self.f = Some(file);
        Ok(())
    }
}


fn encode_header(dst: &mut [u8; HEADER_SIZE]) {
    dst[0..8].copy_from_slice(&MAGIC);
    dst[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    dst[12..16].copy_from_slice(&0u32.to_le_bytes()); // flags
    let crc = crc32(&dst[0..16]);
    dst[16..20].copy_from_slice(&crc.to_le_bytes());
    dst[20..24].copy_from_slice(&0u32.to_le_bytes()); // reserved
}

fn validate_header(hdr: &[u8]) -> Result<(), Error> {
    if hdr[0..8] != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = u32::from_le_bytes(hdr[8..12].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let got = u32::from_le_bytes(hdr[16..20].try_into().expect("4 bytes"));
    let want = crc32(&hdr[0..16]);
    if got != want {
        return Err(Error::HeaderCrcMismatch { got, want });
    }
    Ok(())
}

/// LEB128 unsigned varint, byte-identical to `binary.AppendUvarint`.
fn append_uvarint(dst: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        dst.push((x as u8) | 0x80);
        x >>= 7;
    }
    dst.push(x as u8);
}

/// LEB128 unsigned varint decode, matching `binary.Uvarint` exactly: `None` for
/// a truncated buffer (Go's `n == 0`) and for an overflow (Go's `n < 0`), so a
/// corrupt frame ends replay here just as it does in Go.
fn uvarint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut x: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i == 10 {
            return None; // MaxVarintLen64: overflow
        }
        if b < 0x80 {
            if i == 9 && b > 1 {
                return None; // overflow
            }
            return Some((x | ((b as u64) << shift), i + 1));
        }
        x |= ((b & 0x7f) as u64) << shift;
        shift += 7;
    }
    None
}

/// `appendPutFrame`: appends one complete put frame to `dst`.
fn append_put_frame(dst: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    let mut payload = Vec::with_capacity(key.len() + value.len() + 12);
    append_uvarint(&mut payload, key.len() as u64);
    payload.extend_from_slice(key);
    append_uvarint(&mut payload, value.len() as u64);
    payload.extend_from_slice(value);

    dst.push(TAG_PUT);
    dst.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    dst.extend_from_slice(&crc32(&payload).to_le_bytes());
    dst.extend_from_slice(&payload);
}

/// Writes a fresh snapshot: the header, then one put frame per live key in
/// sorted key order (deterministic, so equal states produce equal files).
/// Returns the number of bytes written.
fn write_snapshot<W: Write>(out: &mut W, records: &Records) -> io::Result<i64> {
    let mut hdr = [0u8; HEADER_SIZE];
    encode_header(&mut hdr);
    out.write_all(&hdr)?;

    let mut keys: Vec<&[u8]> = records.keys().map(Vec::as_slice).collect();
    keys.sort_unstable(); // byte order, like `sort.Strings`
    let mut frame: Vec<u8> = Vec::new();
    let mut written = HEADER_SIZE as i64;
    for key in keys {
        frame.clear();
        append_put_frame(&mut frame, key, &records[key]);
        out.write_all(&frame)?;
        written += frame.len() as i64;
    }
    Ok(written)
}

/// Creates a fresh `<base>.compact-<pid>-<n>` temp file, the shape of
/// `os.CreateTemp(dir, base+".compact-*")`.
fn create_temp(dir: &Path, base: &str) -> io::Result<(File, PathBuf)> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let mut last = io::Error::new(io::ErrorKind::AlreadyExists, "no free temp name");
    for _ in 0..64 {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!("{base}.compact-{pid}-{n}"));
        match File::create_new(&candidate) {
            Ok(f) => return Ok((f, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}


/// Reads and validates the 24-byte header at offset 0, as `Open` does.
fn read_header(path: &Path, f: &mut File, hdr: &mut [u8; HEADER_SIZE]) -> Result<(), Error> {
    f.seek(SeekFrom::Start(0))
        .map_err(|e| Error::io("seek", path, e))?;
    f.read_exact(hdr)
        .map_err(|e| Error::io("read header", path, e))?;
    validate_header(hdr)
}

fn write_header(path: &Path, f: &mut File) -> Result<(), Error> {
    let mut hdr = [0u8; HEADER_SIZE];
    encode_header(&mut hdr);
    f.seek(SeekFrom::Start(0))
        .map_err(|e| Error::io("seek", path, e))?;
    f.write_all(&hdr)
        .map_err(|e| Error::io("write header", path, e))?;
    f.sync_all().map_err(|e| Error::io("sync header", path, e))
}

/// Replays frames from `r` into `records`, returning the offset of the last
/// valid frame boundary. Nothing in the frame loop is an error: the first frame
/// that does not read, decode or match its CRC simply ends replay, exactly as
/// the Go original's loop does.
fn replay_frames<R: Read>(
    r: &mut R,
    size: u64,
    records: &mut Records,
    payload_buf: &mut Vec<u8>,
) -> u64 {
    let mut off = HEADER_SIZE as u64;
    let mut valid = off;
    loop {
        let mut tag = [0u8; 1];
        if r.read_exact(&mut tag).is_err() {
            break; // clean end, or a frame header that was never finished
        }
        off += 1;
        if tag[0] != TAG_PUT && tag[0] != TAG_DELETE {
            break;
        }
        let mut scratch = [0u8; 8];
        if r.read_exact(&mut scratch).is_err() {
            break;
        }
        off += 8;
        let payload_len = u32::from_le_bytes(scratch[0..4].try_into().expect("4 bytes")) as u64;
        let want = u32::from_le_bytes(scratch[4..8].try_into().expect("4 bytes"));
        if payload_len > size - off {
            break; // the payload was never fully written
        }
        payload_buf.clear();
        payload_buf.resize(payload_len as usize, 0);
        if r.read_exact(payload_buf).is_err() {
            break;
        }
        off += payload_len;
        if crc32(payload_buf) != want {
            break;
        }
        if !apply_payload(tag[0], payload_buf, records) {
            break;
        }
        valid = off;
    }
    valid
}

/// `applyPayload`: decodes one frame payload and applies it to the map. Reports
/// false for a payload that does not decode exactly, which ends replay.
fn apply_payload(tag: u8, payload: &[u8], records: &mut Records) -> bool {
    let Some((klen, n)) = uvarint(payload) else {
        return false;
    };
    if klen > (payload.len() - n) as u64 {
        return false;
    }
    let key = &payload[n..n + klen as usize];
    let mut n = n + klen as usize;

    match tag {
        TAG_DELETE => {
            if n != payload.len() {
                return false;
            }
            records.remove(key);
        }
        TAG_PUT => {
            let Some((vlen, m)) = uvarint(&payload[n..]) else {
                return false;
            };
            if vlen > (payload.len() - (n + m)) as u64 {
                return false;
            }
            n += m;
            let value = &payload[n..n + vlen as usize];
            n += vlen as usize;
            if n != payload.len() {
                return false;
            }
            records.insert(key.to_vec(), Arc::from(value));
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests;
