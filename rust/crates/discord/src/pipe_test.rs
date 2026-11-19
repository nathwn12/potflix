
use std::io;
use std::ops::Deref;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::activity::Activity;
use crate::fakepipe::{FakeConn, FakeServer};
use crate::{
    BACKOFF_MAX, Client, Config, Error, READY_GRACE, Reply, RetryEvent, OP_FRAME, OP_HANDSHAKE,
    OP_PING, OP_PONG, backoff_delay,
};

/// Discord's READY dispatch: the client refuses to send commands before it.
pub(crate) const READY: &[u8] = br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#;

/// Closes the client when the test ends (Go's `t.Cleanup`).
pub(crate) struct ClientGuard(pub(crate) Client);

impl Deref for ClientGuard {
    type Target = Client;

    fn deref(&self) -> &Client {
        &self.0
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

/// Accepts one connection on a fresh pipe instance, off the test thread.
pub(crate) fn spawn_accept(
    srv: Arc<FakeServer>,
) -> (mpsc::Receiver<FakeConn>, mpsc::Receiver<io::Error>) {
    let (tx, rx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    drop(thread::spawn(move || match srv.accept() {
        Ok(sc) => {
            let _ = tx.send(sc);
        }
        Err(e) => {
            let _ = etx.send(e);
        }
    }));
    (rx, erx)
}

/// Waits for an accepted connection (Go's `select` on accept / acceptErr / 5s).
pub(crate) fn accept_one(
    rx: &mpsc::Receiver<FakeConn>,
    erx: &mpsc::Receiver<io::Error>,
    what: &str,
) -> FakeConn {
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(sc) => sc,
        Err(_) => match erx.try_recv() {
            Ok(e) => panic!("fake server accept: {e}"),
            Err(_) => panic!("{what}"),
        },
    }
}

/// connectPair: starts a FakeServer on `suffix` and a Client pointed at it, waits
/// for the connection, consumes and returns the client's handshake frame.
pub(crate) fn connect_pair(suffix: i32, mut cfg: Config) -> (ClientGuard, FakeConn, Vec<u8>) {
    let srv = FakeServer::new(suffix);
    let base = srv.base.clone();
    let (rx, erx) = spawn_accept(Arc::new(srv));

    if cfg.client_id.is_empty() {
        cfg.client_id = "123456789012345678".to_string();
    }
    if cfg.pipe_base.is_empty() {
        cfg.pipe_base = base;
    }
    if cfg.retry_delay.is_zero() {
        cfg.retry_delay = Duration::from_millis(30);
    }
    let c = ClientGuard(Client::new(cfg));
    c.start();

    let sc = accept_one(&rx, &erx, "client did not connect to the fake pipe");

    let (op, hs) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read handshake: {e}"));
    assert_eq!(op, OP_HANDSHAKE, "first frame opcode");
    sc.write_frame(OP_FRAME, READY)
        .unwrap_or_else(|e| panic!("write READY: {e}"));
    wait_ready(&c, 1);
    (c, sc, hs)
}

/// Blocks until the client's generation reaches `want`.
pub(crate) fn wait_ready(c: &Client, want: i64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while c.generation() < want {
        if Instant::now() > deadline {
            panic!(
                "client never became ready (gen={}, want >={want})",
                c.generation()
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// Decodes the nonce from a client frame payload.
pub(crate) fn nonce_of(payload: &[u8]) -> String {
    let m: Value = serde_json::from_slice(payload)
        .unwrap_or_else(|e| panic!("decode frame {}: {e}", String::from_utf8_lossy(payload)));
    let nonce = m.get("nonce").and_then(Value::as_str).unwrap_or("");
    if nonce.is_empty() {
        panic!(
            "frame carries no nonce: {}",
            String::from_utf8_lossy(payload)
        );
    }
    nonce.to_string()
}

/// Waits for the next retry delay the supervisor asked for (skipping the
/// `Woke` events), bounded so a broken supervisor fails instead of hanging.
fn next_wait(rx: &mpsc::Receiver<RetryEvent>, what: &str) -> Duration {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(RetryEvent::Wait(d)) => return d,
            Ok(RetryEvent::Woke) => {}
            Err(_) => panic!("no retry wait observed for {what}"),
        }
    }
}

#[test]
fn test_handshake_payload() {
    const ID: &str = "987654321098765432";
    // server on suffix 3: 0..2 must fail first
    let (_c, sc, hs) = connect_pair(
        3,
        Config {
            client_id: ID.to_string(),
            ..Config::default()
        },
    );

    let want = r#"{"v":1,"client_id":"987654321098765432"}"#;
    assert_eq!(String::from_utf8_lossy(&hs).as_ref(), want, "handshake");
    sc.close();
}

#[test]
fn test_ping_pong() {
    let (_c, sc, _) = connect_pair(0, Config::default());

    sc.write_frame(OP_PING, b"ping-payload")
        .unwrap_or_else(|e| panic!("write ping: {e}"));
    let (op, payload) = sc.read_frame().unwrap_or_else(|e| panic!("read pong: {e}"));
    assert_eq!(op, OP_PONG, "reply opcode");
    assert_eq!(payload, b"ping-payload", "pong payload");
    sc.close();
}

#[test]
fn test_reconnect_after_drop() {
    let srv = FakeServer::new(0);
    let base = srv.base.clone();
    let (tx, rx) = mpsc::channel();
    drop(thread::spawn(move || {
        for _ in 0..2 {
            let Ok(sc) = srv.accept() else { return };
            let Ok((_, p)) = sc.read_frame() else {
                sc.close();
                return;
            };
            if tx.send(p).is_err() {
                sc.close();
                return;
            }
            sc.close(); // drop so the client must reconnect
        }
    }));

    let (dtx, drx) = mpsc::channel();
    let c = ClientGuard(Client::new(Config {
        client_id: "424242424242424242".to_string(),
        pipe_base: base,
        retry_delay: Duration::from_millis(40),
        on_drop: Some(Box::new(move |_e: &Error| {
            let _ = dtx.send(());
        })),
        ..Config::default()
    }));
    c.start();

    const WANT: &str = r#"{"v":1,"client_id":"424242424242424242"}"#;
    for i in 0..2 {
        let hs = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("timed out waiting for handshake {i} (reconnect failed)"));
        assert_eq!(String::from_utf8_lossy(&hs).as_ref(), WANT, "handshake {i}");
    }
    drx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("OnDrop was not called after the server closed the pipe"));
}

/// The backoff curve, pinned without a supervisor: the first retry waits exactly
/// the base delay, every further consecutive unhealthy session doubles it, and
/// the wait saturates at the cap. A very large counter must clamp, not panic.
#[test]
fn test_backoff_delay_curve() {
    let base = Duration::from_millis(100);
    assert_eq!(
        backoff_delay(base, 0),
        base,
        "the first retry waits exactly the base delay"
    );
    for n in 1..=12u32 {
        let prev = backoff_delay(base, n - 1);
        let want = (prev * 2).min(BACKOFF_MAX);
        assert_eq!(backoff_delay(base, n), want, "delay {n}");
        assert!(
            backoff_delay(base, n) >= prev,
            "delay {n} must never shrink the wait"
        );
    }
    assert_eq!(backoff_delay(base, 10), BACKOFF_MAX, "saturates at the cap");
    assert_eq!(
        backoff_delay(base, u32::MAX),
        BACKOFF_MAX,
        "a very large counter clamps at the cap and does not overflow or panic"
    );
    // A configured base larger than the cap is never sped up.
    let slow = Duration::from_secs(120);
    assert_eq!(
        backoff_delay(slow, u32::MAX),
        slow,
        "a base above the cap stays at the base"
    );
}

/// Proves the reconnect storm is damped: a peer that accepts the connection and
/// closes it before READY is an unhealthy session, so the supervisor asks for
/// the base delay before the first retry and twice that before the retry after
/// the second consecutive failure. Observed through the test-only retry probe,
/// so no test sleeps the real backoff.
#[test]
fn test_consecutive_unhealthy_sessions_back_off() {
    let srv = FakeServer::new(0);
    let base = srv.base.clone();
    drop(thread::spawn(move || {
        for _ in 0..3 {
            let Ok(sc) = srv.accept() else { return };
            // Consume the handshake, then drop the connection without READY.
            if sc.read_frame().is_err() {
                sc.close();
                return;
            }
            sc.close();
        }
    }));

    let (ptx, prx) = mpsc::channel::<RetryEvent>();
    let (dtx, drx) = mpsc::channel::<()>();
    let retry = Duration::from_millis(50);
    let mut client = Client::new(Config {
        client_id: "123456789012345678".to_string(),
        pipe_base: base,
        retry_delay: retry,
        on_drop: Some(Box::new(move |_e: &Error| {
            let _ = dtx.send(());
        })),
        ..Config::default()
    });
    client.retry_probe = Some(ptx);
    let c = ClientGuard(client);
    c.start();

    assert_eq!(
        next_wait(&prx, "the first retry"),
        retry,
        "the first retry after a drop must keep the configured delay"
    );
    assert_eq!(
        next_wait(&prx, "the retry after the second failure"),
        retry * 2,
        "the second consecutive failure must be damped"
    );

    // The damped wait followed real reconnects: the drop callback fired for both
    // dead sessions, and the second accept above proves the first retry dialed.
    for i in 0..2 {
        drx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("drop {i} was never reported"));
    }
}

/// Proves a healthy session resets the backoff: an unhealthy session raises the
/// counter, a session that reached READY and stayed up past the grace resets it,
/// and the next two consecutive unhealthy sessions back off from the plain retry
/// delay again (base, then base * 2). A broken reset would keep doubling from the
/// raised counter (base * 2, base * 4, ...), which this sequence can never show.
/// The grace is shrunk via the test seam, so the classification needs no real
/// 45 s wait.
#[test]
fn test_healthy_session_resets_backoff() {
    let srv = FakeServer::new(0);
    let base = srv.base.clone();
    drop(thread::spawn(move || {
        for session in 0..4 {
            let Ok(sc) = srv.accept() else { return };
            if sc.read_frame().is_err() {
                sc.close();
                return;
            }
            // Session 1 and 3/4: never READY -- unhealthy. Session 2: READY, alive
            // well past the (shrunk) grace, then dropped -- healthy.
            if session == 1 {
                if sc.write_frame(OP_FRAME, READY).is_err() {
                    sc.close();
                    return;
                }
                thread::sleep(Duration::from_millis(300));
            }
            sc.close();
        }
    }));

    let (ptx, prx) = mpsc::channel::<RetryEvent>();
    let retry = Duration::from_millis(50);
    let mut client = Client::new(Config {
        client_id: "123456789012345678".to_string(),
        pipe_base: base,
        retry_delay: retry,
        ..Config::default()
    });
    // Test seam: 300 ms alive classifies healthy against a 50 ms grace, where the
    // production grace (`READY_GRACE`, 45 s) would need a real 45 s wait.
    client.ready_grace = Duration::from_millis(50);
    client.retry_probe = Some(ptx);
    let c = ClientGuard(client);
    c.start();

    // The client can lose the race against the server's first pipe instance, and
    // that leading dial failure is an extra unhealthy session with its own wait
    // before the session under test. So wait for the window instead: `base, base`
    // in a row can only follow a reset (an unhealthy session increments, so it
    // can never produce two equal waits), and the closing `base * 2` is the
    // second consecutive unhealthy session re-damping from the base.
    let want = [retry, retry, retry * 2];
    let mut seen: Vec<Duration> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.ends_with(&want) {
        assert!(
            Instant::now() < deadline,
            "the healthy-reset backoff sequence never appeared; saw {seen:?}, want {want:?}"
        );
        seen.push(next_wait(&prx, "the healthy-reset backoff sequence"));
    }
}

/// The sustained worst case stays inside the ~2/min guidance: a peer dying
/// just past `READY_GRACE` resets the backoff every cycle, so the cycle is the
/// default retry delay plus the grace. The grace lives in the 30-60 s band and
/// that cycle reconnects at most twice a minute.
#[test]
fn test_ready_grace_bounds_worst_case_rate() {
    assert!(
        (Duration::from_secs(30)..=Duration::from_secs(60)).contains(&READY_GRACE),
        "READY_GRACE = {READY_GRACE:?}, want the 30-60 s band"
    );
    let cycle = Duration::from_secs(5) + READY_GRACE;
    let per_min = 60.0 / cycle.as_secs_f64();
    assert!(
        per_min <= 2.0,
        "worst case {per_min:.2}/min over a {cycle:?} cycle, want <= 2/min"
    );
}

/// Proves a shutdown never waits out the backoff: `close()` returns at once and
/// the supervisor's long retry wait ends immediately -- observed as the probe's
/// `Woke` event within two seconds of a wait that would otherwise last 30 s.
#[test]
fn test_close_wakes_a_long_backoff() {
    // A server namespace with no pipe instance: every dial fails, so the first
    // retry is the long backoff.
    let srv = FakeServer::new(0);
    let base = srv.base.clone();

    let (ptx, prx) = mpsc::channel::<RetryEvent>();
    let retry = Duration::from_secs(30);
    let mut client = Client::new(Config {
        client_id: "123456789012345678".to_string(),
        pipe_base: base,
        retry_delay: retry,
        ..Config::default()
    });
    client.retry_probe = Some(ptx);
    let c = ClientGuard(client);
    c.start();

    assert_eq!(
        next_wait(&prx, "the retry after the failed dial"),
        retry,
        "the first retry after a drop must keep the configured delay"
    );
    let start = Instant::now();
    c.close().unwrap_or_else(|e| panic!("close: {e}"));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "close blocked for {:?}",
        start.elapsed()
    );
    match prx.recv_timeout(Duration::from_secs(2)) {
        Ok(RetryEvent::Woke) => {}
        Ok(RetryEvent::Wait(d)) => {
            panic!("supervisor started another {d:?} wait after close")
        }
        Err(_) => panic!("close did not wake the long retry wait (still in backoff)"),
    }
}

