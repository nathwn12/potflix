# Potflix release procedure

Checklist, in order. Each step names what it proves.

1. Bump `rust\Cargo.toml` `[workspace.package] version` to `X.Y.Z`.
   Proves the single version fact moved forward and nothing else did.
2. Run `cd rust; cargo update --workspace --offline`.
   Proves `rust/Cargo.lock` own-crate lines follow the bump with no network drift.
3. Update `CHANGELOG.md`: promote `## [Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD`, leave a fresh empty `## [Unreleased]` above it.
   Proves the release notes describe only what changed since the last tag.
4. Run `powershell -ExecutionPolicy Bypass -File tools\check.ps1 -Build -Selftest`.
   Proves clippy, workspace tests, parity, the app build, and the headless selftest all pass on the bumped tree.
5. Run `git add rust/Cargo.toml rust/Cargo.lock CHANGELOG.md`.
   Proves exactly the version, lock, and notes are staged and nothing else is.
6. Run `git commit -m "release: vX.Y.Z"`.
   Proves the release is one commit with the standard message.
7. Run `git tag -a vX.Y.Z -m "Potflix X.Y.Z"`.
   Proves the annotated tag names the release.
8. Run `git push origin main --tags`.
   Proves the commit and the tag landed together on origin.
9. Run `git rev-parse vX.Y.Z^{commit}` vs `git rev-parse HEAD`.
   Proves the tag points at the pushed release commit and nothing else.
10. Run `tools\make-installer.ps1`.
    Proves `dist\Potflix-Setup-X.Y.Z.exe` builds from the tagged exe version.
11. Hash installer and exe: `Get-FileHash dist\Potflix-Setup-X.Y.Z.exe`, `Get-FileHash dist\potflix.exe`.
    Proves both artifacts have recorded hashes before upload.
12. Publish both assets to the GitHub release.
    Proves the installer and the exe are the downloadable release.
13. Silent install: run the installer with its silent flag in a clean location.
    Proves the installer installs with no prompts.
14. Compare installed hash with `dist\potflix.exe`: `Get-FileHash` on the installed exe must equal the `dist\potflix.exe` hash.
    Proves the installed bytes are the built bytes.

`check.ps1`, `make-installer.ps1` and `make-icons.ps1` are build steps and stay; no release driver script exists on purpose.
