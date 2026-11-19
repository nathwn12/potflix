
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::OnceLock;

/// The bind address: loopback only (`go\instance.go:16`).
pub const LOCK_HOST: &str = "127.0.0.1";

/// The lock port (`go\instance.go:17`).
pub const LOCK_PORT: u16 = 17843;

/// The backlog the Go port asks for (`go\instance.go:18`); see the deviation
/// note above for why std cannot pass it through.
pub const LOCK_BACKLOG: u32 = 10;

/// The second-instance line, byte-exact (`go\main.go:36`).
pub const ALREADY_RUNNING_MSG: &str = "Potflix is already running.";

/// The second-instance exit code (`go\main.go:115`: `os.Exit(0)`).
pub const ALREADY_RUNNING_EXIT_CODE: i32 = 0;

/// The held listening socket, for process life. The lock is released only when
/// the process dies.
static INSTANCE_LOCK: OnceLock<TcpListener> = OnceLock::new();

/// Binds `127.0.0.1:port` and returns the held listening handle. Port 0 asks the
/// OS for an ephemeral port (test use only -- production always passes
/// [`LOCK_PORT`]); the caller discovers it with `local_addr` and must keep the
/// handle open while it owns the lock. A bind failure means another holder owns
/// the port.
///
/// Tests MUST use port 0 (as the Go tests do) so a live Potflix is neither
/// touched nor disturbed.
pub fn acquire_instance_lock_on(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

/// Binds `127.0.0.1:17843` and keeps the socket open for the process lifetime.
/// On failure it returns the bind error: the caller prints
/// [`ALREADY_RUNNING_MSG`] and exits [`ALREADY_RUNNING_EXIT_CODE`].
///
/// Never call this from a test: it binds the live lock port.
pub fn acquire_instance_lock() -> io::Result<()> {
    let listener = acquire_instance_lock_on(LOCK_PORT)?;
    INSTANCE_LOCK
        .set(listener)
        .map_err(|_| io::Error::other("the single-instance lock is already held by this process"))
}

/// The second-instance path, exactly as `go\main.go:113-116`: the line on
/// stderr, then exit 0. Console-less builds deliver the line to whatever handle
/// the parent supplied, or nowhere -- as Go's `Fprintln(os.Stderr, ...)` does.
pub fn exit_already_running() -> ! {
    eprintln!("{ALREADY_RUNNING_MSG}");
    std::process::exit(ALREADY_RUNNING_EXIT_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, mpsc};
    use std::thread;

    /// Stages a held fake lock on an ephemeral loopback port, redrawing if the
    /// OS ever deals the live lock port number. The live port 17843 is never
    /// bound by a test.
    fn acquire_fake_lock() -> TcpListener {
        for _ in 0..10 {
            let listener = acquire_instance_lock_on(0).expect("fake lock acquire");
            if listener.local_addr().expect("local_addr").port() != LOCK_PORT {
                return listener;
            }
        }
        panic!("ephemeral bind kept landing on the live port {LOCK_PORT}");
    }

    fn port_of(listener: &TcpListener) -> u16 {
        let port = listener.local_addr().expect("local_addr").port();
        assert_ne!(port, 0, "ephemeral bind returned port 0");
        assert_ne!(
            port, LOCK_PORT,
            "a test must never stage the live lock port"
        );
        port
    }

    #[test]
    fn second_instance_fails_while_held() {
        let holder = acquire_fake_lock();
        let port = port_of(&holder);

        assert!(
            acquire_instance_lock_on(port).is_err(),
            "second bind to held 127.0.0.1:{port} succeeded, want failure"
        );
        assert!(
            acquire_instance_lock_on(port).is_err(),
            "third bind to held 127.0.0.1:{port} succeeded, holder lost the lock"
        );
        assert_eq!(port_of(&holder), port, "the holder kept the port");
    }

    #[test]
    fn spam_open_second_instances() {
        const BURST: usize = 32;

        let holder = acquire_fake_lock();
        let port = port_of(&holder);

        let results = burst(BURST, port);
        assert_eq!(results.len(), BURST, "every contender must have run");
        for won in results {
            assert!(!won, "a spam contender bound held 127.0.0.1:{port}");
        }
        assert!(
            acquire_instance_lock_on(port).is_err(),
            "holder lost 127.0.0.1:{port} under the spam burst"
        );
        drop(holder);

        for _ in 0..3 {
            let stage = acquire_fake_lock();
            let race_port = port_of(&stage);
            drop(stage);

            let wins = burst(BURST, race_port)
                .into_iter()
                .filter(|won| *won)
                .count();
            if wins == 1 {
                return;
            }
            assert_eq!(
                wins, 0,
                "spam race for free 127.0.0.1:{race_port} produced {wins} winners, want exactly 1"
            );
        }
        panic!("spam race never produced exactly one winner in 3 rounds");
    }

    /// Fires `burst` threads at `port` from one barrier and returns each
    /// contender's outcome. Every socket stays open until all outcomes are
    /// collected, so a winner cannot free the port for a later contender -- the
    /// same reason the Go test keeps the winners' fds alive until the whole
    /// burst has reported. (Collecting a fixed count, not draining the channel:
    /// the senders are the worker threads, and the workers must not be waited on
    /// by a blocked receiver.)
    fn burst(burst: usize, port: u16) -> Vec<bool> {
        let start = std::sync::Arc::new(Barrier::new(burst));
        let (tx, rx) = mpsc::channel();
        for _ in 0..burst {
            let tx = tx.clone();
            let start = std::sync::Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let _ = tx.send(acquire_instance_lock_on(port).ok());
            });
        }
        drop(tx);
        let held: Vec<Option<TcpListener>> = rx.iter().take(burst).collect();
        let outcomes: Vec<bool> = held.iter().map(Option::is_some).collect();
        drop(held);
        outcomes
    }

    #[test]
    fn already_running_message_is_exact() {
        assert_eq!(ALREADY_RUNNING_MSG, "Potflix is already running.");
        assert_eq!(ALREADY_RUNNING_EXIT_CODE, 0);
        assert_eq!(LOCK_HOST, "127.0.0.1");
        assert_eq!(LOCK_PORT, 17843);
        assert_eq!(LOCK_BACKLOG, 10);
    }

    #[test]
    fn lock_is_loopback_only() {
        let listener = acquire_fake_lock();
        let addr = listener.local_addr().expect("local_addr");
        assert!(addr.ip().is_loopback(), "lock must bind loopback only");
    }
}
