//! Embeds the Potflix icon, manifest, and version into every binary.
//!
//! Replaces Go's `go\rsrc_windows_amd64.syso` (see `go\README.md` § Windows exe
//! icon). The icon source is the repo's shared `..\..\..\assets\icon.ico`; it is
//! copied into
//! `$OUT_DIR` and referenced by bare filename so `rc.exe` finds it either next
//! to the `.rc` or through the `/I $OUT_DIR` that `embed-resource` passes.
//!
//! The `VERSIONINFO` resource is derived from `CARGO_PKG_VERSION` (the
//! workspace version, `rust\Cargo.toml`): `ProductName` is `Potflix`,
//! `CompanyName`/legal/trademark stay empty, and the numeric + string
//! `FileVersion`/`ProductVersion` carry the crate version. Dependency-free
//! (std only): the `.rc` text is written by hand, no new crate.
//!
//! Strict on purpose: if `rc.exe` is missing or fails, the build fails rather
//! than silently shipping an icon-less or version-less exe.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The exe manifest: `asInvoker`, Windows 10/11 compatibility, PerMonitorV2.
/// `supportedOS` id `{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}` is Windows 10/11;
/// the `dpiAwareness` element needs the `asm.v3` + SMI/2016 namespaces.
const MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"));
    // `rust\crates\app` -> `rust\crates` -> `rust` -> repo root: three levels up
    // from this crate's manifest dir (four from this file) is the repo's own
    // `assets\icon.ico` -- never `go\assets\icon.ico`.
    let icon = manifest_dir
        .parent()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
        .expect("the app crate lives three levels below the repo root")
        .join("assets")
        .join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    // The single version fact: `rust\Cargo.toml` workspace version via
    // `CARGO_PKG_VERSION`. Numeric `FILEVERSION`/`PRODUCTVERSION` need four
    // 16-bit parts, so `x.y.z` pads to `x,y,z,0`; anything past four parts is
    // cut. Pre-release/build suffixes (`-`, `+`) never reach the numeric
    // fields; the display strings keep the full `CARGO_PKG_VERSION`.
    let pkg_version =
        env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    let core = pkg_version
        .split(['-', '+'])
        .next()
        .unwrap_or(&pkg_version);
    let mut nums: Vec<u16> = core
        .split('.')
        .map(|part| {
            part.parse::<u16>().unwrap_or_else(|_| {
                panic!("CARGO_PKG_VERSION {pkg_version} is not numeric x.y.z")
            })
        })
        .collect();
    while nums.len() < 4 {
        nums.push(0);
    }
    nums.truncate(4);
    let (v1, v2, v3, v4) = (nums[0], nums[1], nums[2], nums[3]);

    if !icon.is_file() {
        panic!("icon resource missing: {}", icon.display());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let staged_icon = out_dir.join("icon.ico");
    fs::copy(&icon, &staged_icon)
        .unwrap_or_else(|err| panic!("cannot stage {}: {err}", icon.display()));

    // Resource id 1 -- the first ICON group, which is what Explorer and the tray
    // loader pick (`win32.go` iconResourceVersion = 0x00030000). Resource id 1
    // type 24 is the exe manifest: PerMonitorV2 DPI awareness (no Toolkit, no
    // runtime call -- the OS reads it at process start) so the owner-drawn
    // client area gets real per-monitor pixels at 100-200% instead of a
    // bitmap-stretched frame. Go declares no manifest (system-default
    // awareness); the layout math is DPI-parameterised either way, so this
    // changes crispness, not layout or behaviour. Resource id 1 type
    // VERSIONINFO carries the workspace version (numeric + string
    // FileVersion/ProductVersion, ProductName `Potflix`, empty CompanyName and
    // legal fields) so `(Get-Item dist\potflix.exe).VersionInfo` reports the
    // build's version; a DISTINCT type id shares numeric id 1 safely.
    let manifest = out_dir.join("potflix.manifest");
    fs::write(&manifest, MANIFEST_XML).expect("cannot write the generated manifest");
    let rc = out_dir.join("potflix.rc");
    let rc_text = format!(
        "1 ICON \"icon.ico\"\n1 24 \"potflix.manifest\"\n\
1 VERSIONINFO\n\
FILEVERSION {v1},{v2},{v3},{v4}\n\
PRODUCTVERSION {v1},{v2},{v3},{v4}\n\
FILEFLAGSMASK 0x3fL\n\
FILEFLAGS 0x0L\n\
FILEOS 0x40004L\n\
FILETYPE 0x1L\n\
FILESUBTYPE 0x0L\n\
BEGIN\n\
    BLOCK \"StringFileInfo\"\n\
    BEGIN\n\
        BLOCK \"040904b0\"\n\
        BEGIN\n\
            VALUE \"CompanyName\", \"\\0\"\n\
            VALUE \"FileDescription\", \"Potflix\\0\"\n\
            VALUE \"FileVersion\", \"{pkg_version}\\0\"\n\
            VALUE \"InternalName\", \"potflix\\0\"\n\
            VALUE \"LegalCopyright\", \"\\0\"\n\
            VALUE \"LegalTrademarks\", \"\\0\"\n\
            VALUE \"OriginalFilename\", \"potflix.exe\\0\"\n\
            VALUE \"ProductName\", \"Potflix\\0\"\n\
            VALUE \"ProductVersion\", \"{pkg_version}\\0\"\n\
        END\n\
    END\n\
    BLOCK \"VarFileInfo\"\n\
    BEGIN\n\
        VALUE \"Translation\", 0x409, 1200\n\
    END\n\
END\n"
    );
    fs::write(&rc, rc_text).expect("cannot write the generated .rc");

    match embed_resource::compile(&rc, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {}
        other => panic!("resource embedding failed: {other}"),
    }
}
