//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

use std::io::{Read, Write};
use std::os::fd::AsFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};

use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_output::{self, Subpixel, WlOutput},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, Capability, WlSeat},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, Shape, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::error::WaylandError;
use crate::event_loop::AppState;
use crate::input::mouse::{MouseModifiers, encode_mouse_event};
use crate::input::selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
use crate::render::HoveredHyperlinkSpan;

// --- Window Lifecycle State Machine ---

/// Formal window lifecycle states for Wayland surfaces and XDG Shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowState {
    /// Window surface is unmapped or not yet created.
    #[default]
    Unmapped,
    /// Globals bound and surface requested, awaiting initial configure.
    Initializing,
    /// Received initial configure event from compositor, ready for rendering.
    Configured,
    /// Surface actively rendering and presenting frames to compositor.
    Active,
    /// Window close requested or surface destroyed.
    Closed,
}

impl WindowState {
    /// Human-readable label for error diagnostics and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmapped => "Unmapped",
            Self::Initializing => "Initializing",
            Self::Configured => "Configured",
            Self::Active => "Active",
            Self::Closed => "Closed",
        }
    }

    /// Validates whether transitioning from `self` to `next` is permitted.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Unmapped, Self::Initializing)
                | (Self::Initializing, Self::Configured)
                | (Self::Configured, Self::Active)
                | (Self::Active, Self::Active)
                | (Self::Active, Self::Configured)
                | (Self::Configured, Self::Configured)
                | (Self::Initializing, Self::Initializing)
                | (_, Self::Closed)
        )
    }

    /// Transitions to the next state, or returns [`WaylandError::InvalidStateTransition`].
    ///
    /// # Errors
    /// Returns an error if the transition violates the protocol state machine.
    pub fn transition_to(&mut self, next: Self) -> Result<(), WaylandError> {
        if self.can_transition_to(next) {
            *self = next;
            Ok(())
        } else {
            Err(WaylandError::InvalidStateTransition {
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

// --- Wayland State ---

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
    pub fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
    pub fractional_scale: Option<WpFractionalScaleV1>,
    pub viewporter: Option<WpViewporter>,
    pub viewport: Option<WpViewport>,
    pub scale_factor: f64,
    pub preferred_scale_120: u32,
    pub data_device_manager: Option<WlDataDeviceManager>,
    pub data_device: Option<WlDataDevice>,
    pub data_source: Option<WlDataSource>,
    pub current_offer: Option<OfferData>,

    pub surface: Option<WlSurface>,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,

    pub outputs: Vec<WlOutput>,
    pub output_subpixel: Option<Subpixel>,

    pub width: u32,
    pub height: u32,
    pub app_id: String,
    pub title: String,
    pub configured: bool,
    pub window_state: WindowState,
    pub close_requested: bool,
}

impl WaylandState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: 720,
            height: 480,
            scale_factor: 1.0,
            preferred_scale_120: 120,
            output_subpixel: Some(Subpixel::HorizontalRgb),
            app_id: "ftty".to_string(),
            title: "ftty".to_string(),
            ..Default::default()
        }
    }

    /// Returns `true` if both fractional scale and viewport extensions are active on the surface.
    #[must_use]
    pub fn is_fractional_scale_active(&self) -> bool {
        (self.fractional_scale.is_some() && self.viewport.is_some())
            || (self.fractional_scale.is_none() && (self.scale_factor - 1.0).abs() > 0.001)
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

    /// Transitions the window lifecycle state machine.
    ///
    /// # Errors
    /// Returns [`crate::error::WaylandError::InvalidStateTransition`] if the transition violates the lifecycle model.
    pub fn transition_window_to(
        &mut self,
        next: WindowState,
    ) -> Result<(), crate::error::WaylandError> {
        self.window_state.transition_to(next)?;
        self.configured = matches!(
            self.window_state,
            WindowState::Configured | WindowState::Active
        );
        Ok(())
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

// --- Init & Registry ---

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let comp = registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ());
                    state.wayland.compositor = Some(comp);
                    state.wayland.init_window(qh);
                    state.try_init_fractional_scale(qh);
                    state.try_init_viewport(qh);
                }
                "xdg_wm_base" => {
                    let xdg = registry.bind::<XdgWmBase, _, _>(name, 1, qh, ());
                    state.wayland.xdg_wm_base = Some(xdg);
                    state.wayland.init_window(qh);
                    state.try_init_fractional_scale(qh);
                    state.try_init_viewport(qh);
                }
                "wl_seat" => {
                    let seat = registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ());
                    state.wayland.seat = Some(seat);
                    state.wayland.init_text_input(qh);
                    state.wayland.init_data_device(qh);
                }
                "zwp_text_input_manager_v3" => {
                    let manager = registry.bind::<ZwpTextInputManagerV3, _, _>(name, 1, qh, ());
                    state.wayland.text_input_manager = Some(manager);
                    state.wayland.init_text_input(qh);
                }
                "wl_data_device_manager" => {
                    let manager = registry.bind::<WlDataDeviceManager, _, _>(name, 3, qh, ());
                    state.wayland.data_device_manager = Some(manager);
                    state.wayland.init_data_device(qh);
                }
                "wp_cursor_shape_manager_v1" => {
                    let manager = registry.bind::<WpCursorShapeManagerV1, _, _>(name, 1, qh, ());
                    state.wayland.cursor_shape_manager = Some(manager);
                    state.try_init_cursor_shape(qh);
                }
                "wp_fractional_scale_manager_v1" => {
                    let manager =
                        registry.bind::<WpFractionalScaleManagerV1, _, _>(name, 1, qh, ());
                    state.wayland.fractional_scale_manager = Some(manager);
                    state.try_init_fractional_scale(qh);
                }
                "wp_viewporter" => {
                    let viewporter = registry.bind::<WpViewporter, _, _>(name, 1, qh, ());
                    state.wayland.viewporter = Some(viewporter);
                    state.try_init_viewport(qh);
                }
                "wl_output" => {
                    let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                    state.wayland.outputs.push(output);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Geometry { subpixel, .. } = event
            && let wayland_client::WEnum::Value(sub) = subpixel
        {
            state.wayland.output_subpixel = Some(sub);
            let enable_subpixel = state.is_subpixel_preferred()
                && state.renderer.as_ref().is_some_and(|r| r.has_dual_source);
            let bgr = state.is_bgr_subpixel();
            if state.font_mgr.subpixel != enable_subpixel || state.font_mgr.bgr != bgr {
                state.font_mgr.subpixel = enable_subpixel;
                state.font_mgr.bgr = bgr;
                state.atlas.clear();
                state.needs_redraw = true;
            }
        }
    }
}

impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
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

// --- Seat & Input ---

/// Maps a Linux input button code to the X11 mouse button index used on the wire.
#[must_use]
pub fn x11_button_index(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0), // BTN_LEFT
        0x111 => Some(1), // BTN_MIDDLE
        0x112 => Some(2), // BTN_RIGHT
        _ => None,
    }
}

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(Capability::Keyboard) && state.wayland.keyboard.is_none() {
                let keyboard = proxy.get_keyboard(qh, ());
                state.wayland.keyboard = Some(keyboard);
            }
            if caps.contains(Capability::Pointer) {
                if state.wayland.pointer.is_none() {
                    let pointer = proxy.get_pointer(qh, ());
                    state.wayland.pointer = Some(pointer);
                    state.try_init_cursor_shape(qh);
                }
            } else {
                if let Some(device) = state.wayland.cursor_shape_device.take() {
                    device.destroy();
                }
                if let Some(pointer) = state.wayland.pointer.take() {
                    pointer.release();
                }
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
                state.pointer_in_surface = false;
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.keyboard.set_keymap_from_fd(fd, size as usize);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[I");
                    }
                    state.ime.active = true;
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.enable();
                        text_input.set_content_type(
                            zwp_text_input_v3::ContentHint::None,
                            zwp_text_input_v3::ContentPurpose::Terminal,
                        );
                        state.update_ime_cursor_area();
                    }
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[O");
                    }
                    state.ime.clear();
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.disable();
                        text_input.commit();
                    }
                    state.needs_redraw = true;
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.last_serial = serial;
                let pressed = key_state == KeyState::Pressed;
                if pressed
                    && let Some(action) =
                        state.keyboard.check_action(key, &state.config.keybindings)
                {
                    state.handle_key_action(action, Some(qh), Some(_conn));
                } else if let Some(bytes) = state.keyboard.handle_key_event(key, pressed, false) {
                    if pressed && state.config.auto_scroll() && !state.terminal.grid.is_alt_screen()
                    {
                        state.terminal.grid.scroll_viewport_bottom();
                    }
                    let _ = state.pty.write_all(&bytes);
                    if pressed {
                        state.update_ime_cursor_area();
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.last_serial = serial;
                state
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                state.update_hover_state();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_in_surface = true;
                state.pointer_serial = serial;
                state.last_serial = serial;
                state.mouse_pos = [surface_x, surface_y];
                state.current_cursor_shape = None;
                state.update_hover_state();
                state.update_cursor_shape();
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_in_surface = false;
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_pos = [surface_x, surface_y];
                state.update_hover_state();
                let held = state.mouse_buttons_held != 0 || state.mouse_reported;
                if state.terminal.mouse.reports_motion(held) {
                    let button = if held {
                        if state.mouse_buttons_held != 0 {
                            state.mouse_buttons_held.trailing_zeros() as u8
                        } else {
                            state.mouse_button
                        }
                    } else {
                        3
                    };
                    if state.report_mouse_event(button, true, true) {
                        return;
                    }
                }
                if state.mouse_pressed {
                    let (line, _, col) = state.cell_at_pointer(surface_x, surface_y);
                    state.selection.end = SelectionPoint::new(line, col);
                    state.needs_redraw = true;
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Pressed),
                time,
                serial,
            } => {
                state.last_serial = serial;
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                if index == 0
                    && state.keyboard.modifiers().ctrl
                    && let Some(url_owned) = state.url_at_pointer()
                {
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&url_owned)
                            .spawn();
                    });
                    return;
                }
                // Applications that requested mouse tracking own the event; Shift
                // always overrides tracking so text can still be selected.
                if state.report_mouse_event(index, true, false) {
                    state.mouse_buttons_held |= 1 << index;
                    state.mouse_reported = true;
                    state.mouse_button = index;
                    return;
                }
                if index == 1 {
                    // BTN_MIDDLE pastes the primary selection, as elsewhere on X11.
                    state.paste_clipboard(Some(conn));
                    return;
                }
                if index != 0 {
                    return;
                }
                let (line, screen_row, col) =
                    state.cell_at_pointer(state.mouse_pos[0], state.mouse_pos[1]);

                let same_cell = state.last_click_cell == Some((line, col));
                if same_cell && time.saturating_sub(state.last_click_time) < 350 {
                    state.click_count = (state.click_count % 3) + 1;
                } else {
                    state.click_count = 1;
                }
                state.last_click_time = time;
                state.last_click_cell = Some((line, col));
                state.mouse_pressed = true;
                state.update_hover_state();

                match state.click_count {
                    1 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, col),
                            SelectionPoint::new(line, col),
                            SelectionType::Simple,
                        );
                    }
                    2 => {
                        let row = state.terminal.grid.visible_line(screen_row);
                        let (w_start, w_end) = find_word_boundaries(row, col);
                        state.selection = Selection::new(
                            SelectionPoint::new(line, w_start),
                            SelectionPoint::new(line, w_end),
                            SelectionType::Word,
                        );
                    }
                    3 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, 0),
                            SelectionPoint::new(line, state.terminal.grid.cols.saturating_sub(1)),
                            SelectionType::Line,
                        );
                    }
                    _ => {}
                }
                state.needs_redraw = true;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Released),
                ..
            } => {
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                let was_held = (state.mouse_buttons_held & (1 << index)) != 0;
                if was_held {
                    state.mouse_buttons_held &= !(1 << index);
                    state.mouse_reported = state.mouse_buttons_held != 0;
                    state.report_mouse_event(index, false, false);
                }
                if index == 0 {
                    state.mouse_pressed = false;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                let multiplier = f64::from(state.config.scroll_multiplier());
                state.scroll_accumulator += (value / 15.0) * multiplier;

                let lines = state.scroll_accumulator.trunc() as i32;
                if lines == 0 {
                    return;
                }
                state.scroll_accumulator -= f64::from(lines);
                let count = (lines.unsigned_abs() as usize).min(100);

                if state.terminal.mouse.is_reporting() && !state.keyboard.modifiers().shift {
                    // Wheel notches are reported as buttons 64 (up) and 65 (down).
                    let button = if lines < 0 { 64 } else { 65 };
                    for _ in 0..count {
                        if !state.report_mouse_event(button, true, false) {
                            break;
                        }
                    }
                } else if state.terminal.grid.is_alt_screen() {
                    let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
                    let batch = seq.repeat(count);
                    let _ = state.pty.write_all(&batch);
                } else {
                    if lines < 0 {
                        state.terminal.grid.scroll_viewport_up(count);
                    } else {
                        state.terminal.grid.scroll_viewport_down(count);
                    }
                    state.needs_redraw = true;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                state.scroll_accumulator = 0.0;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeManagerV1,
        _event: wp_cursor_shape_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeDeviceV1,
        _event: wp_cursor_shape_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl AppState {
    /// Initializes the cursor shape device if protocol is available and updates shape if focused.
    pub fn try_init_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.wayland.cursor_shape_device.is_none() {
            self.wayland.init_cursor_shape(qh);
            if self.wayland.cursor_shape_device.is_some()
                && self.pointer_in_surface
                && self.pointer_serial != 0
            {
                self.current_cursor_shape = None;
                self.update_cursor_shape();
            }
        }
    }

    /// Updates the Wayland cursor shape based on whether a hyperlink is currently hovered.
    pub fn update_cursor_shape(&mut self) {
        if !self.pointer_in_surface || self.pointer_serial == 0 {
            return;
        }
        let shape = if self.hovered_span.is_some() {
            Shape::Pointer
        } else {
            Shape::Text
        };
        if self.current_cursor_shape == Some(shape) {
            return;
        }
        if let Some(device) = &self.wayland.cursor_shape_device {
            device.set_shape(self.pointer_serial, shape);
            self.current_cursor_shape = Some(shape);
        }
    }

    /// Updates the hovered hyperlink and cursor shape based on current pointer coordinates.
    pub fn update_hover_state(&mut self) {
        let new_span = if !self.pointer_in_surface || self.mouse_pressed {
            None
        } else {
            let (line, screen_row, col) =
                self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
            let row = self.terminal.grid.visible_line(screen_row);
            if let Some(cell) = row.cells.get(col)
                && let Some(id) = cell.hyperlink_id
                && self.terminal.hyperlink_url(id.get()).is_some()
            {
                let mut start_col = col;
                while start_col > 0
                    && row.cells.get(start_col - 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    start_col -= 1;
                }
                let mut end_col = col;
                while end_col + 1 < row.cells.len()
                    && row.cells.get(end_col + 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    end_col += 1;
                }
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else if self.keyboard.modifiers().ctrl
                && let Some((start_col, end_col, _)) =
                    find_url_in_grid(&self.terminal.grid, line, screen_row, col)
            {
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else {
                None
            }
        };
        if self.hovered_span != new_span {
            self.hovered_span = new_span;
            self.needs_redraw = true;
            self.update_cursor_shape();
        }
    }

    /// Returns the resolved URL at the current pointer position if one exists.
    #[must_use]
    pub fn url_at_pointer(&self) -> Option<String> {
        let (line, screen_row, col) = self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
        let row = self.terminal.grid.visible_line(screen_row);
        let cell = row.cells.get(col);
        if let Some(cell) = cell
            && let Some(id) = cell.hyperlink_id
            && let Some(url) = self.terminal.hyperlink_url(id.get())
        {
            return Some(url.to_string());
        }
        find_url_in_grid(&self.terminal.grid, line, screen_row, col).map(|(_, _, url)| url)
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the surface-relative pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, surface_x: f64, surface_y: f64) -> (usize, usize, usize) {
        let scale = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        let cw = f64::from(self.font_mgr.metrics.cell_width) / scale;
        let ch = f64::from(self.font_mgr.metrics.cell_height) / scale;
        let pad_x = f64::from(self.config.padding_x());
        let pad_y = f64::from(self.config.padding_y());

        let col = ((surface_x - pad_x) / cw).max(0.0) as usize;
        let col = col.min(self.terminal.grid.cols.saturating_sub(1));

        let screen_row = ((surface_y - pad_y) / ch).max(0.0) as usize;
        let screen_row = screen_row.min(self.terminal.grid.rows.saturating_sub(1));

        let abs_line =
            self.terminal.grid.scrollback.len() + screen_row - self.terminal.grid.viewport_offset;
        (abs_line, screen_row, col)
    }

    /// Encodes a pointer event for the application, or `None` when the event stays local.
    ///
    /// Mouse reports are suppressed while tracking is disabled and while Shift is held,
    /// which is the conventional override that hands the pointer back to text selection.
    #[doc(hidden)]
    pub fn mouse_report_bytes(&self, button: u8, pressed: bool, motion: bool) -> Option<Vec<u8>> {
        if !self.terminal.mouse.is_reporting() {
            return None;
        }
        let modifiers = self.keyboard.modifiers();
        if modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
        encode_mouse_event(
            self.terminal.mouse.encoding,
            button,
            col,
            screen_row,
            pressed,
            motion,
            MouseModifiers {
                shift: modifiers.shift,
                alt: modifiers.alt,
                ctrl: modifiers.ctrl,
            },
        )
    }

    /// Forwards a pointer event to the PTY and reports whether the application consumed it.
    pub(crate) fn report_mouse_event(&mut self, button: u8, pressed: bool, motion: bool) -> bool {
        let Some(bytes) = self.mouse_report_bytes(button, pressed, motion) else {
            return false;
        };
        self.write_pty_blocking(&bytes);
        true
    }
}

/// Extracts a plaintext URL across wrapped lines in `grid` containing `(target_line, target_screen_row, target_col)`.
/// Returns `(start_col, end_col, url)` where `start_col..=end_col` is the span on `target_line`.
#[must_use]
pub fn find_url_in_grid(
    grid: &crate::grid::Grid,
    target_line: usize,
    target_screen_row: usize,
    target_col: usize,
) -> Option<(usize, usize, String)> {
    let mut start_row = target_screen_row;
    while start_row > 0 {
        if grid.visible_line(start_row - 1).wrapped {
            start_row -= 1;
        } else {
            break;
        }
    }
    let mut end_row = target_screen_row;
    while end_row + 1 < grid.rows {
        if grid.visible_line(end_row).wrapped {
            end_row += 1;
        } else {
            break;
        }
    }

    let mut text = String::new();
    let mut coords: Vec<(usize, usize, usize)> = Vec::new(); // (abs_line, col, width)

    for r in start_row..=end_row {
        let line = grid.visible_line(r);
        let abs_line = grid.scrollback.len() + r - grid.viewport_offset;
        for (c_idx, cell) in line.cells.iter().enumerate() {
            if cell.flags.contains(crate::grid::CellFlags::HIDDEN) {
                text.push(' ');
                coords.push((abs_line, c_idx, 1));
            } else if cell
                .flags
                .contains(crate::grid::CellFlags::WIDE_CHAR_SPACER)
            {
                // Skip wide character continuation spacers
            } else {
                let width = if cell.flags.contains(crate::grid::CellFlags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                text.push(cell.c);
                coords.push((abs_line, c_idx, width));
            }
        }
    }

    let schemes = ["https://", "http://", "file://", "gemini://"];
    for scheme in &schemes {
        let mut search_from = 0;
        while let Some(pos) = text[search_from..].find(scheme) {
            let start = search_from + pos;
            let mut end = start;
            for (idx, ch) in text[start..].char_indices() {
                if ch.is_whitespace()
                    || ch == '<'
                    || ch == '>'
                    || ch == '"'
                    || ch == '`'
                    || ch == '^'
                    || ch == '\\'
                    || ch == '|'
                {
                    break;
                }
                end = start + idx + ch.len_utf8();
            }

            while end > start {
                let Some(last_char) = text[..end].chars().next_back() else {
                    break;
                };
                if matches!(
                    last_char,
                    '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '\'' | '"'
                ) {
                    if last_char == ')'
                        && text[start..end].matches('(').count()
                            == text[start..end].matches(')').count()
                    {
                        break;
                    }
                    end -= last_char.len_utf8();
                } else {
                    break;
                }
            }

            let start_char_idx = text[..start].chars().count();
            let end_char_idx = text[..end].chars().count();

            if start_char_idx < coords.len() && end_char_idx <= coords.len() {
                let url_coords = &coords[start_char_idx..end_char_idx];
                let is_hit = url_coords.iter().any(|&(line, c, w)| {
                    line == target_line && target_col >= c && target_col < c + w
                });

                if is_hit && end > start + scheme.len() {
                    let mut min_col = usize::MAX;
                    let mut max_col = 0;
                    for &(line, c, w) in url_coords {
                        if line == target_line {
                            min_col = min_col.min(c);
                            max_col = max_col.max(c + w - 1);
                        }
                    }
                    if min_col <= max_col {
                        let url = text[start..end].to_string();
                        return Some((min_col, max_col, url));
                    }
                }
            }

            search_from = start + scheme.len();
        }
    }

    None
}

/// Extracts a plaintext URL and its column span on a given row if `col` falls within it.
#[must_use]
pub fn find_url_at_col(row: &crate::grid::Row, col: usize) -> Option<(usize, usize, String)> {
    let mut grid = crate::grid::Grid::new(row.cells.len(), 1, 0);
    grid.lines[0] = row.clone();
    find_url_in_grid(&grid, 0, 0, col)
}

// --- Text Input (IME) ---

impl Dispatch<ZwpTextInputManagerV3, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.active = true;
                }
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.clear();
                    state.needs_redraw = true;
                }
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.ime.stage_preedit(text, cursor_begin, cursor_end);
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime.stage_commit(text);
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.ime.stage_delete(before_length, after_length);
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let (delete, commit) = state.ime.apply_done();
                if let Some((before, after)) = delete {
                    for _ in 0..before {
                        let _ = state.pty.write_all(b"\x08");
                    }
                    for _ in 0..after {
                        let _ = state.pty.write_all(b"\x1b[3~");
                    }
                }
                if let Some(text) = commit {
                    let _ = state.pty.write_all(text.as_bytes());
                }
                state.update_ime_cursor_area();
                state.needs_redraw = true;
            }
            _ => {}
        }
    }
}

