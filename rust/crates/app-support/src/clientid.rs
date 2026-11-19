
use std::fmt;
use std::path::{Path, PathBuf};

use crate::debuglog::Logger;

/// The Java literal (`Main.java:516`): the shipped Discord application.
pub const DEFAULT_CLIENT_ID: i64 = 1_146_489_019_925_004_359;

/// The environment override (`go\clientid.go:74`).
pub const CLIENT_ID_ENV: &str = "POTFLIX_CLIENT_ID";

/// The first candidate file, beside the exe and then in the CWD.
pub const CLIENT_ID_FILE_NAME: &str = "client-id.txt";

/// The second candidate file: raw text, never key/value parsed.
pub const CONFIG_PROPS_FILE_NAME: &str = "config.properties";

/// Shortest accepted digit run (`\d{17,20}`).
pub const MIN_CLIENT_ID_DIGITS: usize = 17;

/// Longest accepted digit run: a longer run is cut at 20 digits, greedily.
pub const MAX_CLIENT_ID_DIGITS: usize = 20;

/// The four source names `resolve_client_id` may report. Only these names are
/// ever logged; the id VALUE is never logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIdSource {
    /// The `POTFLIX_CLIENT_ID` environment variable.
    Env,
    /// The `client-id.txt` file.
    ClientIdFile,
    /// The `config.properties` file.
    ConfigProps,
    /// The hardcoded default.
    Default,
}

impl ClientIdSource {
    /// Every known source name, for the "source is one of four" proof.
    pub const ALL: [ClientIdSource; 4] = [
        ClientIdSource::Env,
        ClientIdSource::ClientIdFile,
        ClientIdSource::ConfigProps,
        ClientIdSource::Default,
    ];

    /// The fixed name reported (and logged) for this source.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientIdSource::Env => "env",
            ClientIdSource::ClientIdFile => "client-id.txt",
            ClientIdSource::ConfigProps => "config.properties",
            ClientIdSource::Default => "default",
        }
    }
}

impl fmt::Display for ClientIdSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The directory holding the running binary, so the client-id files can be
/// resolved beside the installed executable instead of only against the process
/// CWD. `None` is the Go seam's error case: the entry is dropped and the CWD
/// lookup still runs.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// The search order for the client-id files: the executable's directory first,
/// then the process CWD. An absent exe directory just drops that entry.
/// The executable directory may be the CWD; re-reading is harmless (Go keeps
/// both entries).
pub fn client_id_file_dirs(exe_dir: Option<&Path>, cwd: &Path) -> Vec<PathBuf> {
    match exe_dir {
        Some(dir) if !dir.as_os_str().is_empty() => vec![dir.to_path_buf(), cwd.to_path_buf()],
        _ => vec![cwd.to_path_buf()],
    }
}

/// The contract regex `(\d{17,20})` applied with Java's `matcher.find()` /
/// Go's `FindString`: the LEFTMOST run of 17-20 ASCII digits, taken greedily at
/// that position (a longer run is cut at 20 digits).
///
/// `None` covers both "no digit run of 17+" and "the matched run does not fit an
/// `i64`": Go's `strconv.ParseInt` failure leaves the inner branch and moves on
/// to the next candidate file, exactly as `None` does here.
pub fn first_client_id_match(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let run = i - start;
        if run >= MIN_CLIENT_ID_DIGITS {
            let take = run.min(MAX_CLIENT_ID_DIGITS);
            let digits = std::str::from_utf8(&bytes[start..start + take])
                .expect("ASCII digits are valid UTF-8");
            return digits.parse::<i64>().ok();
        }
    }
    None
}

/// `resolve_client_id` against an explicit exe directory, CWD and environment
/// value: the test seam for the contract's order. `env` is `None` when the
/// variable is unset (Go's `os.Getenv` returns `""` for both unset and empty;
/// both fall through here).
pub fn resolve_client_id_in(
    exe_dir: Option<&Path>,
    cwd: &Path,
    env: Option<&str>,
) -> (i64, ClientIdSource) {
    if let Some(value) = env
        && !value.is_empty()
        && let Ok(id) = value.trim().parse::<i64>()
    {
        return (id, ClientIdSource::Env);
    }

    for dir in client_id_file_dirs(exe_dir, cwd) {
        for (name, source) in [
            (CLIENT_ID_FILE_NAME, ClientIdSource::ClientIdFile),
            (CONFIG_PROPS_FILE_NAME, ClientIdSource::ConfigProps),
        ] {
            let Ok(bytes) = std::fs::read(dir.join(name)) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            if let Some(id) = first_client_id_match(text.trim()) {
                return (id, source);
            }
        }
    }

    (DEFAULT_CLIENT_ID, ClientIdSource::Default)
}

/// Resolves the client id for this process: env, then the files beside the
/// running executable, then the files in the process CWD, then the default.
///
/// Callers must log only the source, never the id -- see
/// [`log_client_id_source`].
pub fn resolve_client_id() -> (i64, ClientIdSource) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let env = std::env::var_os(CLIENT_ID_ENV).and_then(|value| value.into_string().ok());
    resolve_client_id_in(exe_dir().as_deref(), &cwd, env.as_deref())
}

