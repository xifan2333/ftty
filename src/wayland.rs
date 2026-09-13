//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

use wayland_client::QueueHandle;
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_keyboard::WlKeyboard, wl_seat::WlSeat, wl_surface::WlSurface,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

/// Tracks the lifecycle and handles of Wayland client globals and window surfaces.
#[derive(Debug, Default)]
pub struct WaylandState {
    pub compositor: Option<WlCompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub seat: Option<WlSeat>,
    pub keyboard: Option<WlKeyboard>,

    pub surface: Option<WlSurface>,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,

    pub width: u32,
    pub height: u32,
    pub configured: bool,
    pub close_requested: bool,
}

impl WaylandState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: 720,
            height: 480,
            ..Default::default()
        }
    }

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

        toplevel.set_title("ftty".to_string());
        toplevel.set_app_id("ftty".to_string());
        toplevel.set_min_size(100, 100);

        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.xdg_toplevel = Some(toplevel);
    }
}
