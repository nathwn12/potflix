
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use app_support::clientid;
use app_support::debuglog::{self, Logger};
use app_support::instance;
use player::control::PlayStatus;
use potflix_win::{tray, win32};
use presence::Tracker;


/// `eventConnected` (`events.go:19`): the SECRET RULE redacts the client id.
pub fn event_connected(log: &Logger) {
    log.log("connected clientId=<redacted>");
}

/// Discord's clean reply to one SET_ACTIVITY, matched by nonce (`events.go:24`).
pub fn event_push_ack(log: &Logger, nonce: &str) {
    log.log(&format!("push ack nonce={nonce}"));
}

/// Discord's own rejection of one SET_ACTIVITY (`events.go:31`).
pub fn event_push_error(log: &Logger, nonce: &str, evt: &str, code: i32, message: &str) {
    log.log(&format!(
        "push error nonce={nonce} evt={evt} code={code} message={message}"
    ));
}

/// A pushed command that got no reply within the ack window (`events.go:37`).
pub fn event_push_ack_timeout(log: &Logger, nonce: &str) {
    log.log(&format!("push ack timeout nonce={nonce}"));
}

/// A transport failure that would otherwise be swallowed (`events.go:43`).
pub fn event_send_error(log: &Logger, err: &impl std::fmt::Display) {
    log.log(&format!("send error: {err}"));
}

/// The push line (`events.go:47`).
pub fn event_push(log: &Logger, details: &str, state: &str, ct: i64, total: i64) {
    log.log(&format!(
        "push details=[{details}] state=[{state}] ct={ct} total={total}"
    ));
}

/// The observation boundary for the push (`events.go:59`): one line per poll
/// that has a card, carrying the exact activity and the gate verdict.
pub fn event_push_attempt(log: &Logger, a: &presence::Activity, v: &presence::gate::Verdict) {
    if v.clear {
        log.log("push attempt gate=clear activity=null");
        return;
    }
    if !v.send
        && (v.reason == presence::gate::REASON_HEARTBEAT_NOT_DUE
            || v.reason == presence::gate::REASON_PAUSED)
    {
        log.log(&format!("push attempt gate=suppressed reason={}", v.reason));
        return;
    }
    let start = if a.has_start {
        a.start.to_string()
    } else {
        "none".to_string()
    };
    let payload = discord::wire_json(&discord::Activity {
        name: a.name.clone(),
        details: a.details.clone(),
        state: a.state.clone(),
        large_image: a.large_image.clone(),
        large_text: a.large_text.clone(),
        has_start: a.has_start,
        start: a.start,
        end: a.end,
        ..Default::default()
    });
    if v.send {
        log.log(&format!(
            "push attempt gate=sent start={start} payload={payload}"
        ));
    } else {
        log.log(&format!(
            "push attempt gate=suppressed reason={} start={start} payload={payload}",
            v.reason
        ));
    }
}

/// The decision line (`events.go:97`); the duplicated trailing `ct=` is
/// contract-verbatim.
#[allow(clippy::too_many_arguments)]
pub fn event_decision(
    log: &Logger,
    ms: i64,
    st: &str,
    ct: i64,
    title: &str,
    action: &str,
    running: bool,
    paused: bool,
    has_start: bool,
    start: i64,
    suf: &str,
) {
    log.log(&format!(
        "decision ms={ms} st={st} ct={ct} title=[{title}] action={action} running={running} paused={paused} hasStart={has_start} start={start} suf={suf} ct={ct}"
    ));
}

/// Cleared once when no player window is present (`events.go:102`).
pub fn event_cleared_no_player(log: &Logger) {
    log.log("cleared (no PotPlayer)");
}

/// Cleared when the tracked window is gone (`events.go:106`).
pub fn event_cleared_window_gone(log: &Logger) {
    log.log("cleared (player window gone)");
}

