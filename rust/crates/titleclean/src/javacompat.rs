
use crate::jdk11tables::{JDK_FULL_UPPER, JDK_MN_RANGES, JDK_UPPER_RANGES, JDK_WORD_RANGES};
use crate::regexlite::Regex;

/// Java's default `.` : any code point except a line terminator.
/// Java line terminators are `\n`, `\r`, U+0085, U+2028, U+2029.
pub const ANY_CH: &str = r"[^\n\r\x{0085}\x{2028}\x{2029}]";

/// Turns a Java-flavoured pattern into this crate's engine syntax carrying
/// Java's semantics: `^` / `$` become `\A` / `\z`, the wildcard `.` excludes
/// Java's line terminators, and (when folded) ASCII letters are expanded to
/// explicit two-case classes, because Java `CASE_INSENSITIVE` is ASCII-only
/// while a Unicode-aware fold would match `ſ` / `K` (U+017F / U+212A).
pub fn port_pattern(pattern: &str, folded: bool) -> String {
    let p = if folded {
        pattern.strip_prefix("(?i)").unwrap_or(pattern)
    } else {
        pattern
    };
    let bytes = p.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 64);
    let mut in_class = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            out.push(c);
            out.push(bytes[i + 1]);
            i += 2;
            continue;
        }
        if in_class {
            out.push(c);
            if c == b']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'[' => {
                in_class = true;
                out.push(c);
            }
            b'.' => out.extend_from_slice(ANY_CH.as_bytes()),
            b'$' => out.extend_from_slice(br"\z"),
            b'^' => out.extend_from_slice(br"\A"),
            b'a'..=b'z' if folded => {
                out.push(b'[');
                out.push(c);
                out.push(c - 32);
                out.push(b']');
            }
            b'A'..=b'Z' if folded => {
                out.push(b'[');
                out.push(c + 32);
                out.push(c);
                out.push(b']');
            }
            _ => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).expect("port_pattern preserves UTF-8")
}

/// The UTF-8 byte offsets where Java's `$` can match in a non-multiline
/// pattern: the absolute end, or the start of a single final line terminator,
/// or the start of a final atomic CRLF. Terminators are multi-byte in UTF-8
/// (U+0085 = `C2 85`, U+2028 / U+2029 = `E2 80 A8` / `E2 80 A9`), so the offset
/// is the byte index where the terminator begins.
pub fn java_end_positions(s: &str) -> Vec<usize> {
    let b = s.as_bytes();
    let n = b.len();
    let mut ps = vec![n];
    if n >= 2 && b[n - 2] == b'\r' && b[n - 1] == b'\n' {
        ps.push(n - 2);
        return ps;
    }
    if n >= 1 {
        match b[n - 1] {
            b'\n' | b'\r' => ps.push(n - 1),
            0x85 if n >= 2 && b[n - 2] == 0xC2 => ps.push(n - 2),
            0xA8 | 0xA9 if n >= 3 && b[n - 3] == 0xE2 && b[n - 2] == 0x80 => ps.push(n - 3),
            _ => {}
        }
    }
    ps
}

/// Emulates Java `Matcher.replaceAll` for a pattern anchored at Java's `$`
/// (compiled here with `\z`): the final line terminator is not consumed, but
/// `$` may still match just before it.
pub fn java_dollar_replace_all(re: &Regex, s: &str, repl: &str) -> String {
    let mut best: Option<(usize, usize)> = None;
    for p in java_end_positions(s) {
        let Some((start, end)) = re.find(&s[..p]) else {
            continue;
        };
        if end != p {
            continue;
        }
        match best {
            None => best = Some((start, p)),
            Some((bs, bp)) if start < bs || (start == bs && p > bp) => {
                best = Some((start, p));
            }
            _ => {}
        }
    }
    match best {
        None => s.to_string(),
        Some((start, end)) => format!("{}{}{}", &s[..start], repl, &s[end..]),
    }
}

