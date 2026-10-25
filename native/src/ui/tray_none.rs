// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows has no StatusNotifier tray yet. `start` always fails, so the shell
//! runs as if no tray watcher exists: closing the window quits.

use std::sync::{Arc, mpsc::SyncSender};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum TrayEvent {
    Open,
}

pub struct TrayBackend;

impl TrayBackend {
    pub fn start(
        _events: SyncSender<TrayEvent>,
        _wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, &'static str> {
        Err("no tray on Windows")
    }

    pub fn is_available(&self) -> bool {
        false
    }

    pub fn quit_requested(&self) -> bool {
        false
    }

    pub fn update(&self, _title: impl Into<String>) {}

    pub fn shutdown(&self) {}
}