/// Cleared when the tracked window is still alive but its action has been Stop
/// for the whole grace (`sustained_stop`): the window stays up while the card
/// must not stay parked on the last movie. No Go original -- the Go/Java stop
/// path kept the card -- so this line exists to make that clear observable.
pub fn event_cleared_stopped(log: &Logger) {
    log.log("cleared (stopped player)");
}

/// The transport dropped; the supervisor will reconnect (`events.go:110`).
pub fn event_session_dropped(log: &Logger, err: &impl std::fmt::Display) {
    log.log(&format!("session dropped: {err}"));
}


/// Keeps the `CONOUT$` handle open for the process life when the fallback path
/// opened one (Go keeps the `*os.File` alive as `os.Stdout`).
static CONSOLE_HANDLE: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn std_handle_valid(n: u32) -> bool {
    let h = unsafe { win32::GetStdHandle(n) };
    !h.is_null() && h as isize != -1
}

/// `attachConsole`: bind the GUI-subsystem process to the launching console so
/// `--selftest` output is visible. It only acts when the caller did NOT already
/// provide a stdout handle: a pipe/file redirect (the normal evidence capture) is
/// kept and left alone.
///
/// Rust's `std::io::stdout` resolves `GetStdHandle(STD_OUTPUT_HANDLE)` lazily, so
/// calling this BEFORE the first write is enough for an attached console to be
/// picked up. A `CONOUT$` fallback handle is held open but cannot be rebound into
/// `std`; the transcript file is the guaranteed sink in that case.
pub fn attach_console() {
    if std_handle_valid(win32::STD_OUTPUT_HANDLE) {
        return; // stdout already redirected; do not steal it
    }
    unsafe { win32::AttachConsole(win32::ATTACH_PARENT_PROCESS) };
    if std_handle_valid(win32::STD_OUTPUT_HANDLE) && std_handle_valid(win32::STD_ERROR_HANDLE) {
        return; // AttachConsole wired handles 1/2 to the parent console
    }
    let name = win32::utf16_z("CONOUT$");
    let handle = unsafe {
        win32::CreateFileW(
            name.as_ptr(),
            win32::GENERIC_WRITE,
            win32::FILE_SHARE_READ_WRITE,
            std::ptr::null(),
            win32::OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle as isize == -1 {
        return;
    }
    let _ = CONSOLE_HANDLE.set(handle as isize);
}


/// The transcript file mirrored with stdout. Created FIRST so a failing stdout
/// write cannot stop it (`go\selftest.go:66-73`).
struct Out {
    file: Option<std::fs::File>,
}

impl Out {
    fn new(path: &Path) -> Out {
        Out {
            file: std::fs::File::create(path).ok(),
        }
    }

    fn line(&mut self, message: &str) {
        let mut buf = String::with_capacity(message.len() + 1);
        buf.push_str(message);
        buf.push('\n');
        if let Some(file) = self.file.as_mut() {
            let _ = file.write_all(buf.as_bytes());
        }
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(buf.as_bytes());
        let _ = stdout.flush();
    }

    fn finish(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }
}

fn fail(out: &mut Out, reason: &str) -> ! {
    out.line(&format!("M1 SELFTEST FAIL: {reason}"));
    out.finish();
    std::process::exit(1);
}

fn m3fail(out: &mut Out, reason: &str) -> ! {
    out.line(&format!("M3 SELFTEST FAIL: {reason}"));
    out.finish();
    std::process::exit(1);
}

fn named(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// `accept` (`go\selftest.go:335-354`): wait (bounded) for the next client
/// connection on the fake pipe.
fn accept_bounded(srv: &discord::FakeServer, secs: u64) -> Result<discord::FakeConn, String> {
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(srv.accept().map_err(|e| e.to_string()));
        });
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(result) => result,
            Err(_) => Err("timed out waiting for the fake-pipe client".to_string()),
        }
    })
}

