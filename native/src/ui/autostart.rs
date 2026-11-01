// SPDX-License-Identifier: GPL-3.0-or-later

//! "Start with system": registers the app to launch at login. Windows uses
//! the per-user Run registry key. The Flatpak asks the Background portal,
//! which writes the autostart entry outside the sandbox; any other Linux
//! build writes `~/.config/autostart` itself.

/// Applies `enabled` and reports the outcome to `done`, which may run later
/// (the portal answers asynchronously) but always on the GTK thread.
pub fn set(enabled: bool, done: impl FnOnce(Result<(), String>) + 'static) {
    #[cfg(windows)]
    done(windows::set(enabled));
    #[cfg(unix)]
    if std::env::var_os("FLATPAK_ID").is_some() {
        portal::request(enabled, done);
    } else {
        done(desktop_file::set(enabled));
    }
}

#[cfg(windows)]
mod windows {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    /// Also deleted by the uninstaller in `windows/installer.iss`.
    const VALUE_NAME: &str = "Warcraft Rustcorder";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(once(0)).collect()
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let status = if enabled {
            let exe = std::env::current_exe().map_err(|error| error.to_string())?;
            let command: Vec<u16> = once(u16::from(b'"'))
                .chain(exe.as_os_str().encode_wide())
                .chain([u16::from(b'"'), 0])
                .collect();
            // SAFETY: all strings are NUL-terminated UTF-16 that outlive the
            // call, and the size covers the whole buffer including the NUL.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                )
            }
        } else {
            // SAFETY: NUL-terminated UTF-16 strings that outlive the call.
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                status => status,
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32).to_string())
        }
    }
}

#[cfg(unix)]
mod desktop_file {
    use std::path::PathBuf;

    use gtk4::glib;

    fn path() -> PathBuf {
        glib::user_config_dir()
            .join("autostart")
            .join("io.github.JohanWes.WarcraftRustcorder.desktop")
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let path = path();
        if !enabled {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error.to_string())
                }
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Warcraft Rustcorder\n\
             Exec=\"{}\"\nIcon=io.github.JohanWes.WarcraftRustcorder\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        std::fs::create_dir_all(path.parent().expect("autostart dir"))
            .and_then(|()| std::fs::write(&path, entry))
            .map_err(|error| error.to_string())
    }
}

#[cfg(unix)]
mod portal {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk4::gio::{self, DBusCallFlags, DBusSignalFlags};
    use gtk4::glib::{self, Variant, VariantDict, prelude::*};

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

    type Done = Box<dyn FnOnce(Result<(), String>)>;

    pub fn request(enabled: bool, done: impl FnOnce(Result<(), String>) + 'static) {
        let connection = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
            Ok(connection) => connection,
            Err(error) => return done(Err(error.to_string())),
        };
        let Some(sender) = connection.unique_name() else {
            return done(Err("no D-Bus name".to_owned()));
        };
        let token = format!("wr_autostart_{}", glib::random_int());
        let handle = format!(
            "{PORTAL_PATH}/request/{}/{token}",
            sender.trim_start_matches(':').replace('.', "_")
        );

        // Subscribe before calling so a fast answer cannot be missed. The
        // subscription lives in `pending` until the answer arrives.
        let done: Rc<RefCell<Option<Done>>> = Rc::new(RefCell::new(Some(Box::new(done))));
        let pending: Rc<RefCell<Option<gio::SignalSubscription>>> = Rc::default();
        let subscription = {
            let done = Rc::clone(&done);
            let pending = Rc::clone(&pending);
            connection.subscribe_to_signal(
                None,
                Some("org.freedesktop.portal.Request"),
                Some("Response"),
                Some(&handle),
                None,
                DBusSignalFlags::NONE,
                move |signal| {
                    pending.borrow_mut().take();
                    let Some(done) = done.borrow_mut().take() else {
                        return;
                    };
                    let response = signal.parameters.child_value(0).get::<u32>();
                    let results = VariantDict::new(Some(&signal.parameters.child_value(1)));
                    let autostart = results.lookup::<bool>("autostart").ok().flatten();
                    done(match (response, autostart) {
                        (Some(0), Some(granted)) if granted == enabled => Ok(()),
                        (Some(0), _) | (Some(1), _) => {
                            Err("the desktop did not allow it".to_owned())
                        }
                        _ => Err("the desktop portal failed".to_owned()),
                    });
                },
            )
        };
        *pending.borrow_mut() = Some(subscription);

        let options = VariantDict::new(None);
        options.insert("handle_token", &token);
        options.insert(
            "reason",
            "Start recording World of Warcraft when you log in",
        );
        options.insert("autostart", enabled);
        options.insert("commandline", vec!["warcraft-rustcorder"]);
        options.insert("dbus-activatable", false);
        let parameters = Variant::tuple_from_iter(["".to_variant(), options.end()]);
        connection.call(
            Some(PORTAL),
            PORTAL_PATH,
            "org.freedesktop.portal.Background",
            "RequestBackground",
            Some(&parameters),
            None,
            DBusCallFlags::NONE,
            -1,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = result {
                    pending.borrow_mut().take();
                    if let Some(done) = done.borrow_mut().take() {
                        done(Err(error.to_string()));
                    }
                }
            },
        );
    }
}
