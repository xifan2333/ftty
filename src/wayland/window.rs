//! Surface and XDG Shell adapter: toplevel lifecycle, frame callbacks, scaling, and viewports.

use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_surface::{self, WlSurface},
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::event_loop::AppState;

use super::WaylandState;

pub use super::WindowState;

impl WaylandState {
    /// Creates and initializes the toplevel window once compositor and xdg_wm_base globals are bound.
    pub fn init_window<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WlSurface, ()>
            + wayland_client::Dispatch<XdgSurface, ()>
            + wayland_client::Dispatch<XdgToplevel, ()>
            + 'static,
    {
        if self.surface.is_some() {
            return;
        }

        let Some(compositor) = &self.compositor else {
            return;
        };
        let Some(xdg_wm_base) = &self.xdg_wm_base else {
            return;
        };

        let surface = compositor.create_surface(qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());

        let title = if self.title.is_empty() {
            "ftty"
        } else {
            &self.title
        };
        let app_id = if self.app_id.is_empty() {
            "ftty"
        } else {
            &self.app_id
        };
        toplevel.set_title(title.to_string());
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_min_size(160, 90);

        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.xdg_toplevel = Some(toplevel);
        let _ = self.transition_window_to(WindowState::Initializing);
    }

    /// Creates and initializes the `wp_fractional_scale_v1` instance once the manager and surface are available.
    pub fn init_fractional_scale<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpFractionalScaleV1, ()> + 'static,
    {
        if self.fractional_scale.is_some() {
            return;
        }
        let Some(manager) = &self.fractional_scale_manager else {
            return;
        };
        let Some(surface) = &self.surface else {
            return;
        };

        let fs = manager.get_fractional_scale(surface, qh, ());
        self.fractional_scale = Some(fs);
    }

    /// Creates and initializes the `wp_viewport` instance once the viewporter and surface are available.
    pub fn init_viewport<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpViewport, ()> + 'static,
    {
        if self.viewport.is_some() {
            return;
        }
        let Some(viewporter) = &self.viewporter else {
            return;
        };
        let Some(surface) = &self.surface else {
            return;
        };

        let vp = viewporter.get_viewport(surface, qh, ());
        self.viewport = Some(vp);
    }
}

impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlSurface,
        event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // The surface re-entered a visible output (e.g. after screen lock/unlock).
            // The compositor may have discarded or suspended our previously presented
            // buffer, and may have stopped issuing frame callbacks while hidden. Clear
            // the stale frame callback and force a redraw so the frame loop immediately
            // repaints and re-commits a fresh buffer without waiting for user input.
            wl_surface::Event::Enter { .. } => {
                state.frame_callback = None;
                state.needs_redraw = true;
            }
            wl_surface::Event::Leave { .. } => {
                // Surface left a visible output (e.g. lock screen engaged). Keep the
                // rendered content intact; the corresponding Enter will trigger a
                // forced repaint on visibility restoration.
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && state.frame_callback.as_ref() == Some(proxy)
        {
            state.frame_callback = None;
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.handle_preferred_scale(scale, conn);
        }
    }
}

impl Dispatch<WpViewporter, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// --- Window & XDG Shell ---

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

/// Resolves new window dimensions for an `xdg_toplevel.configure` event.
///
/// When the compositor sends positive dimensions, they are scheduled as the new size.
/// If either dimension is zero, the client decides its own dimensions (matching WezTerm
/// and Foot): active live dimensions are preserved, superseding any stale queued sizes.
#[must_use]
pub(crate) fn handle_toplevel_configure_size(
    width: i32,
    height: i32,
    live_width: u32,
    live_height: u32,
) -> [u32; 2] {
    if width > 0 && height > 0 {
        [width as u32, height as u32]
    } else {
        [live_width, live_height]
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
                state.pending_size = Some(handle_toplevel_configure_size(
                    width,
                    height,
                    state.wayland.width,
                    state.wayland.height,
                ));
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
