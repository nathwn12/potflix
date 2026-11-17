//! `titleclean` -- mirrors the Go package `go\internal\titleclean`.
//!
//! Dependency-free port of the repo's Java media-title cleaner
//! (`me.potflix.rpc.Main.clean`); see `go\internal\titleclean\README.md` and
//! `docs\history\DECISION.md` (byte-identical to the Java baseline on the full 2604-line
//! corpus and holdout, 0 diffs).
//!
//! Module split follows the Go files: [`clean`] is `clean.go`, [`javacompat`]
//! is `javacompat.go`, [`jdk11tables`] is `jdk11tables.go`, and this file is the
//! public surface `export.go` defines. [`regexlite`] has no Go counterpart: the
//! workspace forbids a third-party regex/Unicode crate here, so the matcher is
//! part of the port.
//!
//! Parity-critical Java semantics carried by the port: Java's `String.trim`,
//! Java's `\s` (which includes `\x0B`), Java's `$` end-of-input positions before
//! a line terminator, ASCII-only `CASE_INSENSITIVE` folding, `Matcher.replaceAll`
//! group-substitution rules, UTF-16 length and surrogate handling in `trunc`,
//! the Unicode-aware `\b` rule (including `Pattern.hasBaseCharacter`'s
//! UTF-16-unit walk), and the JDK-11 `Character` classification and case tables.

pub mod clean;
pub mod javacompat;
pub mod jdk11tables;
pub mod regexlite;

#[cfg(test)]
mod tests;

pub use clean::{Cleaned, build_display, clean, fold_title_marks, is_title_mark_rune, strip_tags};
pub use javacompat::{
    java_dollar_replace_all, java_length, java_trim, java_trunc, java_upper_first, units_to_string,
};
/// Go's `Trunc`: the Java-exact `Main.trunc` (UTF-16 length, U+2026 marker).
pub use javacompat::java_trunc as trunc;
pub use regexlite::Regex;
