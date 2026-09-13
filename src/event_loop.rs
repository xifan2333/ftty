//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, Capability, WlSeat},
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::input::KeyboardHandler;
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::wayland::WaylandState;

/// Shared application state passed to all calloop sources and Wayland event dispatches.
pub struct AppState {
    pub terminal: Terminal,
    pub pty: Pty,
    pub keyboard: KeyboardHandler,
    pub wayland: WaylandState,
    pub running: bool,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl AppState {
    #[must_use]
    pub fn new(terminal: Terminal, pty: Pty) -> Self {
        Self {
            terminal,
            pty,
            keyboard: KeyboardHandler::new(),
            wayland: WaylandState::new(),
            running: true,
            cell_width: 9,
            cell_height: 18,
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland Dispatch Implementations
// ---------------------------------------------------------------------------

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
            version: _,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let comp = registry.bind::<WlCompositor, _, _>(name, 4, qh, ());
                    state.wayland.compositor = Some(comp);
                    state.wayland.init_window(qh);
                }
                "xdg_wm_base" => {
                    let xdg = registry.bind::<XdgWmBase, _, _>(name, 1, qh, ());
                    state.wayland.xdg_wm_base = Some(xdg);
                    state.wayland.init_window(qh);
                }
                "wl_seat" => {
                    let seat = registry.bind::<WlSeat, _, _>(name, 5, qh, ());
                    state.wayland.seat = Some(seat);
                }
                "wl_shm" => {
                    let shm = registry.bind::<WlShm, _, _>(name, 1, qh, ());
                    state.wayland.shm = Some(shm);
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
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.wayland.configured = true;

            // Attach initial buffer so the compositor displays the window immediately
            if let Some(surface) = &state.wayland.surface
                && let Some(shm) = &state.wayland.shm
            {
                let w = (state.wayland.width as i32).max(100);
                let h = (state.wayland.height as i32).max(100);
                if let Ok(buffer) = create_shm_buffer(shm, w, h, qh) {
                    if let Some(old) = state.wayland.current_buffer.take() {
                        old.destroy();
                    }
                    surface.attach(Some(&buffer), 0, 0);
                    surface.damage_buffer(0, 0, w, h);
                    surface.commit();
                    state.wayland.current_buffer = Some(buffer);
                }
            }
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
                if width > 0 && height > 0 {
                    state.wayland.width = width as u32;
                    state.wayland.height = height as u32;

                    let cols = (width as u32 / state.cell_width).max(1) as u16;
                    let rows = (height as u32 / state.cell_height).max(1) as u16;

                    state.terminal.grid.resize(cols as usize, rows as usize);
                    let _ = state.pty.resize(cols, rows);
                }
            }
            xdg_toplevel::Event::Close => {
                state.wayland.close_requested = true;
                state.running = false;
            }
            _ => {}
        }
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
            && caps.contains(Capability::Keyboard)
            && state.wayland.keyboard.is_none()
        {
            let keyboard = proxy.get_keyboard(qh, ());
            state.wayland.keyboard = Some(keyboard);
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
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: _,
                fd,
                size,
            } => unsafe {
                state
                    .keyboard
                    .set_keymap_from_fd(fd.as_raw_fd(), size as usize);
            },
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(KeyState::Pressed),
                ..
            } => {
                if let Some(bytes) = state.keyboard.handle_key(key) {
                    let _ = state.pty.write_all(&bytes);
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

fn create_shm_buffer<D>(
    shm: &WlShm,
    width: i32,
    height: i32,
    qh: &QueueHandle<D>,
) -> io::Result<WlBuffer>
where
    D: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
{
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};

    // Bound and sanitize dimensions to prevent overflow and absurd memory requests
    let width = (width.clamp(100, 8192)) as usize;
    let height = (height.clamp(100, 8192)) as usize;

    let stride = width
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "width overflow"))?;
    let size = stride
        .checked_mul(height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "size overflow"))?;

    let fd = unsafe { libc::memfd_create(c"ftty-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    unsafe {
        if libc::ftruncate(fd, size as libc::off_t) < 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }

        // Fill buffer with dark charcoal background color (#181818)
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr != libc::MAP_FAILED {
            let slice = std::slice::from_raw_parts_mut(ptr.cast::<u32>(), size / 4);
            slice.fill(0xFF18_1818);
            libc::munmap(ptr, size);
        }
    }

    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let pool = shm.create_pool(owned_fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();

    Ok(buffer)
}

