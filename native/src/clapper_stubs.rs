// SPDX-License-Identifier: GPL-3.0-or-later

//! Clapper's Rust bindings link its MPRIS and server features, which Clapper
//! cannot build on Windows (MPRIS needs gio-unix). The app uses neither, but
//! the MinGW linker still wants every referenced symbol, so these stand in.

macro_rules! unavailable {
    ($($name:ident),* $(,)?) => {
        $(
            #[unsafe(no_mangle)]
            extern "C" fn $name() {
                std::process::abort();
            }
        )*
    };
}

unavailable!(
    clapper_mpris_get_fallback_art_url,
    clapper_mpris_get_queue_controllable,
    clapper_mpris_get_type,
    clapper_mpris_new,
    clapper_mpris_set_fallback_art_url,
    clapper_mpris_set_queue_controllable,
    clapper_server_get_current_port,
    clapper_server_get_enabled,
    clapper_server_get_port,
    clapper_server_get_queue_controllable,
    clapper_server_get_running,
    clapper_server_get_type,
    clapper_server_new,
    clapper_server_set_enabled,
    clapper_server_set_port,
    clapper_server_set_queue_controllable,
);