/// Proves the reviewer's MEDIUM finding is closed: a peer that accepts the
/// connection, completes READY, then stops draining must not stall the client.
/// The send returns once the write deadline expires, the client drops the
/// truncated connection and reconnects, and a later push is served on the fresh
/// connection.
#[test]
fn test_stalled_peer_write_is_bounded() {
    let srv = Arc::new(FakeServer {
        buffer_bytes: 256, // small enough that a normal SET_ACTIVITY blocks
        ..FakeServer::new(0)
    });
    let base = srv.base.clone();
    let (rx, erx) = spawn_accept(Arc::clone(&srv));

    let (rtx, rrx) = mpsc::channel();
    let mut client = Client::new(Config {
        client_id: "123456789012345678".to_string(),
        pipe_base: base,
        retry_delay: Duration::from_millis(30),
        on_reply: Some(Box::new(move |r: Reply| {
            let _ = rtx.send(r);
        })),
        ..Config::default()
    });
    // test-only; the production default is ACK_TIMEOUT
    client.write_timeout = Duration::from_millis(200);
    let c = Arc::new(ClientGuard(client));
    c.start();

    let sc = accept_one(&rx, &erx, "client did not connect to the fake pipe");
    let (op, _) = sc.read_frame().unwrap_or_else(|e| panic!("handshake: {e}"));
    assert_eq!(op, OP_HANDSHAKE, "handshake");
    sc.write_frame(OP_FRAME, READY)
        .unwrap_or_else(|e| panic!("write READY: {e}"));
    wait_ready(&c, 1);

    // The server now stops reading. A payload larger than the pipe buffer blocks
    // in the write unless the deadline bounds it.
    let (stx, srx) = mpsc::channel();
    let start = Instant::now();
    {
        let c = Arc::clone(&c);
        drop(thread::spawn(move || {
            let _ = stx.send(c.set_activity(Activity {
                details: "x".repeat(4096),
                state: "S1 E1".to_string(),
                ..Activity::default()
            }));
        }));
    }
    match srx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => panic!("SetActivity to a stalled peer returned Ok, want a write error"),
        Ok(Err(_)) => {
            let elapsed = start.elapsed();
            assert!(
                elapsed <= Duration::from_secs(3),
                "SetActivity stalled {elapsed:?}; the write is not bounded"
            );
        }
        Err(_) => panic!("SetActivity never returned; the write is not bounded"),
    }

    let (rx2, erx2) = spawn_accept(Arc::clone(&srv));
    let sc2 = accept_one(
        &rx2,
        &erx2,
        "client did not reconnect after the bounded write",
    );
    let (op, _) = sc2
        .read_frame()
        .unwrap_or_else(|e| panic!("reconnect handshake: {e}"));
    assert_eq!(op, OP_HANDSHAKE, "reconnect handshake");
    sc2.write_frame(OP_FRAME, READY)
        .unwrap_or_else(|e| panic!("write READY (reconnect): {e}"));
    wait_ready(&c, 2);

    c.set_activity(Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        ..Activity::default()
    })
    .unwrap_or_else(|e| panic!("later push after reconnect: {e}"));
    let (op, frame) = sc2
        .read_frame()
        .unwrap_or_else(|e| panic!("read later push: {e}"));
    assert_eq!(op, OP_FRAME, "later push opcode");
    let want = nonce_of(&frame);
    sc2.write_frame(
        OP_FRAME,
        format!(r#"{{"cmd":"SET_ACTIVITY","nonce":"{want}"}}"#).as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write later ack: {e}"));
    match rrx.recv_timeout(Duration::from_secs(3)) {
        Ok(r) => assert_eq!(r.nonce, want, "later push ack nonce"),
        Err(_) => panic!("later push was not served after the bounded write"),
    }
    sc.close();
    sc2.close();
}

/// Proves the unproven-session fix: a peer that accepts a command and NEVER
/// answers it must not leave the client limping on that connection. The ack
/// timeout reports the nonce once, drops the connection, and the supervisor
/// reconnects with a fresh handshake; a later command is delivered on the new
/// session instead of being swallowed by the dead one.
#[test]
fn test_ack_timeout_drops_connection_and_reconnects() {
    let srv = Arc::new(FakeServer::new(0));
    let base = srv.base.clone();
    let (rx, erx) = spawn_accept(Arc::clone(&srv));

    let (atx, arx) = mpsc::channel::<String>();
    let c = ClientGuard(Client::new(Config {
        client_id: "123456789012345678".to_string(),
        pipe_base: base,
        retry_delay: Duration::from_millis(30),
        on_ack_timeout: Some(Box::new(move |nonce: &str| {
            let _ = atx.send(nonce.to_string());
        })),
        ..Config::default()
    }));
    c.start();

    let sc = accept_one(&rx, &erx, "client did not connect to the fake pipe");
    let (op, _) = sc.read_frame().unwrap_or_else(|e| panic!("handshake: {e}"));
    assert_eq!(op, OP_HANDSHAKE, "handshake");
    sc.write_frame(OP_FRAME, READY)
        .unwrap_or_else(|e| panic!("write READY: {e}"));
    wait_ready(&c, 1);

    // The peer reads the command and never answers it.
    c.clear_activity()
        .unwrap_or_else(|e| panic!("clear_activity: {e}"));
    let (op, frame) = sc.read_frame().unwrap_or_else(|e| panic!("read the clear: {e}"));
    assert_eq!(op, OP_FRAME, "clear opcode");
    let unacked = nonce_of(&frame);

    // The ack timeout must drop that connection; accept the reconnect and
    // complete its handshake, which is what bumps the generation.
    let (rx2, erx2) = spawn_accept(Arc::clone(&srv));
    let sc2 = accept_one(
        &rx2,
        &erx2,
        "client did not reconnect after the unanswered command",
    );
    let (op, _) = sc2
        .read_frame()
        .unwrap_or_else(|e| panic!("reconnect handshake: {e}"));
    assert_eq!(op, OP_HANDSHAKE, "reconnect handshake");
    sc2.write_frame(OP_FRAME, READY)
        .unwrap_or_else(|e| panic!("write READY (reconnect): {e}"));
    wait_ready(&c, 2);

    // The callback fired for the unanswered command, by its own nonce (the
    // removal in `ack_expired` is what keeps it to one report per command).
    match arx.recv_timeout(Duration::from_secs(3)) {
        Ok(nonce) => assert_eq!(nonce, unacked, "reported nonce"),
        Err(_) => panic!("on_ack_timeout did not fire for the unanswered command"),
    }

    // A later command is delivered on the fresh session, not on the dead one.
    c.clear_activity()
        .unwrap_or_else(|e| panic!("later clear_activity: {e}"));
    let (op, frame) = sc2
        .read_frame()
        .unwrap_or_else(|e| panic!("read the later clear: {e}"));
    assert_eq!(op, OP_FRAME, "later command opcode");
    assert_ne!(nonce_of(&frame), unacked, "a fresh command, not a replay");

    sc.close();
    sc2.close();
}

/// Proves the reviewer's LOW finding is closed: two sends that overlap
/// (clear-on-exit racing the poll loop) each keep their own verdict; the earlier
/// one is not silently overwritten by the later.
#[test]
fn test_overlapping_sends_both_accounted() {
    let (rtx, rrx) = mpsc::channel();
    let (c, sc, _) = connect_pair(
        0,
        Config {
            on_reply: Some(Box::new(move |r: Reply| {
                let _ = rtx.send(r.nonce);
            })),
            ..Config::default()
        },
    );
    let c = Arc::new(c);

    let (dtx, drx) = mpsc::channel();
    for _ in 0..2 {
        let c = Arc::clone(&c);
        let dtx = dtx.clone();
        drop(thread::spawn(move || {
            let r = c.set_activity(Activity {
                details: "Show".to_string(),
                state: "S1 E2".to_string(),
                ..Activity::default()
            });
            let _ = dtx.send(r);
        }));
    }
    drop(dtx);

    // Read both frames: both sends are now in flight (both begin_pending calls
    // happened before either reply can be written).
    let mut nonces = Vec::new();
    for i in 0..2 {
        let (op, frame) = sc
            .read_frame()
            .unwrap_or_else(|e| panic!("read frame {i}: {e}"));
        assert_eq!(op, OP_FRAME, "frame {i}");
        nonces.push(nonce_of(&frame));
    }
    for r in drx.iter() {
        r.unwrap_or_else(|e| panic!("SetActivity: {e}"));
    }

    for n in &nonces {
        sc.write_frame(
            OP_FRAME,
            format!(r#"{{"cmd":"SET_ACTIVITY","nonce":"{n}"}}"#).as_bytes(),
        )
        .unwrap_or_else(|e| panic!("reply nonce {n}: {e}"));
    }

    let mut got = Vec::new();
    for i in 0..2 {
        match rrx.recv_timeout(Duration::from_secs(3)) {
            Ok(n) => got.push(n),
            Err(_) => panic!("only {i} of 2 verdicts accounted for; got {got:?} want {nonces:?}"),
        }
    }
    for n in &nonces {
        assert!(
            got.contains(n),
            "verdict for nonce {n} was dropped; accounted: {got:?}"
        );
    }
    sc.close();
}
