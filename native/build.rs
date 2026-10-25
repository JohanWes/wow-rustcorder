// SPDX-License-Identifier: GPL-3.0-or-later

//! Compiles the two GResource bundles. `data/resources.gresource.xml` (CSS,
//! category icons, product mark, icon license notices) is embedded in the
//! binary. `data/spells.gresource.xml` (the spell database and its icons) is
//! compiled to `$OUT_DIR/spells.gresource` for development runs; the Flatpak
//! installs its own copy and the app mmaps it at runtime, so the large,
//! rarely-changing spell data stays out of the binary.
//! Shells out to the GLib tool instead of adding a build dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let data = manifest_dir.join("../data");
    let data = data
        .canonicalize()
        .unwrap_or_else(|error| panic!("read {}: {error}", data.display()));
    // Windows canonical paths are `\\?\` verbatim paths, which GLib cannot open.
    let data = data
        .to_str()
        .and_then(|path| path.strip_prefix(r"\\?\"))
        .map_or(data.clone(), PathBuf::from);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));

    for entry in std::fs::read_dir(data.join("assets/icons")).expect("list icon assets") {
        let entry = entry.expect("icon asset entry");
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
    // The product mark, embedded as the sidebar logo.
    println!(
        "cargo:rerun-if-changed={}",
        data.join("icons/io.github.JohanWes.WarcraftRecorder.svg")
            .display()
    );
    // The spell database: the JSON plus every bundled spell icon.
    if let Ok(entries) = std::fs::read_dir(data.join("spells")) {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
    let style_dir = manifest_dir.join("src/ui");
    println!(
        "cargo:rerun-if-changed={}",
        style_dir.join("style.css").display()
    );

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources(&manifest_dir, &out_dir);
    }

    compile(
        &data,
        &style_dir,
        &data.join("resources.gresource.xml"),
        &out_dir.join("warcraft-recorder.gresource"),
    );
    compile(
        &data,
        &style_dir,
        &data.join("spells.gresource.xml"),
        &out_dir.join("spells.gresource"),
    );
}

/// Embeds the icon and version details Windows shows for the exe.
fn windows_resources(manifest_dir: &Path, out_dir: &Path) {
    let icon = manifest_dir.join("../windows/warcraft-recorder.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = std::env::var("CARGO_PKG_VERSION").expect("package version");
    let numbers = version.replace('.', ",");
    let rc = out_dir.join("app.rc");
    std::fs::write(
        &rc,
        format!(
            r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numbers},0
PRODUCTVERSION {numbers},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Warcraft Recorder"
      VALUE "ProductName", "Warcraft Recorder"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "warcraft-recorder.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
            icon = icon.display().to_string().replace('\\', "/"),
        ),
    )
    .expect("write app.rc");
    let object = out_dir.join("app-resources.o");
    let status = Command::new("windres")
        .arg(&rc)
        .arg("-O")
        .arg("coff")
        .arg("-o")
        .arg(&object)
        .status()
        .expect("run windres (MSYS2 binutils)");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-link-arg-bins={}", object.display());
}

fn compile(data: &Path, style_dir: &Path, xml: &Path, output: &Path) {
    println!("cargo:rerun-if-changed={}", xml.display());
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(data)
        .arg("--sourcedir")
        .arg(style_dir)
        .arg(format!("--target={}", output.display()))
        .arg(xml)
        .status()
        .expect("run glib-compile-resources (provided by the GLib SDK)");
    assert!(status.success(), "glib-compile-resources failed");
}
