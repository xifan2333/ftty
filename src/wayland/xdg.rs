//! XDG Shell protocol dispatch (`xdg_wm_base`, `xdg_surface`, `xdg_toplevel`).

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::event_loop::AppState;

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
            state.wayland.configured = true;
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
                states,
            } => {
                let mut is_tiled = false;
                let mut is_maximized = false;
                let mut is_fullscreen = false;

                for chunk in states.as_chunks::<4>().0 {
                    let val = u32::from_ne_bytes(*chunk);
                    match xdg_toplevel::State::try_from(val) {
                        Ok(xdg_toplevel::State::Maximized) => is_maximized = true,
                        Ok(xdg_toplevel::State::Fullscreen) => is_fullscreen = true,
                        Ok(
                            xdg_toplevel::State::TiledLeft
                            | xdg_toplevel::State::TiledRight
                            | xdg_toplevel::State::TiledTop
                            | xdg_toplevel::State::TiledBottom,
                        ) => is_tiled = true,
                        _ => {}
                    }
                }

                let is_floating = !is_tiled && !is_maximized && !is_fullscreen;

                if is_floating && width > 0 && height > 0 {
                    state.wayland.stashed_floating_size = Some([width as u32, height as u32]);
                }

                let default_w = (state.config.columns() as u32)
                    .saturating_mul(state.font_mgr.metrics.cell_width)
                    .saturating_add(u32::from(state.config.padding_x()) * 2);
                let default_h = (state.config.rows() as u32)
                    .saturating_mul(state.font_mgr.metrics.cell_height)
                    .saturating_add(u32::from(state.config.padding_y()) * 2);

                let stashed = state
                    .wayland
                    .stashed_floating_size
                    .unwrap_or([default_w, default_h]);

                let target_w = if width > 0 { width as u32 } else { stashed[0] };
                let target_h = if height > 0 {
                    height as u32
                } else {
                    stashed[1]
                };

                state.pending_size = Some([target_w, target_h]);
            }
            xdg_toplevel::Event::Close => {
                state.wayland.close_requested = true;
                state.running = false;
            }
            _ => {}
        }
    }
}
