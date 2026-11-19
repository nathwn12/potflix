
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
