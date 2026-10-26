// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows updates. A release publishes `latest.json` (version, installer URL,
//! SHA-256) to the project's GitHub Pages; nothing reaches users before that
//! file lands there. The app downloads the installer with the `curl.exe` that
//! ships with Windows, checks its hash, and runs it silently. The installer
//! closes this app and starts the new version.

use std::path::PathBuf;

use libadwaita as adw;
use libadwaita::prelude::*;
use serde::Deserialize;
use warcraft_recorder::process;

const MANIFEST_URL: &str =
    "https://johanwes.github.io/wow-recorder-linuxwayland/windows/latest.json";

#[derive(Clone, Debug, Deserialize)]
struct Latest {
    version: String,
    url: String,
    sha256: String,
}

/// "Check for updates": says when there is nothing new.
pub fn check(parent: &gtk4::Widget) {
    run_check(parent, true);
}

/// Startup check: stays quiet unless a newer version exists.
pub fn check_quietly(parent: &gtk4::Widget) {
    run_check(parent, false);
}

fn run_check(parent: &gtk4::Widget, report_current: bool) {
    let parent = parent.clone();
    gtk4::glib::spawn_future_local(async move {
        let latest = gtk4::gio::spawn_blocking(fetch_latest)
            .await
            .unwrap_or_else(|_| Err("the update check crashed".into()));
        match latest {
            Ok(Some(latest)) if is_newer(&latest.version, env!("CARGO_PKG_VERSION")) => {
                offer(&parent, latest)
            }
            Ok(_) if report_current => info(
                &parent,
                "Warcraft Recorder is up to date",
                &format!("Version {} is the latest.", env!("CARGO_PKG_VERSION")),
            ),
            Err(error) if report_current => info(
                &parent,
                "Could not check for updates",
                &format!("The update information could not be read: {error}"),
            ),
            Ok(_) | Err(_) => {
                tracing::info!("no Windows update offered");
            }
        }
    });
}

fn offer(parent: &gtk4::Widget, latest: Latest) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Update to version {}?", latest.version)),
        Some(
            "Warcraft Recorder closes while the update installs and starts again \
             afterwards. A recording in progress is cut short.",
        ),
    );
    dialog.add_responses(&[("later", "Later"), ("update", "Update")]);
    dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("update"));
    dialog.set_close_response("later");
    let dialog_parent = parent.clone();
    let parent = parent.clone();
    dialog.connect_response(Some("update"), move |_, _| install(&parent, latest.clone()));
    dialog.present(Some(&dialog_parent));
}

fn install(parent: &gtk4::Widget, latest: Latest) {
    let parent = parent.clone();
    gtk4::glib::spawn_future_local(async move {
        let started = gtk4::gio::spawn_blocking(move || download_and_start(&latest))
            .await
            .unwrap_or_else(|_| Err("the update task crashed".into()));
        if let Err(error) = started {
            info(&parent, "The update did not finish", &error);
        }
    });
}

/// `None` until the first Windows release publishes its manifest (404).
fn fetch_latest() -> Result<Option<Latest>, String> {
    let output = process::command("curl.exe")
        .args(["-sSL", "--max-time", "20", "-w", "\n%{http_code}", MANIFEST_URL])
        .output()
        .map_err(|error| format!("curl.exe: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim() {
        "200" => serde_json::from_str(body)
            .map(Some)
            .map_err(|error| error.to_string()),
        "404" => Ok(None),
        code => Err(format!("the update server answered HTTP {code}")),
    }
}

fn download_and_start(latest: &Latest) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!(
        "WarcraftRecorder-{}-setup.exe",
        latest.version.replace(['/', '\\'], "")
    ));
    let status = process::command("curl.exe")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(&path)
        .arg(&latest.url)
        .status()
        .map_err(|error| format!("curl.exe: {error}"))?;
    if !status.success() {
        return Err(format!("The download failed ({status})."));
    }
    let actual = sha256(&path)?;
    if !actual.eq_ignore_ascii_case(latest.sha256.trim()) {
        let _ = std::fs::remove_file(&path);
        return Err("The downloaded installer did not match its published checksum.".into());
    }
    std::process::Command::new(&path)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .spawn()
        .map(drop)
        .map_err(|error| format!("The installer could not start: {error}"))
}

/// SHA-256 by `certutil`, which every Windows has.
fn sha256(path: &PathBuf) -> Result<String, String> {
    let output = process::command("certutil.exe")
        .arg("-hashfile")
        .arg(path)
        .arg("SHA256")
        .output()
        .map_err(|error| format!("certutil.exe: {error}"))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.replace(' ', ""))
        .find(|line| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| "The installer's checksum could not be computed.".into())
}

/// Dotted numeric versions, e.g. `1.0.16` over `1.0.15`.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |version: &str| -> Vec<u64> {
        version
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    parts(candidate) > parts(current)
}

fn info(parent: &gtk4::Widget, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("close", "Close")]);
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}
