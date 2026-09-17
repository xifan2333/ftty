//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

pub mod clipboard;
pub mod init;
pub mod seat;
pub mod text_input;
pub mod xdg;

pub use clipboard::best_text_mime;
pub use seat::x11_button_index;

use wayland_client::QueueHandle;
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_data_device::WlDataDevice,
    wl_data_device_manager::WlDataDeviceManager, wl_data_offer::WlDataOffer,
    wl_data_source::WlDataSource, wl_keyboard::WlKeyboard, wl_pointer::WlPointer, wl_seat::WlSeat,
    wl_surface::WlSurface,
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

/// An incoming clipboard selection offer together with its advertised MIME types.
#[derive(Debug, Clone)]
pub struct OfferData {
    pub offer: WlDataOffer,
    pub mime_types: Vec<String>,
}

/// Tracks the lifecycle and handles of Wayland client globals and window surfaces.
#[derive(Debug, Default)]
pub struct WaylandState {
    pub compositor: Option<WlCompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub seat: Option<WlSeat>,
    pub keyboard: Option<WlKeyboard>,
    pub pointer: Option<WlPointer>,
    pub text_input_manager: Option<ZwpTextInputManagerV3>,
    pub text_input: Option<ZwpTextInputV3>,
    pub cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    pub cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    pub data_device_manager: Option<WlDataDeviceManager>,
    pub data_device: Option<WlDataDevice>,
    pub data_source: Option<WlDataSource>,
    pub current_offer: Option<OfferData>,

    pub surface: Option<WlSurface>,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,

    pub width: u32,
    pub height: u32,
    pub configured: bool,
    pub close_requested: bool,
    pub stashed_floating_size: Option<[u32; 2]>,
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
        toplevel.set_min_size(160, 90);

        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.xdg_toplevel = Some(toplevel);
    }

    /// Creates and initializes the `zwp_text_input_v3` instance once the manager and seat are available.
    pub fn init_text_input<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<ZwpTextInputV3, ()> + 'static,
    {
        if self.text_input.is_some() {
            return;
        }
        let Some(manager) = &self.text_input_manager else {
            return;
        };
        let Some(seat) = &self.seat else {
            return;
        };

        let text_input = manager.get_text_input(seat, qh, ());
        self.text_input = Some(text_input);
    }

    /// Creates and initializes the `wp_cursor_shape_device_v1` instance once the manager and pointer are available.
    pub fn init_cursor_shape<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpCursorShapeDeviceV1, ()> + 'static,
    {
        if self.cursor_shape_device.is_some() {
            return;
        }
        let Some(manager) = &self.cursor_shape_manager else {
            return;
        };
        let Some(pointer) = &self.pointer else {
            return;
        };

        let device = manager.get_pointer(pointer, qh, ());
        self.cursor_shape_device = Some(device);
    }

    /// Creates and initializes the `wl_data_device` instance once the manager and seat are available.
    pub fn init_data_device<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WlDataDevice, ()> + 'static,
    {
        if self.data_device.is_some() {
            return;
        }
        let Some(manager) = &self.data_device_manager else {
            return;
        };
        let Some(seat) = &self.seat else {
            return;
        };

        let device = manager.get_data_device(seat, qh, ());
        self.data_device = Some(device);
    }
}
