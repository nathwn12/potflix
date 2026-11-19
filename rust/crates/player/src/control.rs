
use core::fmt;

use windows_sys::Win32::Foundation::{LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_USER,
};

use crate::window::Handle;

/// `requestType` is the query channel: `WM_USER` (0x0400).
pub(crate) const REQUEST_TYPE: u32 = WM_USER;

/// Query message ids from `JNAMessageConst` (MESSAGE-CODES.txt).
pub(crate) const MSG_GET_TOTAL_TIME: u32 = 20482;
pub(crate) const MSG_GET_CURRENT_TIME: u32 = 20484;
pub(crate) const MSG_GET_PLAY_STATUS: u32 = 20486;

/// How long one query may block before it is treated as unanswered. The Java
/// helper's raw `SendMessage` waited forever, which froze the poll loop (and
/// every clear behind it) against a hung player window; this bound keeps the
/// loop running, which is exactly when the card has to be cleared.
const QUERY_TIMEOUT_MS: u32 = 1000;

/// `msgGetPlayFileName` (24608, 0x6020) is PotPlayer's POT_GET_PLAYFILE_NAME from
/// the ld3l/PotPlayerControl `InternalSimpleCmd.h` protocol -- the same header
/// the three ids above come from. Unlike those `LRESULT` queries it answers with
/// `WM_COPYDATA` carrying the UTF-8 file path, so it is served by
/// [`current_file`](crate::pathquery::current_file) rather than [`send`].
pub(crate) const MSG_GET_PLAY_FILE_NAME: u32 = 24608;

/// `PlayStatus` is the decoded player state. The zero value is `STOPPED`,
/// matching Java's enum order (Stopped, Paused, Running, Undefined).
///
/// A newtype over `i32` -- not a fieldless enum -- because Go's `PlayStatus` is
/// an `int` that carries values outside the four declared names (`PlayStatus(99)`
/// stringifies to `"Undefined"`), and that is observable in the decision log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayStatus(i32);

impl PlayStatus {
    pub const STOPPED: PlayStatus = PlayStatus(0);
    pub const PAUSED: PlayStatus = PlayStatus(1);
    pub const RUNNING: PlayStatus = PlayStatus(2);
    pub const UNDEFINED: PlayStatus = PlayStatus(3);
}

/// Matches `PlayStatus.toString()` in the Java app (used in the decision log).
impl fmt::Display for PlayStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::STOPPED => "Stopped",
            Self::PAUSED => "Paused",
            Self::RUNNING => "Running",
            _ => "Undefined",
        })
    }
}

/// Maps the raw `GET_PLAY_STATUS` int: -1 Stopped, 1 Paused, 2 Running, anything
/// else (including 0) Undefined.
pub fn map_play_status(raw: i32) -> PlayStatus {
    match raw {
        -1 => PlayStatus::STOPPED,
        1 => PlayStatus::PAUSED,
        2 => PlayStatus::RUNNING,
        _ => PlayStatus::UNDEFINED,
    }
}

/// The `LRESULT` low 32 bits read as a SIGNED value: `0xFFFF_FFFF` becomes -1,
/// not 4294967295. Go spells this `int(int32(uint32(r)))`
/// (`control.go:77`); getting it wrong silently turns a raw status of -1 into a
/// huge positive number, which maps to `Undefined` instead of `Stopped`.
fn lresult_low32_signed(lresult: LRESULT) -> i32 {
    lresult as u32 as i32
}

/// Issues one query and returns the raw `LRESULT`, or `None` when the window did
/// not answer: `SendMessageTimeoutW` fails (0) when the window is gone and
/// `SMTO_ABORTIFHUNG` returns immediately for a window already hung, so a hung
/// player is reported as unanswered instead of blocking the caller. (A window
/// procedure that answers 0 is indistinguishable from a failed query here, and
/// both read as not-ready below.)
fn send(hwnd: Handle, msg_id: u32) -> Option<LRESULT> {
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd.as_ptr(),
            REQUEST_TYPE,
            msg_id as WPARAM,
            0,
            SMTO_ABORTIFHUNG,
            QUERY_TIMEOUT_MS,
            &mut result,
        )
    };
    if ok == 0 { None } else { Some(result as LRESULT) }
}

/// Queries the play state through [`lresult_low32_signed`]; an unanswered query
/// is `Undefined`, which the poll loop skips.
pub fn get_play_status(hwnd: Handle) -> PlayStatus {
    match send(hwnd, MSG_GET_PLAY_STATUS) {
        Some(raw) => map_play_status(lresult_low32_signed(raw)),
        None => PlayStatus::UNDEFINED,
    }
}

/// Queries the current playback position in milliseconds; 0 (a not-ready read)
/// when the window did not answer.
pub fn get_current_time(hwnd: Handle) -> i64 {
    send(hwnd, MSG_GET_CURRENT_TIME).map_or(0, |raw| raw as i64)
}

/// Queries the media duration in milliseconds; 0 when the window did not answer.
pub fn get_total_time(hwnd: Handle) -> i64 {
    send(hwnd, MSG_GET_TOTAL_TIME).map_or(0, |raw| raw as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_play_status() {
        let cases: [(i32, PlayStatus); 7] = [
            (-1, PlayStatus::STOPPED),
            (1, PlayStatus::PAUSED),
            (2, PlayStatus::RUNNING),
            (0, PlayStatus::UNDEFINED),
            (3, PlayStatus::UNDEFINED),
            (-2, PlayStatus::UNDEFINED),
            (20486, PlayStatus::UNDEFINED),
        ];
        for (raw, want) in cases {
            assert_eq!(super::map_play_status(raw), want, "map_play_status({raw})");
        }
    }

    #[test]
    fn play_status_string() {
        assert_eq!(PlayStatus::STOPPED.to_string(), "Stopped");
        assert_eq!(PlayStatus::PAUSED.to_string(), "Paused");
        assert_eq!(PlayStatus::RUNNING.to_string(), "Running");
        assert_eq!(PlayStatus::UNDEFINED.to_string(), "Undefined");
        assert_eq!(PlayStatus(99).to_string(), "Undefined");
    }

    /// Proof for the signed low-32 read (`control.go:74-78`): a wndproc that
    /// returns -1 arrives as -1, not as 0xFFFF_FFFF.
    #[test]
    fn lresult_low32_is_signed() {
        assert_eq!(lresult_low32_signed(0xFFFF_FFFF), -1);
        assert_eq!(lresult_low32_signed(-1), -1);
        assert_eq!(lresult_low32_signed(2), 2);
        assert_eq!(
            super::map_play_status(lresult_low32_signed(0xFFFF_FFFF)),
            PlayStatus::STOPPED
        );
    }

    /// An unanswered query -- a window that cannot answer, taking the same
    /// mapping a `SendMessageTimeoutW` expiry takes -- reads as `Undefined` for
    /// the status and 0 for the times, so the poll loop keeps running instead of
    /// parking on the read (or waiting forever, as raw `SendMessage` did).
    #[test]
    fn unanswered_query_reads_not_ready() {
        let hwnd = Handle::dangling();
        assert_eq!(get_play_status(hwnd), PlayStatus::UNDEFINED);
        assert_eq!(get_current_time(hwnd), 0);
        assert_eq!(get_total_time(hwnd), 0);
    }
}
