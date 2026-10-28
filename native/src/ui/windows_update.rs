// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows updates. A release publishes `latest.json` (version, installer URL,
//! SHA-256) to the project's GitHub Pages; nothing reaches users before that
//! file lands there. The app downloads the installer with the `curl.exe` that
//! ships with Windows behind a progress dialog, checks its hash, quits the way
//! the tray's Quit does, and `main.rs` then runs the installer invisibly. The
//! installer starts the new version.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use libadwaita as adw;
use libadwaita::prelude::*;
use serde::Deserialize;
use warcraft_rustcorder::process;

use super::operational_actions::info_dialog as info;

const MANIFEST_URL: &str = "https://johanwes.github.io/wow-rustcorder/windows/latest.json";
/// The only place an installer may come from, so a tampered manifest cannot
/// point the app at someone else's executable.
const RELEASE_PREFIX: &str = "https://github.com/JohanWes/wow-rustcorder/releases/download/";

#[derive(Clone, Debug, Deserialize)]
struct Latest {
    version: String,
    url: String,
    sha256: String,
    /// Installer bytes for the progress bar; 0 when the manifest predates it.
    #[serde(default)]
    size: u64,
}

/// The verified installer, run by `run_pending_installer` after the app quit.
static PENDING: Mutex<Option<PathBuf>> = Mutex::new(None);

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
                "Warcraft Rustcorder is up to date",
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
            "Warcraft Rustcorder downloads the update, closes while it installs and \
             starts again afterwards. A recording in progress is saved and ends.",
        ),
    );
    dialog.add_responses(&[("later", "Later"), ("update", "Update")]);
    dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("update"));
    dialog.set_close_response("later");
    let dialog_parent = parent.clone();
    let parent = parent.clone();
    // Opened once the offer dialog has closed, so the two never overlap.
    dialog.connect_response(Some("update"), move |_, _| {
        let parent = parent.clone();
        let latest = latest.clone();
        gtk4::glib::idle_add_local_once(move || install(&parent, latest));
    });
    dialog.present(Some(&dialog_parent));
}

fn install(parent: &gtk4::Widget, latest: Latest) {
    let path = std::env::temp_dir().join(format!(
        "WarcraftRustcorder-{}-setup.exe",
        latest.version.replace(['/', '\\'], "")
    ));
    let child = process::command("curl.exe")
        .args(["-fsSL", "--connect-timeout", "20", "--speed-limit", "1024"])
        .args(["--speed-time", "30", "-o"])
        .arg(&path)
        .arg(&latest.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => Rc::new(RefCell::new(Some(child))),
        Err(error) => {
            return info(
                parent,
                "The update did not finish",
                &format!("curl.exe: {error}"),
            );
        }
    };
    let bar = gtk4::ProgressBar::new();
    bar.set_show_text(true);
    let dialog = adw::AlertDialog::new(
        Some(&format!("Updating to version {}", latest.version)),
        Some("Downloading the update…"),
    );
    dialog.set_extra_child(Some(&bar));
    dialog.add_responses(&[("cancel", "Cancel")]);
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("cancel"), {
        let child = Rc::clone(&child);
        let path = path.clone();
        move |_, _| {
            if let Some(mut child) = child.borrow_mut().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&path);
        }
    });
    dialog.present(Some(parent));
    let parent = parent.clone();
    gtk4::glib::timeout_add_local(Duration::from_millis(150), move || {
        // Cancel took the child: stop polling.
        let Some(status) = child.borrow_mut().as_mut().map(Child::try_wait) else {
            return gtk4::glib::ControlFlow::Break;
        };
        let finished = match status {
            Ok(None) => {
                show_progress(&bar, &path, latest.size);
                return gtk4::glib::ControlFlow::Continue;
            }
            Ok(Some(status)) if status.success() => Ok(()),
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut child) = child.borrow_mut().take()
                    && let Some(pipe) = child.stderr.as_mut()
                {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                Err(format!("The download failed ({status}). {}", stderr.trim()))
            }
            Err(error) => Err(format!("The download failed: {error}")),
        };
        child.borrow_mut().take();
        match finished {
            Ok(()) => verify_and_quit(&parent, &dialog, &bar, path.clone(), latest.clone()),
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                dialog.force_close();
                info(&parent, "The update did not finish", &error);
            }
        }
        gtk4::glib::ControlFlow::Break
    });
}

fn show_progress(bar: &gtk4::ProgressBar, path: &Path, size: u64) {
    let done = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    let mb = |bytes: u64| bytes as f64 / 1_000_000.0;
    if size == 0 {
        bar.pulse();
        bar.set_text(Some(&format!("{:.1} MB", mb(done))));
    } else {
        bar.set_fraction((done as f64 / size as f64).min(1.0));
        bar.set_text(Some(&format!("{:.1} of {:.1} MB", mb(done), mb(size))));
    }
}

/// Check the hash, then quit; the installer runs once the app has stopped.
fn verify_and_quit(
    parent: &gtk4::Widget,
    dialog: &adw::AlertDialog,
    bar: &gtk4::ProgressBar,
    path: PathBuf,
    latest: Latest,
) {
    dialog.set_body("Checking the download…");
    dialog.set_response_enabled("cancel", false);
    dialog.set_can_close(false);
    bar.set_fraction(1.0);
    let parent = parent.clone();
    let dialog = dialog.clone();
    gtk4::glib::spawn_future_local(async move {
        let checked = gtk4::gio::spawn_blocking({
            let path = path.clone();
            move || verify(&path, &latest.sha256)
        })
        .await
        .unwrap_or_else(|_| Err("the checksum task crashed".into()));
        match checked {
            Ok(()) => {
                if let Ok(mut pending) = PENDING.lock() {
                    *pending = Some(path);
                }
                dialog.set_body("Installing… Warcraft Rustcorder restarts in a few seconds.");
                super::request_quit();
            }
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                dialog.force_close();
                info(&parent, "The update did not finish", &error);
            }
        }
    });
}

fn verify(path: &PathBuf, expected: &str) -> Result<(), String> {
    if sha256(path)?.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err("The downloaded installer did not match its published checksum.".into())
    }
}

/// Called by `main.rs` once the recorder has stopped: the installer replaces
/// the app's files invisibly and starts the new version.
pub fn run_pending_installer() {
    let Some(path) = PENDING.lock().ok().and_then(|mut pending| pending.take()) else {
        return;
    };
    let mut log = OsString::from("/LOG=");
    log.push(std::env::temp_dir().join("WarcraftRustcorder-update.log"));
    if let Err(error) = std::process::Command::new(&path)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .arg(log)
        .spawn()
    {
        tracing::error!(%error, "the update installer could not start");
    }
}

/// `None` until the first Windows release publishes its manifest (404).
fn fetch_latest() -> Result<Option<Latest>, String> {
    let output = process::command("curl.exe")
        .args([
            "-sSL",
            "--max-time",
            "20",
            "-w",
            "\n%{http_code}",
            MANIFEST_URL,
        ])
        .output()
        .map_err(|error| format!("curl.exe: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim() {
        "200" => {
            let latest: Latest = serde_json::from_str(body).map_err(|error| error.to_string())?;
            if !latest.url.starts_with(RELEASE_PREFIX) || latest.url.contains("..") {
                return Err(format!(
                    "the update manifest points outside the project's releases: {}",
                    latest.url
                ));
            }
            Ok(Some(latest))
        }
        "404" => Ok(None),
        code => Err(format!("the update server answered HTTP {code}")),
    }
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