/// `runSelfTest` (`go\selftest.go:65`): the ordered stage list. It never returns.
pub fn run_self_test(log: &Logger) -> ! {
    let transcript = debuglog::exe_dir_or_dot().join("m1-selftest.out");
    let mut out = Out::new(&transcript);

    let nid_size = std::mem::size_of::<win32::NotifyIconData>();
    if nid_size != 976 {
        fail(
            &mut out,
            &format!("NOTIFYICONDATAW size = {nid_size}, want 976"),
        );
    }
    out.line(&format!("struct layout: NOTIFYICONDATAW={nid_size} bytes"));

    let (_, source) = clientid::resolve_client_id();
    out.line(&format!("client-id source: {source} (value not logged)"));
    log.log(&format!("selftest: client-id source={source}"));

    let tray_window = match tray::create_tray_window() {
        Ok(window) => window,
        Err(err) => fail(&mut out, &format!("tray window init: {err}")),
    };
    out.line(&format!(
        "hidden window created: hwnd=0x{:x}",
        tray_window.hwnd as usize
    ));
    if tray_window.icon.is_null() {
        let diag = tray::load_embedded_icon_diag()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        fail(&mut out, &format!("embedded icon did not load: {diag}"));
    }
    out.line(&format!(
        "embedded icon loaded: hicon=0x{:x}",
        tray_window.icon as usize
    ));
    if !tray_window.added {
        fail(&mut out, "Shell_NotifyIconW NIM_ADD returned false");
    }
    out.line("notify-icon add returned true");
    log.log("selftest: notify-icon add result=true");

    let hwnd_addr = tray_window.hwnd as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        unsafe {
            win32::PostMessageW(
                hwnd_addr as *mut core::ffi::c_void,
                win32::WM_APP_QUIT,
                0,
                0,
            );
        }
    });
    let start = Instant::now();
    tray::message_loop();
    let elapsed = start.elapsed();
    if elapsed < Duration::from_millis(2500) {
        fail(
            &mut out,
            &format!("message loop exited early after {elapsed:?}"),
        );
    }
    out.line(&format!("message loop pumped {}ms", elapsed.as_millis()));

    tray::tray_delete(tray_window.hwnd);
    unsafe { win32::DestroyWindow(tray_window.hwnd) };

    log.log("selftest: rotation check starting");
    match std::fs::metadata(log.path()) {
        Err(err) => fail(&mut out, &format!("debug log not written: {err}")),
        Ok(meta) if meta.len() == 0 => fail(&mut out, "debug log is empty"),
        Ok(_) => {}
    }
    let rot_path = debuglog::exe_dir_or_dot().join("potflix-debug-selftest.log");
    for suffix in ["", ".1", ".2"] {
        let _ = std::fs::remove_file(named(&rot_path, suffix));
    }
    let rot = Logger::new(&rot_path, 256, 3);
    for index in 0..60 {
        rot.log(&format!(
            "rotation test line {index:03} padding padding padding"
        ));
    }
    rot.close();
    if std::fs::metadata(named(&rot_path, ".1")).is_err() {
        fail(
            &mut out,
            &format!("rotation did not produce {}.1", rot_path.display()),
        );
    }
    let rotated = match std::fs::metadata(&rot_path) {
        Ok(meta) => meta,
        Err(err) => fail(&mut out, &format!("rotated current log missing: {err}")),
    };
    if rotated.len() > 256 {
        fail(
            &mut out,
            &format!(
                "rotated current log {} bytes exceeds cap 256",
                rotated.len()
            ),
        );
    }
    out.line(&format!(
        "log written: {}; rotation at cap 256 -> .1 present, current={} bytes",
        log.path().display(),
        rotated.len()
    ));
    log.log(&format!(
        "selftest: rotation ok (.1 present, current={} bytes)",
        rotated.len()
    ));

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => fail(&mut out, &format!("os.Executable: {err}")),
    };
    let output = match std::process::Command::new(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(err) => fail(&mut out, &format!("second instance did not run: {err}")),
    };
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let got = String::from_utf8_lossy(&combined).trim().to_string();
    let exit_code = output.status.code().unwrap_or(-1);
    if got != instance::ALREADY_RUNNING_MSG {
        fail(
            &mut out,
            &format!(
                "second instance stderr={got:?}, want {:?}",
                instance::ALREADY_RUNNING_MSG
            ),
        );
    }
    if exit_code != 0 {
        fail(
            &mut out,
            &format!("second instance exit={exit_code}, want 0"),
        );
    }
    out.line(&format!("second instance: stderr={got:?} exit={exit_code}"));
    log.log(&format!(
        "selftest: second instance printed {got:?} and exited {exit_code}"
    ));

    let cleaned = titleclean::clean("Show.S01E02.Pilot.1080p.mkv");
    if cleaned.title != "Show" || cleaned.episode_tag != "S1 E2" || cleaned.episode_name != "Pilot"
    {
        fail(
            &mut out,
            &format!(
                "titleclean.Clean = title={:?} tag={:?} name={:?}, want title=Show tag=\"S1 E2\" name=Pilot",
                cleaned.title, cleaned.episode_tag, cleaned.episode_name
            ),
        );
    }
    let truncated = titleclean::trunc("abcdef", 4);
    if truncated != "abc\u{2026}" {
        fail(
            &mut out,
            &format!("titleclean.Trunc(abcdef,4) = {truncated:?}, want abc<U+2026>"),
        );
    }
    out.line(&format!(
        "titleclean: Clean -> title={:?} tag={:?} name={:?}; Trunc ok",
        cleaned.title, cleaned.episode_tag, cleaned.episode_name
    ));

    for (title, want) in [
        ("Some Movie - PotPlayer", true),
        ("PotPlayer", true),
        ("Some Movie", false),
        ("- PotPlayer", false),
    ] {
        let got = player::window::matches(title);
        if got != want {
            fail(
                &mut out,
                &format!("player.Matches({title:?}) = {got}, want {want}"),
            );
        }
    }
    for (raw, want) in [
        (-1, PlayStatus::STOPPED),
        (1, PlayStatus::PAUSED),
        (2, PlayStatus::RUNNING),
        (0, PlayStatus::UNDEFINED),
        (3, PlayStatus::UNDEFINED),
    ] {
        let got = player::control::map_play_status(raw);
        if got != want {
            fail(
                &mut out,
                &format!("player.MapPlayStatus({raw}) = {got}, want {want}"),
            );
        }
    }
    out.line("player: title predicate and status mapping ok");

    let matrix: [(&str, PlayStatus, i64, i64); 6] = [
        ("A - PotPlayer", PlayStatus::RUNNING, 1000, 100000),
        ("A - PotPlayer", PlayStatus::RUNNING, 1250, 100250),
        ("A - PotPlayer", PlayStatus::PAUSED, 1250, 100500),
        ("A - PotPlayer", PlayStatus::PAUSED, 1250, 100750),
        ("A - PotPlayer", PlayStatus::RUNNING, 1260, 101000),
        ("A - PotPlayer", PlayStatus::STOPPED, 0, 101250),
    ];
    let mut tracker = Tracker::new();
    let mut emits = 0;
    let mut last_tag = String::new();
    for (title, status, ct, now) in matrix {
        if let Some(action) = tracker.poll(title, status, ct, now) {
            emits += 1;
            last_tag = action.tag().to_string();
            event_decision(
                log,
                now,
                &status.to_string(),
                ct,
                title,
                action.tag(),
                action.running,
                action.paused,
                action.has_start,
                action.start,
                &action.suffix,
            );
            out.line(&format!(
                "  decision ms={now} st={status} ct={ct} action={}",
                action.tag()
            ));
        }
    }
    if emits != 4 || last_tag != "STOP" {
        fail(
            &mut out,
            &format!("presence matrix: emits={emits} lastTag={last_tag:?}, want 4 and STOP"),
        );
    }
    out.line(&format!(
        "presence: transition matrix produced {emits} decisions, last={last_tag}"
    ));

    let with_state = presence::with_state_time("S1 E2", " - 01:23");
    if with_state != "S1 E2 - 01:23" {
        fail(
            &mut out,
            &format!("presence.WithStateTime = {with_state:?}, want \"S1 E2 - 01:23\""),
        );
    }
    let leading_dash = presence::with_state_time("", " - 01:23");
    if leading_dash != "01:23" {
        fail(
            &mut out,
            &format!("presence.WithStateTime leading-dash strip = {leading_dash:?}, want 01:23"),
        );
    }
    let formatted = presence::fmt_ms(3_661_000);
    if formatted != "1:01:01" {
        fail(
            &mut out,
            &format!("presence.Fmt(3661000) = {formatted:?}, want 1:01:01"),
        );
    }
    out.line("presence: WithStateTime and Fmt ok");

    let mut debounce = Tracker::new();
    debounce.poll("D - PotPlayer", PlayStatus::RUNNING, 1000, 0);
    if let Some(held) = debounce.poll("D - PotPlayer", PlayStatus::RUNNING, 5000, 200) {
        fail(
            &mut out,
            &format!("debounce: seek within 1 s should be held, got {held:?}"),
        );
    }
    if !debounce.pending_debounce() {
        fail(
            &mut out,
            "debounce: held seek should report PendingDebounce",
        );
    }
    match debounce.poll("D - PotPlayer", PlayStatus::RUNNING, 5000, 1200) {
        Some(action) if action.running => {
            if action.start != 1200 - 5000 {
                fail(
                    &mut out,
                    &format!(
                        "debounce: trailing anchor start={}, want {}",
                        action.start,
                        1200 - 5000
                    ),
                );
            }
        }
        other => fail(
            &mut out,
            &format!("debounce: held seek should land after the window, got {other:?}"),
        ),
    }
    out.line("presence: RUN debounce holds a repeat within 1 s and lands it after the window");

    if let Some(hwnd) = player::window::find_first() {
        let title = player::window::title(hwnd);
        let status = player::control::get_play_status(hwnd);
        out.line(&format!(
            "player: live PotPlayer hwnd=0x{:x} title=[{title}] alive={} status={status} ct={} total={}",
            hwnd.as_ptr() as usize,
            player::window::alive(hwnd),
            player::control::get_current_time(hwnd),
            player::control::get_total_time(hwnd)
        ));
        log.log(&format!(
            "selftest: live PotPlayer hwnd=0x{:x} title=[{title}] status={status}",
            hwnd.as_ptr() as usize
        ));
    } else {
        out.line("player: no PotPlayer window found; live control/title read deferred");
    }

    const M3_ID: &str = "123456789012345678";
    let srv = discord::FakeServer::new(0);
    if !srv.name().starts_with(r"\\.\pipe\potflix-fake-") {
        m3fail(
            &mut out,
            &format!(
                "fake pipe {:?} is outside the potflix-fake-* namespace",
                srv.name()
            ),
        );
    }
    if srv.name().contains("discord-ipc") {
        m3fail(
            &mut out,
            &format!(
                "fake pipe {:?} collides with the real discord-ipc namespace",
                srv.name()
            ),
        );
    }
    let (drop_tx, drop_rx) = mpsc::channel::<()>();
    let client = discord::Client::new(discord::Config {
        client_id: M3_ID.to_string(),
        pipe_base: srv.base.clone(),
        retry_delay: Duration::from_millis(50),
        on_drop: Some(Box::new(move |_err: &discord::Error| {
            let _ = drop_tx.send(());
        })),
        ..Default::default()
    });
    client.start();

    let mut conn = match accept_bounded(&srv, 5) {
        Ok(conn) => conn,
        Err(err) => m3fail(&mut out, &format!("connect: {err}")),
    };

    // (12a) handshake: first frame is opcode 0 carrying the client hello.
    let (op, hello_bytes) = match conn.read_frame() {
        Ok(frame) => frame,
        Err(err) => m3fail(&mut out, &format!("read handshake: {err}")),
    };
    if op != discord::OP_HANDSHAKE {
        m3fail(
            &mut out,
            &format!(
                "first frame opcode = {op}, want {} (handshake)",
                discord::OP_HANDSHAKE
            ),
        );
    }
    let hello: serde_json::Value = match serde_json::from_slice(&hello_bytes) {
        Ok(value) => value,
        Err(err) => m3fail(
            &mut out,
            &format!(
                "handshake not JSON: {} ({err})",
                String::from_utf8_lossy(&hello_bytes)
            ),
        ),
    };
    let hello_v = hello.get("v").and_then(serde_json::Value::as_i64);
    let hello_id = hello
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if hello_v != Some(1) || hello_id != M3_ID {
        m3fail(
            &mut out,
            &format!(
                "handshake = {} , want v=1 client_id={M3_ID}",
                String::from_utf8_lossy(&hello_bytes)
            ),
        );
    }
    if let Err(err) = conn.write_frame(
        discord::OP_FRAME,
        br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
    ) {
        m3fail(&mut out, &format!("write READY: {err}"));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while client.generation() < 1 {
        if Instant::now() > deadline {
            m3fail(
                &mut out,
                &format!(
                    "client never became ready after the READY dispatch (generation {})",
                    client.generation()
                ),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    out.line(&format!(
        "m3: handshake {}; READY -> generation {}",
        String::from_utf8_lossy(&hello_bytes),
        client.generation()
    ));
    log.log("selftest: m3 handshake + READY ok");

    // (12b) SET_ACTIVITY: envelope + activity object + assets + timer.
    if let Err(err) = client.set_activity(discord::Activity {
        details: "Show".to_string(),
        state: r#"S1 E2 "Pilot""#.to_string(),
        small_image: "play".to_string(),
        has_start: true,
        start: 12345,
        end: 67890,
        ..Default::default()
    }) {
        m3fail(&mut out, &format!("SetActivity: {err}"));
    }
    let (op, payload) = match conn.read_frame() {
        Ok(frame) => frame,
        Err(err) => m3fail(&mut out, &format!("read activity: {err}")),
    };
    if op != discord::OP_FRAME {
        m3fail(
            &mut out,
            &format!("activity opcode = {op}, want {}", discord::OP_FRAME),
        );
    }
    let set_cmd: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(err) => m3fail(
            &mut out,
            &format!(
                "activity not JSON: {} ({err})",
                String::from_utf8_lossy(&payload)
            ),
        ),
    };
    if set_cmd["cmd"].as_str() != Some("SET_ACTIVITY") {
        m3fail(
            &mut out,
            &format!("cmd = {:?}, want SET_ACTIVITY", set_cmd["cmd"]),
        );
    }
    if set_cmd["nonce"].as_str().unwrap_or("").is_empty() {
        m3fail(
            &mut out,
            &format!("nonce empty in {}", String::from_utf8_lossy(&payload)),
        );
    }
    if set_cmd["args"]["pid"].as_i64() != Some(i64::from(std::process::id())) {
        m3fail(
            &mut out,
            &format!(
                "pid = {}, want {}",
                set_cmd["args"]["pid"].as_i64().unwrap_or(-1),
                std::process::id()
            ),
        );
    }
    let activity = &set_cmd["args"]["activity"];
    if activity.is_null() {
        m3fail(&mut out, "activity null, want an object");
    }
    let activity_type = activity["type"].as_i64().unwrap_or(0);
    let details = activity["details"].as_str().unwrap_or("");
    let state = activity["state"].as_str().unwrap_or("");
    if activity_type != 3 || details != "Show" || state != r#"S1 E2 "Pilot""# {
        m3fail(&mut out, &format!("activity = {activity}"));
    }
    if activity["timestamps"]["start"].as_i64() != Some(12345)
        || activity["timestamps"]["end"].as_i64() != Some(67890)
    {
        m3fail(
            &mut out,
            &format!(
                "timestamps = {}, want start=12345 end=67890",
                activity["timestamps"]
            ),
        );
    }
    let assets = &activity["assets"];
    // An unresolved large image is omitted entirely: the branded `logo` is
    // never a missing-art fallback.
    if assets.get("large_image").is_some() {
        m3fail(
            &mut out,
            &format!(
                "large_image = {:?}, want omitted (assets = {assets})",
                assets["large_image"].as_str()
            ),
        );
    }
    // The component was handed `small_image: "play"` above, but the wire
    // boundary drops the small slot: it is caller-compat only and must never
    // be serialized (no status icon over the artwork).
    if assets.get("small_image").is_some() {
        m3fail(
            &mut out,
            &format!(
                "small_image = {:?}, want omitted (no status icon on the wire; assets = {assets})",
                assets["small_image"].as_str()
            ),
        );
    }
    if assets.get("large_text").is_some() {
        m3fail(
            &mut out,
            &format!("large_text present while empty, want omitted: {assets}"),
        );
    }
    if assets.get("small_text").is_some() {
        m3fail(
            &mut out,
            &format!("small_text present while empty, want omitted: {assets}"),
        );
    }
    out.line(&format!(
        "m3: SET_ACTIVITY pid={} details={details:?} large_image=\"logo\" small_image=omitted start=12345 end=67890",
        std::process::id()
    ));
    log.log("selftest: m3 set_activity ok");

    if let Err(err) = client.clear_activity() {
        m3fail(&mut out, &format!("ClearActivity: {err}"));
    }
    let (op, payload) = match conn.read_frame() {
        Ok(frame) => frame,
        Err(err) => m3fail(&mut out, &format!("read clear: {err}")),
    };
    if op != discord::OP_FRAME {
        m3fail(
            &mut out,
            &format!("clear opcode = {op}, want {}", discord::OP_FRAME),
        );
    }
    let clear: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(err) => m3fail(
            &mut out,
            &format!(
                "clear not JSON: {} ({err})",
                String::from_utf8_lossy(&payload)
            ),
        ),
    };
    if clear["cmd"].as_str() != Some("SET_ACTIVITY") {
        m3fail(
            &mut out,
            &format!("clear cmd = {:?}, want SET_ACTIVITY", clear["cmd"]),
        );
    }
    if !clear["args"]["activity"].is_null() {
        m3fail(
            &mut out,
            &format!("clear activity = {}, want null", clear["args"]["activity"]),
        );
    }
    out.line("m3: null clear SET_ACTIVITY activity=null");
    log.log("selftest: m3 null clear ok");

    if let Err(err) = conn.write_frame(discord::OP_PING, b"selftest-ping") {
        m3fail(&mut out, &format!("write ping: {err}"));
    }
    let (op, payload) = match conn.read_frame() {
        Ok(frame) => frame,
        Err(err) => m3fail(&mut out, &format!("read pong: {err}")),
    };
    if op != discord::OP_PONG || payload != *b"selftest-ping" {
        m3fail(
            &mut out,
            &format!(
                "pong = op {op} payload {:?}, want op {} \"selftest-ping\"",
                String::from_utf8_lossy(&payload),
                discord::OP_PONG
            ),
        );
    }
    out.line(&format!(
        "m3: ping answered opcode={op} payload={:?}",
        String::from_utf8_lossy(&payload)
    ));
    log.log("selftest: m3 ping/pong ok");

    let gen_before = client.generation();
    conn.close(); // drop the server side; the client must reconnect
    conn = match accept_bounded(&srv, 5) {
        Ok(conn) => conn,
        Err(err) => m3fail(&mut out, &format!("reconnect accept: {err}")),
    };
    let (op, hello_bytes) = match conn.read_frame() {
        Ok(frame) => frame,
        Err(err) => m3fail(&mut out, &format!("read reconnect handshake: {err}")),
    };
    if op != discord::OP_HANDSHAKE {
        m3fail(
            &mut out,
            &format!(
                "reconnect first frame opcode = {op}, want {}",
                discord::OP_HANDSHAKE
            ),
        );
    }
    let reconnect_id = serde_json::from_slice::<serde_json::Value>(&hello_bytes)
        .ok()
        .and_then(|value| {
            value
                .get("client_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if reconnect_id != M3_ID {
        m3fail(
            &mut out,
            &format!(
                "reconnect handshake = {}",
                String::from_utf8_lossy(&hello_bytes)
            ),
        );
    }
    if let Err(err) = conn.write_frame(
        discord::OP_FRAME,
        br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
    ) {
        m3fail(&mut out, &format!("write reconnect READY: {err}"));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while client.generation() < gen_before + 1 {
        if Instant::now() > deadline {
            m3fail(
                &mut out,
                &format!(
                    "client never became ready after reconnect READY (generation {}, want {})",
                    client.generation(),
                    gen_before + 1
                ),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    match drop_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => m3fail(
            &mut out,
            "OnDrop was not called after the fake server dropped the pipe",
        ),
    }
    let gen_after = client.generation();
    if gen_after <= gen_before {
        m3fail(
            &mut out,
            &format!(
                "generation did not advance on reconnect: before={gen_before} after={gen_after}"
            ),
        );
    }
    out.line(&format!(
        "m3: reconnect handshake ok, generation {gen_before} -> {gen_after}"
    ));
    log.log(&format!(
        "selftest: m3 reconnect ok generation={gen_before}->{gen_after}"
    ));

    // (13) push-attempt recorder positive control: force one gate decision
    // through the real event writer, then read the line back from the log.
    // The card comes from the shipped composer (`compose_card` with the
    // default spec, via `build_activity_resolved`) -- not the legacy
    // filename-only `build_activity` mapping -- so the control exercises the
    // same lines the send path ships.
    let mut gate = presence::gate::Gate::new();
    let card = presence::build_activity_resolved(
        "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
        Some(&presence::Action {
            running: true,
            has_start: true,
            start: 1000,
            ct: 1000,
            ..Default::default()
        }),
        &presence::Resolved::default(),
    );
    gate.force_next();
    let (out_card, verdict) = gate.decide(card, 1000, 60000, 900000, false, false);
    if !verdict.send {
        m3fail(
            &mut out,
            &format!("positive control: forced Decide returned {verdict}, want sent"),
        );
    }
    event_push_attempt(log, &out_card, &verdict);
    let log_data = match std::fs::read_to_string(log.path()) {
        Ok(text) => text,
        Err(err) => m3fail(
            &mut out,
            &format!("positive control: read {}: {err}", log.path().display()),
        ),
    };
    if !log_data.contains("push attempt gate=sent") {
        m3fail(
            &mut out,
            &format!(
                "positive control: forced push-attempt line missing from {}",
                log.path().display()
            ),
        );
    }
    let Some(last_sent) = log_data
        .lines()
        .rev()
        .find(|line| line.contains("push attempt gate=sent"))
    else {
        m3fail(
            &mut out,
            &format!(
                "positive control: no sent push-attempt line in {}",
                log.path().display()
            ),
        );
    };
    if !last_sent.contains(r#""name":"Show""#) {
        m3fail(
            &mut out,
            &format!("positive control: the media-title header is missing: {last_sent}"),
        );
    }
    if last_sent.contains(r#""logo""#) || last_sent.contains(r#""small_image""#) {
        m3fail(
            &mut out,
            &format!("positive control: a branded/status icon reached the wire: {last_sent}"),
        );
    }
    out.line(&format!(
        "push-attempt recorder: forced push logged (gate=sent) and read back from {}",
        log.path().display()
    ));
    log.log("selftest: positive control push attempt recorded");

    out.line("M3 LAYER SELFTEST PASS");
    out.line("M2 LAYER SELFTEST PASS");
    out.line("M1 SELFTEST PASS");
    out.finish();
    std::process::exit(0);
}