/// Writes the Go port's exact line (`go\main.go:126`) for the resolved source.
///
/// The id VALUE is deliberately not a parameter: the only client-id fact that
/// may ever reach a log is the source name.
pub fn log_client_id_source(logger: &Logger, source: ClientIdSource) {
    logger.log(&format!("client-id source: {source} (value not logged)"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    /// A valid 17-digit id built from a repeated digit, so no fixture is a real
    /// Discord application.
    fn id(digit: char) -> i64 {
        id_of(digit, 17)
    }

    /// A run of `len` identical digits, for the regex fixtures (may exceed an
    /// `i64`, which is exactly what the overflow cases need).
    fn digits(digit: char, len: usize) -> String {
        std::iter::repeat_n(digit, len).collect()
    }

    fn id_of(digit: char, len: usize) -> i64 {
        digits(digit, len).parse().expect("<=19 digits fit an i64")
    }

    /// Every source name is a fixed string carrying no trace of the id VALUE.
    fn assert_source_hides_value(id: i64, source: ClientIdSource, ctx: &str) {
        assert!(
            ClientIdSource::ALL.contains(&source),
            "{ctx}: source {source} is not one of the four known source names"
        );
        assert!(
            !source.as_str().contains(&id.to_string()),
            "{ctx}: source string {source} leaks the id value"
        );
    }

    #[test]
    fn env_wins_over_exe_dir_and_cwd() {
        let install = Scratch::new("env-exe");
        let cwd = Scratch::new("env-cwd");
        install.write(CLIENT_ID_FILE_NAME, &id('8').to_string());
        cwd.write(CLIENT_ID_FILE_NAME, &id('9').to_string());

        let want = 12_345_678_901_234_567_i64;
        let (got, source) =
            resolve_client_id_in(Some(install.path()), cwd.path(), Some("12345678901234567"));
        assert_eq!((got, source), (want, ClientIdSource::Env));
        assert_source_hides_value(got, source, "env");
    }

    #[test]
    fn client_id_file_beats_config_properties_in_one_directory() {
        let cwd = Scratch::new("same-dir");
        cwd.write(CLIENT_ID_FILE_NAME, &id('2').to_string());
        cwd.write(CONFIG_PROPS_FILE_NAME, &id('3').to_string());

        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (id('2'), ClientIdSource::ClientIdFile));
        assert_source_hides_value(got, source, "client-id.txt");
    }

    #[test]
    fn config_properties_is_raw_scanned() {
        let cwd = Scratch::new("props-raw");
        cwd.write(CONFIG_PROPS_FILE_NAME, &id('3').to_string());
        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (id('3'), ClientIdSource::ConfigProps));

        cwd.write(
            CONFIG_PROPS_FILE_NAME,
            "not a key=value\nclientId = 44444444444444444\n",
        );
        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (id('4'), ClientIdSource::ConfigProps));
        assert_source_hides_value(got, source, "raw scan");
    }

    #[test]
    fn default_when_no_source_is_present() {
        let cwd = Scratch::new("default");
        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (DEFAULT_CLIENT_ID, ClientIdSource::Default));
        assert_source_hides_value(got, source, "default");
    }

    #[test]
    fn finds_file_beside_exe_when_cwd_has_none() {
        let install = Scratch::new("exe-first");
        let cwd = Scratch::new("exe-first-cwd");
        install.write(CLIENT_ID_FILE_NAME, &format!("{}\n", id('5')));

        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!((got, source), (id('5'), ClientIdSource::ClientIdFile));
        assert_source_hides_value(got, source, "client-id.txt beside exe, CWD elsewhere");
    }

    #[test]
    fn falls_back_to_cwd() {
        let install = Scratch::new("cwd-fallback-exe");
        let cwd = Scratch::new("cwd-fallback");
        cwd.write(CLIENT_ID_FILE_NAME, &id('6').to_string());

        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!((got, source), (id('6'), ClientIdSource::ClientIdFile));
        assert_source_hides_value(got, source, "CWD fallback");
    }

    #[test]
    fn absent_exe_dir_falls_through_to_cwd_then_default() {
        let cwd = Scratch::new("no-exe-dir");
        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (DEFAULT_CLIENT_ID, ClientIdSource::Default));

        cwd.write(CONFIG_PROPS_FILE_NAME, "clientId=77777777777777777\n");
        let (got, source) = resolve_client_id_in(None, cwd.path(), None);
        assert_eq!((got, source), (id('7'), ClientIdSource::ConfigProps));
        assert_source_hides_value(got, source, "no exe dir, CWD config.properties");
    }

    /// The full precedence ladder in one case, including the pair the Go tests
    /// leave implicit: the order is PER DIRECTORY, so the exe directory's
    /// `config.properties` outranks the CWD's `client-id.txt`.
    #[test]
    fn per_directory_order_is_exe_then_cwd_for_each_file() {
        let install = Scratch::new("ladder-exe");
        let cwd = Scratch::new("ladder-cwd");

        install.write(CONFIG_PROPS_FILE_NAME, &id('1').to_string());
        cwd.write(CLIENT_ID_FILE_NAME, &id('2').to_string());
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!(
            (got, source),
            (id('1'), ClientIdSource::ConfigProps),
            "exe config.properties must outrank cwd client-id.txt"
        );

        install.write(CLIENT_ID_FILE_NAME, &id('3').to_string());
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!((got, source), (id('3'), ClientIdSource::ClientIdFile));

        let (got, source) =
            resolve_client_id_in(Some(install.path()), cwd.path(), Some("12345678901234567"));
        assert_eq!((got, source), (12_345_678_901_234_567, ClientIdSource::Env));

        let (got, source) =
            resolve_client_id_in(Some(install.path()), cwd.path(), Some("not-a-number"));
        assert_eq!((got, source), (id('3'), ClientIdSource::ClientIdFile));

        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), Some(""));
        assert_eq!((got, source), (id('3'), ClientIdSource::ClientIdFile));
    }

    #[test]
    fn first_match_is_leftmost_and_capped_at_twenty_digits() {
        assert_eq!(first_client_id_match(""), None);
        assert_eq!(first_client_id_match("1234567890123456"), None, "16 digits");
        assert_eq!(
            first_client_id_match("12345678901234567"),
            Some(12_345_678_901_234_567)
        );
        assert_eq!(
            first_client_id_match("id=12345678901234567 trailing"),
            Some(12_345_678_901_234_567)
        );
        assert_eq!(
            first_client_id_match(&digits('1', 21)),
            None,
            "21 digits: the 20-digit cut overflows an i64"
        );
        assert_eq!(
            first_client_id_match(&id_of('1', 19).to_string()),
            Some(id_of('1', 19)),
            "19 digits: the whole run is the match"
        );
        assert_eq!(
            first_client_id_match("999 12345678901234567 88888888888888888"),
            Some(12_345_678_901_234_567)
        );
    }

    #[test]
    fn overflowing_match_falls_through_to_the_next_file() {
        let install = Scratch::new("overflow-exe");
        let cwd = Scratch::new("overflow-cwd");

        install.write(CONFIG_PROPS_FILE_NAME, &digits('9', 20));
        cwd.write(CLIENT_ID_FILE_NAME, &id('4').to_string());
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!(
            (got, source),
            (id('4'), ClientIdSource::ClientIdFile),
            "an unparseable match must fall through to the next file"
        );

        std::fs::remove_file(cwd.join(CLIENT_ID_FILE_NAME)).expect("remove cwd fixture");
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!((got, source), (DEFAULT_CLIENT_ID, ClientIdSource::Default));
    }

    #[test]
    fn source_is_one_of_four_known_names() {
        let install = Scratch::new("sources-exe");
        let cwd = Scratch::new("sources-cwd");

        let (got, source) =
            resolve_client_id_in(Some(install.path()), cwd.path(), Some("12345678901234567"));
        assert_eq!((got, source), (12_345_678_901_234_567, ClientIdSource::Env));
        assert_source_hides_value(got, source, "env");

        install.write(CLIENT_ID_FILE_NAME, "11111111111111112");
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!(
            (got, source),
            (11_111_111_111_111_112, ClientIdSource::ClientIdFile)
        );
        assert_source_hides_value(got, source, "client-id.txt");

        std::fs::remove_file(install.join(CLIENT_ID_FILE_NAME)).expect("remove client-id.txt");
        install.write(CONFIG_PROPS_FILE_NAME, "x=22222222222222223");
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!(
            (got, source),
            (22_222_222_222_222_223, ClientIdSource::ConfigProps)
        );
        assert_source_hides_value(got, source, "config.properties");

        std::fs::remove_file(install.join(CONFIG_PROPS_FILE_NAME)).expect("remove props");
        let (got, source) = resolve_client_id_in(Some(install.path()), cwd.path(), None);
        assert_eq!((got, source), (DEFAULT_CLIENT_ID, ClientIdSource::Default));
        assert_source_hides_value(got, source, "default");
    }

    #[test]
    fn resolve_client_id_uses_the_real_exe_dir_and_cwd() {
        let cwd = std::env::current_dir().expect("process cwd");
        let env = std::env::var_os(CLIENT_ID_ENV).and_then(|v| v.into_string().ok());
        let want = resolve_client_id_in(exe_dir().as_deref(), &cwd, env.as_deref());
        let got = resolve_client_id();
        assert_eq!(got, want);
        assert!(ClientIdSource::ALL.contains(&got.1));
    }

    #[test]
    fn log_line_carries_the_source_never_the_value() {
        let scratch = Scratch::new("log-source");
        let logger = Logger::new(scratch.join("potflix-debug.log"), 0, 1);
        log_client_id_source(&logger, ClientIdSource::ConfigProps);
        logger.close();

        let text = String::from_utf8(scratch.read("potflix-debug.log")).expect("utf-8 log");
        assert!(
            text.contains("client-id source: config.properties (value not logged)"),
            "log line = {text:?}"
        );
        for source in ClientIdSource::ALL {
            assert!(!source.as_str().contains(&DEFAULT_CLIENT_ID.to_string()));
        }
    }
}