impl AppState {
    /// Updates the Wayland `text-input-v3` cursor bounding box so the IME popup window tracks the cursor.
    pub fn update_ime_cursor_area(&self) {
        let Some(text_input) = &self.wayland.text_input else {
            return;
        };
        let (x, y, w, h) = crate::input::ime::calculate_cursor_rect(
            &self.terminal.grid,
            self.font_mgr.metrics,
            [self.config.padding_x(), self.config.padding_y()],
        );
        let scale = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        let logical_x = (f64::from(x) / scale).round() as i32;
        let logical_y = (f64::from(y) / scale).round() as i32;
        let logical_w = (f64::from(w) / scale).round().max(1.0) as i32;
        let logical_h = (f64::from(h) / scale).round().max(1.0) as i32;
        text_input.set_cursor_rectangle(logical_x, logical_y, logical_w, logical_h);
        text_input.commit();
    }
}

// --- Clipboard ---

/// Selects the highest-fidelity UTF-8 text MIME type advertised by an offer.
#[must_use]
pub fn best_text_mime(mimes: &[String]) -> Option<&str> {
    for candidate in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
        if let Some(found) = mimes.iter().find(|m| m.as_str() == candidate) {
            return Some(found.as_str());
        }
    }
    None
}

impl Dispatch<WlDataDeviceManager, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state.pending_offers.len() >= 4 {
                    state.pending_offers.remove(0);
                }
                state.pending_offers.push(crate::wayland::OfferData {
                    offer: id,
                    mime_types: Vec::new(),
                });
            }
            wl_data_device::Event::Selection { id } => {
                state.wayland.current_offer = id.and_then(|offer| {
                    state
                        .pending_offers
                        .iter()
                        .position(|o| o.offer == offer)
                        .map(|idx| state.pending_offers.swap_remove(idx))
                });
                if state.wayland.current_offer.is_none() {
                    state.pending_offers.clear();
                }
            }
            _ => {}
        }
    }

    // data_offer creates a server-owned proxy before its MIME and selection events arrive.
    wayland_client::event_created_child!(AppState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataSource, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type: _, fd } => {
                let text = state.clipboard_text.clone();
                std::thread::spawn(move || {
                    let mut file = std::fs::File::from(fd);
                    if let Some(text) = text {
                        let _ = file.write_all(text.as_bytes());
                    }
                });
            }
            wl_data_source::Event::Cancelled => {
                state.wayland.data_source = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Some(data) = state.pending_offers.iter_mut().find(|o| &o.offer == proxy) {
                data.mime_types.push(mime_type);
            } else if let Some(current) = &mut state.wayland.current_offer
                && &current.offer == proxy
            {
                current.mime_types.push(mime_type);
            }
        }
    }
}

