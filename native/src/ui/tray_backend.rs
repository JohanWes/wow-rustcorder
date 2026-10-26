// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc::SyncSender};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::StandardItem;

/// The product mark from the embedded UI bundle. The tray sends it as pixmaps
/// rather than a theme icon name, so the panel always shows the icon this
/// build ships instead of whatever an installed Flatpak exported.
const ICON_RESOURCE: &str =
    "/io/github/JohanWes/WarcraftRustcorder/icons/scalable/apps/warcraft-rustcorder.svg";
const ICON_SIZES: [i32; 6] = [16, 22, 24, 32, 48, 64];

/// The only event carried over the bounded channel is Open; it is idempotent
/// (present the window) so dropping it under saturation is harmless. Quit is a
/// latched flag instead; see `RecorderTray::request_quit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayEvent {
    Open,
}

pub struct TrayBackend {
    handle: Handle<RecorderTray>,
    available: Arc<AtomicBool>,
    quit_requested: Arc<AtomicBool>,
}

struct RecorderTray {
    events: SyncSender<TrayEvent>,
    available: Arc<AtomicBool>,
    quit_requested: Arc<AtomicBool>,
    /// Nudges the shell's drain; Open, Quit, and availability changes are
    /// otherwise never seen.
    wake: Arc<dyn Fn() + Send + Sync>,
    title: String,
    icons: Vec<ksni::Icon>,
}

impl TrayBackend {
    pub fn start(
        events: SyncSender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, ksni::Error> {
        let available = Arc::new(AtomicBool::new(true));
        let quit_requested = Arc::new(AtomicBool::new(false));
        let tray = RecorderTray {
            events,
            available: Arc::clone(&available),
            quit_requested: Arc::clone(&quit_requested),
            wake,
            title: "Warcraft Rustcorder".into(),
            icons: tray_icons(),
        };

        let mut service = tray.assume_sni_available(true);
        if std::env::var_os("FLATPAK_ID").is_some() {
            service = service.disable_dbus_name(true);
        }
        let handle = service.spawn()?;

        Ok(Self {
            handle,
            available,
            quit_requested,
        })
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    /// Set by the tray's Quit menu item. The GTK pump reads this when woken and
    /// dispatches one graceful `Shutdown`; using a latch instead of a channel
    /// send keeps the single-threaded tray executor from ever parking (a
    /// blocking send there would deadlock `shutdown`).
    pub fn quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Acquire)
    }

    pub fn update(&self, title: impl Into<String>) {
        let title = title.into();
        self.handle.update(move |tray| tray.title = title);
    }

    pub fn shutdown(&self) {
        self.handle.shutdown().wait();
    }
}

impl RecorderTray {
    /// Non-blocking: the window-present intent is idempotent, so a dropped Open
    /// under a saturated channel is fine and never stalls the tray executor.
    fn request_open(&self) {
        let _ = self.events.try_send(TrayEvent::Open);
        (self.wake)();
    }

    fn request_quit(&self) {
        self.quit_requested.store(true, Ordering::Release);
        (self.wake)();
    }
}

/// Rasterizes the product mark at the usual panel sizes as ARGB32 in network
/// byte order. Needs the UI resources registered, which `ui::register` does
/// before the tray starts.
fn tray_icons() -> Vec<ksni::Icon> {
    ICON_SIZES
        .into_iter()
        .filter_map(|size| {
            let pixbuf =
                gtk4::gdk_pixbuf::Pixbuf::from_resource_at_scale(ICON_RESOURCE, size, size, true)
                    .map_err(|error| tracing::warn!(%error, size, "cannot render tray icon"))
                    .ok()?;
            if pixbuf.n_channels() != 4 {
                return None;
            }
            let (width, height) = (pixbuf.width(), pixbuf.height());
            let stride = pixbuf.rowstride() as usize;
            let pixels = pixbuf.read_pixel_bytes();
            let mut data = Vec::with_capacity((width * height * 4) as usize);
            for row in pixels.chunks(stride).take(height as usize) {
                let (rgba, _) = row[..width as usize * 4].as_chunks::<4>();
                for [r, g, b, a] in rgba {
                    data.extend_from_slice(&[*a, *r, *g, *b]);
                }
            }
            Some(ksni::Icon {
                width,
                height,
                data,
            })
        })
        .collect()
}

impl ksni::Tray for RecorderTray {
    fn id(&self) -> String {
        "warcraft-rustcorder".into()
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.request_open();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open".into(),
                icon_name: "window-new-symbolic".into(),
                activate: Box::new(|tray: &mut RecorderTray| tray.request_open()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|tray: &mut RecorderTray| tray.request_quit()),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        self.available.store(true, Ordering::Release);
        (self.wake)();
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.available.store(false, Ordering::Release);
        (self.wake)();
        true
    }
}
