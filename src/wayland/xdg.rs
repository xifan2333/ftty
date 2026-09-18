//! XDG Shell protocol dispatch (`xdg_wm_base`, `xdg_surface`, `xdg_toplevel`).

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::event_loop::AppState;
use crate::wayland::WindowState;

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _state: &mut Self,
        proxy: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            let _ = state.wayland.transition_window_to(WindowState::Configured);
            // Defer the resize until the queue is drained: back-to-back configure pairs then
            // collapse into one size change instead of scrolling content on a transient size.
            state.configure_pending = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states: _,
            } => {
                // If the compositor supplies positive dimensions (tiling, fullscreen, or resize),
                // schedule the new size. If zero, the client decides its own dimensions (matching
                // WezTerm and Foot): preserve the active window size without resetting to defaults.
                if width > 0 && height > 0 {
                    state.pending_size = Some([width as u32, height as u32]);
                }
            }
            xdg_toplevel::Event::Close => {
                state.wayland.close_requested = true;
                let _ = state.wayland.transition_window_to(WindowState::Closed);
                state.running = false;
            }
            _ => {}
        }
    }
}