impl AppState {
    /// Sets the clipboard content internally and offers it through the Wayland data device.
    pub fn set_clipboard_text(&mut self, text: String, qh: Option<&QueueHandle<Self>>) {
        self.terminal.set_clipboard_content(Some(text.clone()));
        self.clipboard_text = Some(text);

        if let (Some(qh), Some(manager), Some(device)) = (
            qh,
            &self.wayland.data_device_manager,
            &self.wayland.data_device,
        ) {
            let source = manager.create_data_source(qh, ());
            source.offer("text/plain;charset=utf-8".to_string());
            source.offer("text/plain".to_string());
            source.offer("UTF8_STRING".to_string());
            device.set_selection(Some(&source), self.last_serial);
            self.wayland.data_source = Some(source);
        }
    }

    /// Copies the currently selected text to the Wayland clipboard and internal buffer.
    pub fn copy_selection(&mut self, qh: Option<&QueueHandle<Self>>) {
        let text = self.selection.extract_text(&self.terminal.grid);
        if text.is_empty() {
            return;
        }

        self.set_clipboard_text(text, qh);
    }

    /// Pastes text from the Wayland clipboard into the terminal PTY.
    pub fn paste_clipboard(&mut self, conn: Option<&Connection>) {
        let bracketed = self.terminal.bracketed_paste;
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types)
                && let Ok((read_fd, write_fd)) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            {
                offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                drop(write_fd);

                if let Some(c) = conn {
                    let _ = c.flush();
                }

                if let Ok(pty_fd) = self.pty.try_clone_master() {
                    std::thread::spawn(move || {
                        // Limit paste payload to at most 10 MiB to prevent memory exhaustion
                        let mut reader = std::fs::File::from(read_fd).take(10 * 1024 * 1024);
                        let mut bytes = Vec::new();
                        if reader.read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                            let payload = if bracketed {
                                let mut wrapped = Vec::with_capacity(bytes.len() + 12);
                                wrapped.extend_from_slice(b"\x1b[200~");
                                wrapped.extend_from_slice(&bytes);
                                wrapped.extend_from_slice(b"\x1b[201~");
                                wrapped
                            } else {
                                bytes
                            };
                            // PTY master is nonblocking: write with poll readiness loop to avoid truncation
                            let mut to_write = &payload[..];
                            let start = std::time::Instant::now();
                            while !to_write.is_empty()
                                && start.elapsed() < std::time::Duration::from_secs(5)
                            {
                                let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                                if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                                    break;
                                }
                                match nix::unistd::write(&pty_fd, to_write) {
                                    Ok(0) => break,
                                    Ok(written) => to_write = &to_write[written..],
                                    Err(Errno::EINTR | Errno::EAGAIN) => continue,
                                    Err(_) => break,
                                }
                            }
                        }
                    });
                    return;
                }
            }
            return;
        }

        // Fallback to internal clipboard buffer if offer not available
        let fallback_text = self.clipboard_text.clone();
        if let Some(text) = fallback_text {
            let payload = if bracketed {
                let mut wrapped = Vec::with_capacity(text.len() + 12);
                wrapped.extend_from_slice(b"\x1b[200~");
                wrapped.extend_from_slice(text.as_bytes());
                wrapped.extend_from_slice(b"\x1b[201~");
                wrapped
            } else {
                text.into_bytes()
            };

            if payload.len() <= 4096 {
                self.write_pty_blocking(&payload);
            } else if let Ok(pty_fd) = self.pty.try_clone_master() {
                std::thread::spawn(move || {
                    let mut to_write = &payload[..];
                    let start = std::time::Instant::now();
                    while !to_write.is_empty()
                        && start.elapsed() < std::time::Duration::from_secs(5)
                    {
                        let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                        if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                            break;
                        }
                        match nix::unistd::write(&pty_fd, to_write) {
                            Ok(0) => break,
                            Ok(written) => to_write = &to_write[written..],
                            Err(Errno::EINTR | Errno::EAGAIN) => continue,
                            Err(_) => break,
                        }
                    }
                });
            } else {
                // If cloning PTY master failed, do not block the main event loop with an oversized write;
                // write bounded head chunk only.
                self.write_pty_blocking(&payload[..4096]);
            }
        }
    }
}

pub mod clipboard {
    pub use super::best_text_mime;
}

pub mod seat {
    pub use super::{find_url_at_col, find_url_in_grid, x11_button_index};
}

pub mod window {
    pub use super::WindowState;
}

pub mod init {}
pub mod text_input {}
pub mod xdg {}
