<p align="center">
  <img src="assets/icon.ico" width="120" height="120" alt="Potflix icon" />
</p>

# Potflix

Shows your current PotPlayer track as a Discord Rich Presence card. Built for Windows users who play media in PotPlayer.

## Install

Needs Windows, PotPlayer, the Discord desktop app, and a Rust toolchain with `cargo`. Build from source:

```powershell
git clone https://github.com/nathwn12/potflix.git
cd potflix
powershell -ExecutionPolicy Bypass -File tools\rust-build.ps1
.\dist\potflix.exe
```

## Usage

Run the exe and play media in PotPlayer:

```powershell
.\dist\potflix.exe
```

Discord shows `Watching <title>` while media plays. Closing the window parks the app in the tray; right-click the tray icon to open or exit.

## Configuration

| Key | Default | Effect |
| --- | --- | --- |
| `POTFLIX_CLIENT_ID` env or `client-id.txt` beside the exe | built-in ID | Discord app name and artwork |
| Search scope | `All types` | Limit lookup to `Movies` or `Series` |
| `--minimized` | off | Start silent in the tray, for autostart |
| `--selftest` | off | Headless wiring proof; needs no PotPlayer or Discord |

## Development

```powershell
powershell -ExecutionPolicy Bypass -File tools\check.ps1 -Build -Selftest
```

Runs clippy, the workspace tests, the title-parity step, the build, and the headless selftest.

## Links

- [Releases](https://github.com/nathwn12/potflix/releases)
- [MIT](LICENSE)
