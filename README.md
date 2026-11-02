<p align="center">
  <img src="assets/icon.ico" width="120" height="120" alt="Potflix icon" />
</p>
<h1 align="center">Potflix</h1>
<p align="center"><strong>Mirror whatever plays in PotPlayer onto your Discord card.</strong></p>

## Install

You build from source. On Windows, with a Rust toolchain installed:

```powershell
git clone https://github.com/nathwn12/potflix.git
cd Potflix
powershell -ExecutionPolicy Bypass -File tools\rust-build.ps1
.\dist\potflix.exe
```

First run opens the window on the search surface: one title field, one Search, and an empty object card that says nothing is playing yet. Type a title (a trailing year is fine -- `Dune 2021`), press Search, and pick a row: the card shows a **local preview** of that record, and the status line under it opens with `Local preview` and says plainly that nothing was sent to Discord. Add a media folder from Settings (one disclosure away) and Scan when you want a library. Once PotPlayer is playing and the Discord desktop app is running, the same card goes live on its own and the status line switches to `Presence live …` instead.

## Use it

Run `.\dist\potflix.exe` (or `run.bat`) and look for the tray icon. Right-click it for Open and Exit; closing the window parks it back in the tray.

You add library folders with Add folder, keep them fresh with Scan for new files or Refresh for a full re-walk, and preview the card in the Card view before it reaches Discord.

While something plays and the card shows the wrong title, type the title (plus the year when you know it) in the one field under the object and press Search, then pick the row that matches. Picking previews it locally; when something is playing, the list also carries `Apply to playing title`, and that is what stores the match on your machine so it wins from then on. Picking without pressing Apply sends nothing. The results list carries exactly one repair action of its own, `Not this one? Open advanced match.`, which opens the older picker with its own criteria; the tray menu's Fix match entry is that same picker reached from outside the window, and it only exists while something is playing, because that is what it corrects. The folder list answers Up/Down/Home/End, and Enter opens the selected folder.

The Advanced disclosure holds the search scope (`All types`, the default, `Movies`, `Series`). It shapes the search only: the row you pick decides the preview's kind and year, so a series can never show as a film because of an earlier filter. The field accepts up to 128 characters and shows a counter; past that it refuses the character and says why.

Two switches exist and nothing else is honoured:

```powershell
.\dist\potflix.exe --selftest    # headless wiring proof, no PotPlayer or Discord needed
.\dist\potflix.exe --minimized   # start silent in the tray, for autostart
```

Exit any running copy before `--selftest`: one instance runs at a time. You need no config file and no account. If you want your own Discord app name and artwork instead of the built-in presence, save your Application ID in `client-id.txt` beside the exe or export `POTFLIX_CLIENT_ID` and restart.

## What you get

You get a Discord card whose header is the media title: the client draws `Watching <media title>`, so a movie reads `Watching Dune: Part Two` and an episode reads `Watching Breaking Bad` -- never `Watching Potflix`. No body line from a current preset repeats the header (a retired Title/Series line still in flight from an older build renders its stored line until it migrates to Default on load and save -- see the Card view note below). Filenames are cleaned into real titles (codecs, tags, and release groups stripped, years parenthesized, `SxxEyy` parsed). Series cards leave the first body line empty, since the header already carries the show title, and show `SxxEyy · Episode` on the second (the filename's own tag stands in for a missing code; the episode title is the resolved title, else the filename's own episode name; empty when neither side is known), with ` - Paused` while paused. Movie cards show `Title (Year)` on the first line (the clean filename title when the lookup misses) and `Genres · Runtime` on the second (missing parts skipped, ` - Paused` while paused). The synopsis rides the cover slot's hover text (`large_text`) as a snippet capped at 128 UTF-16 units and marked with `…` when it is cut; the complete stored note opens in the Card view's Synopsis tooltip, on hover or keyboard focus. There is no status icon and no branded stand-in: the artwork slot carries the resolved poster URL or nothing at all, so a file with no poster shows no image rather than the Potflix mark, and no new artwork is invented.

You get correct states without touching anything: a count-up timer while paused, and a cleared card when nothing plays -- no `Browsing`/`Idle` activity and no header left behind.

The Card view is a local preview of the Discord card, not a byte-for-byte copy of what Discord gets: the presence it renders is composed in Potflix, so its status line reads `Local preview` until something is live, and the Activity tab's full synopsis body is local. Discord has no synopsis field, so what can leave is the hover slot, and how much depends on length: when the stored note fits the 128 UTF-16-unit hover budget it is sent whole as hover text (`large_text`); when it exceeds that budget a shortened snippet is sent instead and the full body stays in Potflix. The Synopsis state preset can also place shortened synopsis text in the state line. The preview follows the live card's kind on its own: its caption is that Discord header verbatim (`Watching <media title>`), there is no Series/Movie toggle -- the kind is inherited from the matched record and displayed, never preselected by the user (the one `Advanced` disclosure carries the search scope, `All types` by default, which never changes the displayed kind) -- there is no Details row (line 1 is fixed by the shipped mapping), State offers Default, Tag, Synopsis, or Minimal behind that same disclosure, and a Title/Series/Episode choice saved by an older build quietly becomes Default. At rest the window shows the object, one field, one Search and that one disclosure; every payload knob is one click away and nothing else is on screen.

You get metadata with no keys and no accounts, cached on disk for 30 days; offline, the card falls back to the clean filename title. You can export and re-import that cache as one JSON bundle.

## Requirements

You need Windows, PotPlayer, the Discord desktop app (the web client has no local presence pipe), and a Rust toolchain with `cargo` for the one-time build.

## Links

- [Repository](https://github.com/nathwn12/potflix)
- [Changelog](CHANGELOG.md)
- [Rust port notes](RUST-PORT.md)
- [Third-party notices](THIRD-PARTY-NOTICES.md)
- [Issues](https://github.com/nathwn12/potflix/issues)
- [Releases](https://github.com/nathwn12/potflix/releases)
- [License](LICENSE) (MIT)

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
