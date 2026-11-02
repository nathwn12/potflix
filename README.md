# Potflix

Windows tray app that mirrors whatever plays in PotPlayer onto a Discord presence card.

## Install

Build from source. On Windows, with a Rust toolchain installed:

```powershell
git clone https://github.com/nathwn12/Potflix.git
cd Potflix
powershell -ExecutionPolicy Bypass -File tools\rust-build.ps1
.\dist\potflix.exe
```

First run opens the window on the search surface: one title field, one Search, and an empty object card for the nothing-playing state. A title with a trailing year is accepted (`Dune 2021`); picking a row previews that record locally, and the status line reads `Local preview` with nothing sent to Discord. Media folders are added from Settings; Scan reads them in. Once PotPlayer is playing and the Discord desktop app is running, the same card goes live and the status line reads `Presence live …`.

## Usage

Run `.\dist\potflix.exe` (or `run.bat`) and look for the tray icon. Right-click gives Open and Exit; closing the window parks it back in the tray.

Library folders are added with Add folder, kept fresh with Scan for new files or Refresh for a full re-walk, and previewed in the Card view before anything reaches Discord.

While something plays and the card shows the wrong title, type the title (plus the year when known) in the one field under the object and press Search, then pick the matching row. Picking previews locally; when something is playing, the list also carries `Apply to playing title`, and that stores the match on the machine so it wins from then on. Picking without Apply sends nothing. The results list carries one repair action, `Not this one? Open advanced match`, which opens the older picker with its own criteria; the tray menu's Fix match entry is that same picker from outside the window, present only while something plays. The folder list answers Up/Down/Home/End, and Enter opens the selected folder.

The Advanced disclosure holds the search scope (`All types` by default, `Movies`, `Series`). It shapes the search only: the picked row decides the preview's kind and year. The field accepts up to 128 characters with a counter; past that it refuses the character and says why.

Two switches exist:

```powershell
.\dist\potflix.exe --selftest    # headless wiring proof, no PotPlayer or Discord needed
.\dist\potflix.exe --minimized   # start silent in the tray, for autostart
```

Exit any running copy before `--selftest`: one instance runs at a time. No config file and no account are needed. For a custom Discord app name and artwork instead of the built-in presence, save the Application ID in `client-id.txt` beside the exe or export `POTFLIX_CLIENT_ID` and restart.

## What it does

- Discord card whose header is the media title: the client draws `Watching <media title>` — a movie reads `Watching Dune: Part Two`, an episode reads `Watching Breaking Bad`. No body line from a current preset repeats the header.
- Filenames cleaned into real titles (codecs, tags, release groups stripped, years parenthesized, `SxxEyy` parsed). Series cards leave the first body line empty and show `SxxEyy · Episode` on the second (` - Paused` while paused). Movie cards show `Title (Year)` on the first line and `Genres · Runtime` on the second (missing parts skipped, ` - Paused` while paused).
- Cover-slot hover text (`large_text`) capped at 128 UTF-16 units and marked with `…` when cut; the complete stored note stays in the Card view's Synopsis tooltip. The artwork slot carries the resolved poster URL or nothing — a file with no poster shows no image. No status icon and no branded stand-in.
- Count-up timer while paused, cleared card when nothing plays — no `Browsing`/`Idle` activity and no header left behind.
- Card view as local preview of the Discord card: status reads `Local preview` until something is live. State offers Default, Tag, Synopsis, or Minimal behind the Advanced disclosure; a Title/Series/Episode choice saved by an older build quietly becomes Default.
- Metadata with no keys and no accounts, cached on disk for 30 days; offline, the card falls back to the clean filename title. The cache exports and re-imports as one JSON bundle.

## Requirements

Windows, PotPlayer, the Discord desktop app (the web client has no local presence pipe), and a Rust toolchain with `cargo` for the one-time build.

## Links

- [Repository](https://github.com/nathwn12/Potflix)
- [Changelog](CHANGELOG.md)
- [Rust port notes](RUST-PORT.md)
- [Third-party notices](THIRD-PARTY-NOTICES.md)
- [Issues](https://github.com/nathwn12/Potflix/issues)
- [Releases](https://github.com/nathwn12/Potflix/releases)

## License

MIT. See [LICENSE](LICENSE).