// ---------------------------------------------------------------------------
// Unified Event Loop Runner
// ---------------------------------------------------------------------------

/// Runs the unified calloop event loop until terminal exits.
///
/// # Errors
/// Returns [`io::Error`] if Wayland connection, calloop initialization, or event dispatching fails.
pub fn run_event_loop(mut app_state: AppState) -> io::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

    let event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut event_loop: EventLoop<AppState> = EventLoop::try_new().map_err(io::Error::other)?;

    // 1. Wayland Event Source
    let wayland_source = WaylandSource::new(conn.clone(), event_queue);
    event_loop
        .handle()
        .insert_source(wayland_source, |(), queue, state: &mut AppState| {
            queue.dispatch_pending(state)
        })
        .map_err(io::Error::other)?;

    // 2. PTY Master Read Event Source
    let pty_master = app_state.pty.try_clone_master()?;
    let pty_source = Generic::new(pty_master, Interest::READ, Mode::Level);
    event_loop
        .handle()
        .insert_source(pty_source, |_event, _fd, state: &mut AppState| {
            let mut buf = [0u8; 8192];
            match state.pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    state.terminal.advance_bytes(&buf[..n]);
                    Ok(calloop::PostAction::Continue)
                }
                Ok(_) => {
                    // EOF on PTY master
                    state.running = false;
                    Ok(calloop::PostAction::Reregister)
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    Ok(calloop::PostAction::Continue)
                }
                Err(_) => {
                    // Child process likely exited (EIO on Linux PTY)
                    state.running = false;
                    Ok(calloop::PostAction::Reregister)
                }
            }
        })
        .map_err(io::Error::other)?;

    // 3. POSIX Signals Event Source
    let signals = Signals::new(&[
        Signal::SIGCHLD,
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGUSR1,
    ])
    .map_err(io::Error::other)?;
    event_loop
        .handle()
        .insert_source(signals, |event, _, state: &mut AppState| {
            match event.signal() {
                Signal::SIGCHLD => {
                    if !state.pty.is_alive() {
                        state.running = false;
                    }
                }
                Signal::SIGINT | Signal::SIGTERM => {
                    state.running = false;
                }
                Signal::SIGUSR1 => {
                    // Hook for future theme dynamic reload
                }
                _ => {}
            }
        })
        .map_err(io::Error::other)?;

    // 4. Main Event Loop Tick
    while app_state.running {
        event_loop
            .dispatch(Some(Duration::from_millis(16)), &mut app_state)
            .map_err(io::Error::other)?;

        let _ = conn.flush();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_initialization() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some("/bin/sh"), 80, 24).expect("PTY spawn");
        let app = AppState::new(term, pty);

        assert!(app.running);
        assert_eq!(app.terminal.grid.cols, 80);
        assert_eq!(app.terminal.grid.rows, 24);
        assert_eq!(app.cell_width, 9);
        assert_eq!(app.cell_height, 18);
        assert!(!app.wayland.configured);
    }

    #[test]
    fn test_pty_and_terminal_roundtrip() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some("/bin/sh"), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty);

        // Send a command to shell via PTY
        use std::io::{Read, Write};
        app.pty
            .write_all(b"echo ftty_test_ok\n")
            .expect("write to pty");

        // Wait briefly and read output back into terminal
        let mut received = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let mut buf = [0u8; 1024];
            if let Ok(n) = app.pty.read(&mut buf)
                && n > 0
            {
                app.terminal.advance_bytes(&buf[..n]);
                let full_screen: String = app
                    .terminal
                    .grid
                    .lines
                    .iter()
                    .flat_map(|r| r.cells.iter().map(|c| c.c))
                    .collect();
                if full_screen.contains("ftty_test_ok") {
                    received = true;
                    break;
                }
            }
        }
        assert!(received, "Expected shell echo in terminal grid");
    }
}