fn in_ranges2(rs: &[(char, char)], c: char) -> bool {
    rs.binary_search_by(|&(lo, hi)| {
        if c < lo {
            std::cmp::Ordering::Greater
        } else if c > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// Java `Character.toUpperCase` for the BMP, as JDK 11 defines it: the code
/// point plus the table's delta. A delta that lands outside the scalar range
/// cannot occur in this table; the code point is then returned unchanged.
fn in_ranges3(rs: &[(char, char, i32)], c: char) -> Option<char> {
    let i = rs
        .binary_search_by(|&(lo, hi, _)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()?;
    char::from_u32((c as i32 + rs[i].2) as u32)
}

/// JDK 11 `Character.isLetterOrDigit`.
pub fn jdk_is_letter_or_digit(c: char) -> bool {
    in_ranges2(JDK_WORD_RANGES, c)
}

/// JDK 11 `Character.getType(c) == NON_SPACING_MARK`.
pub fn jdk_is_mn(c: char) -> bool {
    in_ranges2(JDK_MN_RANGES, c)
}

/// JDK 11 `Character.toUpperCase` (simple, BMP).
pub fn jdk_upper_simple(c: char) -> char {
    in_ranges3(JDK_UPPER_RANGES, c).unwrap_or(c)
}

/// JDK 11 full `Character.toUpperCase` expansions (one-to-many).
pub fn jdk_full_upper(c: char) -> Option<&'static str> {
    JDK_FULL_UPPER
        .binary_search_by(|&(k, _)| k.cmp(&c))
        .ok()
        .map(|i| JDK_FULL_UPPER[i].1)
}

/// Java `Pattern.Bound.isWord`: `ch == '_' || isLetterOrDigit(ch)`.
pub fn is_java_word(c: char) -> bool {
    c == '_' || jdk_is_letter_or_digit(c)
}

fn decode_surrogate(hi: u16, lo: u16) -> char {
    let v = 0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
    char::from_u32(v).unwrap_or('\u{fffd}')
}

/// Mirrors `Character.codePointAt`: a surrogate pair is decoded only when the
/// unit at `i` is a high surrogate and `i + 1` is low; otherwise the single
/// unit is returned. A lone surrogate cannot be a `char` in Rust, and no JDK-11
/// table covers U+D800..U+DFFF, so it is modelled as U+FFFD, which no table
/// covers either -- the classification is identical.
pub fn java_code_point_at(u: &[u16], i: usize) -> char {
    let c = u[i];
    if (0xD800..=0xDBFF).contains(&c) && i + 1 < u.len() {
        let d = u[i + 1];
        if (0xDC00..=0xDFFF).contains(&d) {
            return decode_surrogate(c, d);
        }
    }
    char::from_u32(u32::from(c)).unwrap_or('\u{fffd}')
}

/// Mirrors `Character.codePointBefore`. Lone surrogate as in
/// [`java_code_point_at`].
pub fn java_code_point_before(u: &[u16], i: usize) -> char {
    let c = u[i - 1];
    if (0xDC00..=0xDFFF).contains(&c) && i >= 2 {
        let d = u[i - 2];
        if (0xD800..=0xDBFF).contains(&d) {
            return decode_surrogate(d, c);
        }
    }
    char::from_u32(u32::from(c)).unwrap_or('\u{fffd}')
}

/// Mirrors `Pattern.hasBaseCharacter`: it walks UTF-16 units downward (not code
/// points), so an astral base separated from the offset by marks is NOT found --
/// Java lands on the low surrogate and stops. That quirk is required here.
pub fn has_base_u(u: &[u16], x: isize) -> bool {
    let mut x = x;
    while x >= 0 {
        let c = java_code_point_at(u, x as usize);
        if jdk_is_letter_or_digit(c) {
            return true;
        }
        if jdk_is_mn(c) {
            x -= 1;
            continue;
        }
        return false;
    }
    false
}

/// Java's `\b` at UTF-16 offset `off`.
pub fn java_boundary_unit(u: &[u16], off: usize) -> bool {
    let mut left = false;
    if off > 0 {
        let c = java_code_point_before(u, off);
        left = is_java_word(c) || (jdk_is_mn(c) && has_base_u(u, off as isize - 1));
    }
    let mut right = false;
    if off < u.len() {
        let c = java_code_point_at(u, off);
        right = is_java_word(c) || (jdk_is_mn(c) && has_base_u(u, off as isize));
    }
    left != right
}

/// Encodes to UTF-16 as Java sees the string.
pub fn utf16_encode(r: &[char]) -> Vec<u16> {
    let mut u = Vec::with_capacity(r.len());
    for &c in r {
        let mut buf = [0u16; 2];
        u.extend_from_slice(c.encode_utf16(&mut buf));
    }
    u
}

/// Java's `\b` at code point position `pos`.
pub fn java_boundary(r: &[char], pos: usize) -> bool {
    let u = utf16_encode(r);
    let mut off = 0usize;
    for &c in &r[..pos.min(r.len())] {
        off += if c >= '\u{10000}' { 2 } else { 1 };
    }
    java_boundary_unit(&u, off)
}

pub fn contains_line_term(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
}

/// ASCII-only lowercase, byte for byte (Java's fold is ASCII-only).
pub fn ascii_lower(s: &str) -> String {
    let bytes: Vec<u8> = s
        .bytes()
        .map(|c| if c.is_ascii_uppercase() { c + 32 } else { c })
        .collect();
    match String::from_utf8(bytes) {
        Ok(lower) => lower,
        Err(_) => s.to_string(),
    }
}

/// Mirrors Java's `s.substring(0,1).toUpperCase() + s.substring(1)`: only the
/// first UTF-16 unit is mapped, so a leading supplementary code point (surrogate
/// pair) is left untouched, while full BMP expansions do apply.
pub fn java_upper_first(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut r: Vec<char> = s.chars().collect();
    let c = r[0];
    if c >= '\u{10000}' {
        return s.to_string();
    }
    if let Some(exp) = jdk_full_upper(c) {
        let mut out = String::from(exp);
        out.extend(r[1..].iter());
        return out;
    }
    let u = jdk_upper_simple(c);
    if u != c {
        r[0] = u;
        return r.into_iter().collect();
    }
    s.to_string()
}

/// Reconstructs a string from UTF-16 units, rendering an unpaired surrogate as
/// `?` exactly as Java's UTF-8 encoder does on output.
pub fn units_to_string(u: &[u16]) -> String {
    let mut out = String::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        if (0xD800..=0xDBFF).contains(&c) {
            if i + 1 < u.len() && (0xDC00..=0xDFFF).contains(&u[i + 1]) {
                out.push(decode_surrogate(c, u[i + 1]));
                i += 1;
            } else {
                out.push('?');
            }
        } else if (0xDC00..=0xDFFF).contains(&c) {
            out.push('?');
        } else {
            out.push(char::from_u32(u32::from(c)).unwrap_or('\u{fffd}'));
        }
        i += 1;
    }
    out
}

/// Mirrors `Main.trunc` using Java's UTF-16 length and `substring`, with a split
/// surrogate pair becoming `?` in the final UTF-8 output.
pub fn java_trunc(s: &str, n: usize) -> String {
    if s.is_empty() {
        return String::new();
    }
    let u = utf16_encode(&s.chars().collect::<Vec<char>>());
    if u.len() <= n {
        return s.to_string();
    }
    // Java would throw on `substring(0, -1)`; the port keeps the empty prefix.
    let kept = units_to_string(&u[..n.saturating_sub(1)]);
    let mut out = java_trim(&kept);
    out.push('\u{2026}');
    out
}

/// Mirrors `String.length()`: UTF-16 code units, not code points.
pub fn java_length(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Mirrors `java.lang.String.trim()`: strip bytes `<= U+0020`.
pub fn java_trim(s: &str) -> String {
    let b = s.as_bytes();
    let mut start = 0;
    let mut end = b.len();
    while start < end && b[start] <= 0x20 {
        start += 1;
    }
    while end > start && b[end - 1] <= 0x20 {
        end -= 1;
    }
    s[start..end].to_string()
}
