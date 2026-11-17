# DECISION

## Rewrite: the app moves to Rust

The app has been rewritten in Rust. The Go module in `go\` was the incumbent
that shipped; `rust\` is now the implementation — a Cargo workspace whose member
crates under `rust\crates\*` mirror the Go packages one-for-one — and `go\` is
retired at merge.

**Why.** The Go port proved the cleaner and shipped a working tray app, but the
Rust port is the target for the same surface with a stricter static guarantee
(`--locked` builds, `-D warnings` clippy as a gate), one toolchain for the whole
tree, and no C toolchain on the Win32 FFI path (`windows-sys` feature-gated
bindings). The port is a behavioural mirror, not a re-design: one crate per Go
package, the same on-disk formats, the same wire shapes.

**Why a parity oracle.** The cleaner is the one piece whose output is a
byte-exact contract, and it was already proven byte-identical to the Java
baseline by the Go port. The Rust rewrite therefore does not re-derive the Java
golden set: it treats the Go cleaner as the oracle and requires the Rust
binary to reproduce its TSV output hash-for-hash on the same corpora. The
committed contract is `rust\parity-golden.sha256` — a SHA-256 per corpus, with
the corpora themselves deliberately absent (they carry real media filenames).
The gate builds the Rust binary, runs it over the same corpora, and fails on a
hash mismatch against the oracle, so parity is checked, not asserted. The
oracle is FROZEN: the Go reproducer is retired, so the hashes can never be
regenerated — only matched.

**Retired reference.** `go\` and its original `src\main\java\me\potflix\rpc\Main.java`
were retained as the frozen reference while the Rust port took over the shipped
binary; both trees are now retired (history in git). `go\` was the oracle the
cleaner was proven against and the behaviour the Rust implementation had to
match, and `Main.java` was the source of the original Java golden set.

The record below is the original cleaner-port bake-off, kept because the Rust
parity oracle is derived from its Go result.

## Record: media-title cleaner port (original bake-off)

**Verdict: Go.**

Three ports of the repository's Java title cleaner (`me.potflix.rpc.Main.clean`)
were benchmarked against one regenerated Java golden set. All figures below are
the measured bake-off results; they are not re-derived here.

## Baseline

- Java golden regenerated from source, pinned at commit `540a1d1`.
- 2604 rows, split 2083 train / 521 holdout.
- Golden sha256: `CCDB3E76…90AE`.

## Results

### Go

- Diff vs golden: 0 diffs on every set (corpus and holdout).
- Repairs before green: five mechanisms repaired, then JDK-11 Unicode tables,
  error paths, and a 20k-input fuzz run.
- Binary: 2,190,848 bytes stripped (3,248,128 bytes plain).
- Timing: 101.6 / 99.7 / 95.7 ms; best 95.7 ms.
- LOC: `clean.go` 422, `javacompat.go` 336, `jdk11tables.go` 251,
  `main.go` 111, tests 305.
- Stdlib only.

### C#

- Diff vs golden: 0 on corpus and holdout, but 24 adversarial cases remained
  open, then 17 of 161 reviewer rows.
- `ECMAScript-ASCII` folding and JB-exactness were both refuted.
- Binary: 162,304 bytes framework-dependent vs 73,537,251 bytes
  self-contained.
- LOC: 654. Target: `net10.0`.

### Java (incumbent)

- Jar: 3,349,076 bytes, with a mutable v1 download.

## Verdict

Go. It is the only port that is diff-clean across the corpus, the holdout,
and the adversarial set, with no external dependencies and predictable
runtime.

## Residuals

- Unicode behaviour is pinned to JDK-11 / Unicode-10 tables; a future JDK
  would move the target.
- Astral uppercase drift is unreachable through `clean()` but would diverge
  if that path were exercised.
- The Go writer is strict about UTF-8 where Java's writer would replace
  malformed bytes; the difference only shows on inputs the Java baseline
  cannot emit.
