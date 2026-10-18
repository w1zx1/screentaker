// Wayland (KDE) backend. The frozen frame is captured through KWin's
// screenShot2 D-Bus interface (src/dbus.rs); the interactive overlay is native:
// layer-shell surfaces per output in logical pixels, crosshair cursor,
// drag-select brightens the region, Esc cancels. Dragging rewrites only the
// union strip of the old and new selections, not the whole overlay.

use std::{
    error::Error,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr,
};

use crate::{notify, png_bytes, Img};
use wayland_client::{
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::WlDataOffer,
        wl_data_source::{self, WlDataSource},
        wl_keyboard::{self, KeyState, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

type Qh = QueueHandle<State>;
type Res<T> = Result<T, Box<dyn Error>>;

const BTN_LEFT: u32 = 0x110;
// KWin reports wl_keyboard keys as raw evdev codes, so Escape arrives as 1, not
// as the XKB key code 9 the keymap wording suggests — that one is the '8' key
// here. Escape is the only key this overlay listens to, so no keymap is needed.
const KEY_ESC: u32 = 1;

#[derive(Clone, Copy)]
struct Sel {
    x1: i32, y1: i32, x2: i32, y2: i32,
}

impl Sel {
    fn left(&self) -> i32 { self.x1.min(self.x2) }
    fn top(&self) -> i32 { self.y1.min(self.y2) }
    fn right(&self) -> i32 { self.x1.max(self.x2) }
    fn bottom(&self) -> i32 { self.y1.max(self.y2) }
    fn empty(&self) -> bool { self.x1 == self.x2 || self.y1 == self.y2 }
}

// ---- frozen frame -------------------------------------------------------

// ---- shm ----------------------------------------------------------------

struct ShmMem {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

impl ShmMem {
    // anonymous shared memory: no /dev/shm file is ever created
    fn alloc(len: usize) -> Res<ShmMem> {
        let fd = unsafe { libc::memfd_create(c"screentaker".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(Box::new(std::io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(Box::new(std::io::Error::last_os_error()));
        }
        let mmap = unsafe {
            libc::mmap(ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if mmap == libc::MAP_FAILED {
            return Err(Box::new(std::io::Error::last_os_error()));
        }
        Ok(ShmMem { fd, ptr: mmap as *mut u8, len })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

// ---- per-output state ---------------------------------------------------

struct Out {
    output: WlOutput,
    // logical geometry
    pos: (i32, i32),
    logical: Option<(u32, u32)>, // from the layer configure
    coff: (u32, u32),            // this output's origin on the canvas, device px
    // overlay: dark background in overlay format, bytes [b,g,r,a]
    dark: Option<Vec<u8>>,
    pool: Option<ShmMem>, // 2 overlay pages
    ovbufs: Option<[WlBuffer; 2]>,
    busy: [bool; 2],
    front: usize,
    // rows painted by the previous redraw; a buffer being (re)used is stale for
    // exactly the changes since its last show, i.e. the previous band plus the
    // current one. Repainting both keeps every buffer fully correct, which the
    // compositor's per-buffer cache of damaged rows then mirrors exactly.
    prev: Option<(u32, u32)>,
    band: Option<(u32, u32)>, // dirty logical rows (local)
    dirty: bool,
    surface: Option<WlSurface>,
    lsurf: Option<ZwlrLayerSurfaceV1>,
}

impl Out {
    fn new(output: WlOutput) -> Out {
        Out {
            output,
            pos: (0, 0),
            logical: None,
            coff: (0, 0),
            dark: None,
            pool: None,
            ovbufs: None,
            busy: [false; 2],
            front: 0,
            prev: None,
            band: None,
            dirty: false,
            surface: None,
            lsurf: None,
        }
    }
}

enum Done {
    Shot(Sel),
    Cancel,
}

#[derive(Default)]
struct State {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    lshell: Option<ZwlrLayerShellV1>,
    curmgr: Option<WpCursorShapeManagerV1>,
    cursdev: Option<WpCursorShapeDeviceV1>,
    pointer: Option<WlPointer>,
    seat: Option<WlSeat>,
    datamgr: Option<WlDataDeviceManager>,
    serial: u32, // of the input event that finished the selection
    clip: Vec<(String, Vec<u8>)>, // what we offer on paste (served by the forked child)
    clip_src: Option<WlDataSource>,
    clip_dev: Option<WlDataDevice>,
    clip_stop: bool,
    clip_served: bool,
    frame: Option<Img>,
    kx: f64, // logical → canvas scale, set in setup_overlays()
    ky: f64,
    outputs: Vec<Out>,
    on_surface: Option<usize>,
    cursor: (i32, i32),
    press: Option<(i32, i32)>,
    sel: Option<Sel>,
    done: Option<Done>,
}

// ---- entry point --------------------------------------------------------

impl State {
    fn out_idx_surface(&self, s: &WlSurface) -> Option<usize> {
        self.outputs.iter().position(|o| o.surface.as_ref() == Some(s))
    }
}

pub fn run() -> Res<()> {
    // Capture first: the overlay only opens once the frozen frame exists.
    let frame = crate::dbus::workspace_frame()?;

    let conn = Connection::connect_to_env()?;
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State::default();

    eq.roundtrip(&mut st)?; // globals arrive, bound in the registry handler
    eq.roundtrip(&mut st)?; // wl_output geometry + seat capabilities

    for (name, ok) in [
        ("wl_compositor", st.compositor.is_some()),
        ("wl_shm", st.shm.is_some()),
        ("zwlr_layer_shell_v1", st.lshell.is_some()),
    ] {
        if !ok {
            return Err(format!("compositor does not support {name}").into());
        }
    }
    if st.outputs.is_empty() {
        return Err("no outputs".into());
    }
    st.frame = Some(frame);

    // overlay: one layer surface per output
    let n = st.outputs.len();
    for i in 0..n {
        let surface = st.compositor.as_ref().unwrap().create_surface(&qh, ());
        let lsurf: ZwlrLayerSurfaceV1 = st.lshell.as_ref().unwrap().get_layer_surface(
            &surface,
            Some(&st.outputs[i].output),
            Layer::Overlay,
            // KWin turns this string into the window type of the layer surface
            // (scopeToType() in layershellv1window.cpp), and the type decides
            // which desktop effects animate the window. The open/close animation
            // effects only touch normal and dialog windows; the fading-popups
            // effect fades the on-screen-display, splash and tooltip types.
            // "utility" is the one type neither effect animates, so the overlay
            // pops up and goes away instantly, as a picker should.
            "utility".to_string(),
            &qh,
            i,
        );
        lsurf.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        lsurf.set_exclusive_zone(-1);
        if i == 0 {
            // Esc comes through this one
            lsurf.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        }
        surface.commit(); // triggers configure
        st.outputs[i].surface = Some(surface);
        st.outputs[i].lsurf = Some(lsurf);
    }

    // configure events carry the logical sizes; overlay buffers need them all
    while st.outputs.iter().any(|o| o.logical.is_none()) {
        eq.blocking_dispatch(&mut st)?;
    }
    st.setup_overlays(&qh)?;

    for i in 0..n {
        st.redraw(i); // initial dark frame
    }

    // selection loop
    while st.done.is_none() {
        eq.blocking_dispatch(&mut st)?;
        for i in 0..n {
            st.redraw(i);
        }
    }

    st.finish_run(&qh, &conn, &mut eq);
    Ok(())
}

impl State {
    // ---- canvas setup ---------------------------------------------------

    // Map every output onto the spectacle canvas (device pixel bounding box)
    // and build the overlay shm pool of each.
    fn setup_overlays(&mut self, qh: &Qh) -> Res<()> {
        let fr = self.frame.as_ref().ok_or("no captured frame")?;
        let (bx0, by0) = self.outputs.iter().fold((i32::MAX, i32::MAX), |(x, y), o| {
            (x.min(o.pos.0), y.min(o.pos.1))
        });
        let (bx1, by1) = self.outputs.iter().fold((i32::MIN, i32::MIN), |(x, y), o| {
            let l = o.logical.unwrap_or((0, 0));
            (x.max(o.pos.0 + l.0 as i32), y.max(o.pos.1 + l.1 as i32))
        });
        self.kx = fr.w as f64 / ((bx1 - bx0).max(1) as f64);
        self.ky = fr.h as f64 / ((by1 - by0).max(1) as f64);
        for o in &mut self.outputs {
            o.coff = (
                ((o.pos.0 - bx0) as f64 * self.kx).round() as u32,
                ((o.pos.1 - by0) as f64 * self.ky).round() as u32,
            );
        }
        for i in 0..self.outputs.len() {
            self.setup_overlay(i, qh)?;
        }
        Ok(())
    }

    // ---- rendering ------------------------------------------------------

    // Dark background in overlay format, sampled down from the native canvas.
    fn compute_dark(&self, i: usize, lw: u32, lh: u32) -> Res<Vec<u8>> {
        let fr = self.frame.as_ref().ok_or("no captured frame")?;
        let o = &self.outputs[i];
        let (kx, ky) = (self.kx, self.ky);
        let (fw, fh) = (fr.w as usize, fr.h as usize);
        let src = &fr.rgba;
        let stride = lw as usize * 4;
        let mut dark = vec![0u8; stride * lh as usize];
        for ly in 0..lh as usize {
            let cy = ((o.coff.1 as f64 + ly as f64 * ky) as usize).min(fh - 1);
            for lx in 0..lw as usize {
                let cx = ((o.coff.0 as f64 + lx as f64 * kx) as usize).min(fw - 1);
                let s = (cy * fw + cx) * 4;
                let d = ly * stride + lx * 4;
                dark[d] = src[s + 2] >> 1; // b
                dark[d + 1] = src[s + 1] >> 1; // g
                dark[d + 2] = src[s] >> 1; // r
                dark[d + 3] = 0xFF;
            }
        }
        Ok(dark)
    }

    fn redraw(&mut self, i: usize) {
        // snapshot everything we need from immutable borrows, then mutate
        let Some(frame) = self.frame.as_ref() else { return };
        let o = &self.outputs[i];
        if !o.dirty {
            return;
        }
        let (Some(pool), Some(ovbufs), Some((lw, lh)), Some(dark)) =
            (o.pool.as_ref(), o.ovbufs.as_ref(), o.logical, o.dark.as_ref())
        else {
            return;
        };
        let pick = o.front ^ 1;
        if o.busy[pick] {
            return; // the compositor still reads the only buffer safe to touch
        }
        let stride = lw as usize * 4;
        let (b0, b1) = if let Some((a, b)) = o.band { (a.min(b), a.max(b)) } else { (0, lh) };
        // the reused buffer is stale for everything that changed since its last
        // show: this band plus the previous one; repaint that union so the buffer
        // is whole again (see `prev`).
        let (y0, y1) = if let Some((a, b)) = o.prev {
            (b0.min(a.min(b)), b1.max(a.max(b)))
        } else {
            (b0, b1)
        };
        let (ox, oy) = o.pos;
        let (coffx, coffy) = o.coff;
        let sel = self.sel;
        let (kx, ky) = (self.kx, self.ky);
        let (fw, fh) = (frame.w as usize, frame.h as usize);
        let rgba = frame.rgba.as_ptr();
        let dark_ptr = dark.as_ptr();
        let page_ptr = pool.ptr;
        let surface = o.surface.as_ref().unwrap().clone();
        let buf = ovbufs[pick].clone();

        unsafe {
            let page = page_ptr.add(pick * stride * lh as usize);
            for ly in y0 as usize..y1 as usize {
                let row = ly * stride;
                ptr::copy_nonoverlapping(dark_ptr.add(row), page.add(row), stride);
            }
            if let Some(s) = sel {
                let x0 = (s.left() - ox).clamp(0, lw as i32) as usize;
                let x1 = (s.right() - ox).clamp(0, lw as i32) as usize;
                if x1 > x0 {
                    for ly in y0 as i32..y1 as i32 {
                        let gy = oy + ly;
                        if gy < s.top() || gy >= s.bottom() {
                            continue;
                        }
                        let row = (ly as usize) * stride;
                        let cy = ((coffy as f64 + ly as f64 * ky) as usize).min(fh - 1);
                        for lx in x0..x1 {
                            let cx = ((coffx as f64 + lx as f64 * kx) as usize).min(fw - 1);
                            let src = rgba.add((cy * fw + cx) * 4);
                            let d = page.add(row + lx * 4);
                            *d = *src.add(2); // b
                            *d.add(1) = *src.add(1); // g
                            *d.add(2) = *src; // r
                            *d.add(3) = 0xFF;
                        }
                    }
                }
            }
        }

        let o = &mut self.outputs[i];
        o.busy[pick] = true;
        o.front = pick;
        o.prev = Some((b0, b1));
        o.band = None;
        o.dirty = false;
        surface.damage_buffer(0, y0 as i32, lw as i32, (y1 - y0) as i32);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
    }

    // crop the selection at native (canvas) resolution
    fn crop(&self, sel: Sel) -> Option<(u32, u32, Vec<u8>)> {
        let fr = self.frame.as_ref()?;
        let rw = sel.right() - sel.left();
        let rh = sel.bottom() - sel.top();
        if rw == 0 || rh == 0 {
            return None;
        }
        let cw = ((rw as f64 * self.kx).round() as u32).max(1);
        let ch = ((rh as f64 * self.ky).round() as u32).max(1);
        let mut rgba = Vec::with_capacity(cw as usize * ch as usize * 4);
        for ly in 0..ch {
            let gy = sel.top() + ((ly as f64 / self.ky).round() as i32);
            for lx in 0..cw {
                let gx = sel.left() + ((lx as f64 / self.kx).round() as i32);
                // global logical point → canvas pixel, black outside every output
                let px = self
                    .outputs
                    .iter()
                    .find_map(|o| {
                        let l = o.logical?;
                        let (px, py) = (gx - o.pos.0, gy - o.pos.1);
                        if px < 0 || py < 0 || px >= l.0 as i32 || py >= l.1 as i32 {
                            return None;
                        }
                        let cx = ((o.coff.0 as f64 + px as f64 * self.kx) as usize).min(fr.w as usize - 1);
                        let cy = ((o.coff.1 as f64 + py as f64 * self.ky) as usize).min(fr.h as usize - 1);
                        let s = (cy * fr.w as usize + cx) * 4;
                        Some([fr.rgba[s], fr.rgba[s + 1], fr.rgba[s + 2], 255])
                    })
                    .unwrap_or([0, 0, 0, 255]);
                rgba.extend_from_slice(&px);
            }
        }
        Some((cw, ch, rgba))
    }

    // ---- overlay setup --------------------------------------------------

    fn setup_overlay(&mut self, i: usize, qh: &Qh) -> Res<()> {
        let (lw, lh) = self.outputs[i].logical.unwrap_or((1, 1));
        let dark = self.compute_dark(i, lw, lh)?;
        let total = dark.len() * 2;
        let mut mem = ShmMem::alloc(total)?;
        mem.bytes_mut()[..dark.len()].copy_from_slice(&dark);
        mem.bytes_mut()[dark.len()..].copy_from_slice(&dark);

        let pool = self.shm.as_ref().unwrap().create_pool(mem.fd.as_fd(), total as i32, qh, ());
        let stride = lw as usize * 4;
        let b0 = pool.create_buffer(0, lw as i32, lh as i32, stride as i32, Format::Argb8888, qh, i);
        let b1 = pool.create_buffer(dark.len() as i32, lw as i32, lh as i32, stride as i32, Format::Argb8888, qh, i);

        let o = &mut self.outputs[i];
        o.dark = Some(dark);
        o.pool = Some(mem);
        o.ovbufs = Some([b0, b1]);
        o.busy = [false, false];
        o.front = 0;
        o.prev = None;
        o.band = None;
        o.dirty = true; // initial full paint happens through redraw()
        Ok(())
    }

    // ---- input ----------------------------------------------------------

    fn on_motion(&mut self, gx: i32, gy: i32) {
        self.cursor = (gx, gy);
        let Some(press) = self.press else { return };
        let new = Sel { x1: press.0, y1: press.1, x2: gx, y2: gy };
        let (old_top, old_bot) = match self.sel {
            Some(s) => (s.top(), s.bottom()),
            None => (gy, gy),
        };
        let top = old_top.min(new.top());
        let bot = old_bot.max(new.bottom());
        for o in self.outputs.iter_mut() {
            let Some(l) = o.logical else { continue };
            let y0 = (top - o.pos.1).clamp(0, l.1 as i32) as u32;
            let y1 = (bot + 1 - o.pos.1).clamp(0, l.1 as i32) as u32;
            if y1 <= y0 {
                continue;
            }
            o.band = Some(match o.band {
                Some((a, b)) => (a.min(y0), b.max(y1)),
                None => (y0, y1),
            });
            o.dirty = true;
        }
        self.sel = Some(new);
    }

    // ---- finishing ------------------------------------------------------

    // Take ownership of the clipboard and tear the overlay down. The pixels stay
    // in self.clip for the wl_data_source Send handler that answers pastes.
    fn serve_clipboard(&mut self, png: Vec<u8>, qh: &Qh, conn: &Connection) -> Res<()> {
        let mgr = self.datamgr.clone().ok_or("no wl_data_device_manager")?;
        let seat = self.seat.clone().ok_or("no wl_seat")?;
        let dev = mgr.get_data_device(&seat, qh, ());
        let src = mgr.create_data_source(qh, ());
        // Klipper (plasmashell) drops any selection it cannot classify: it wants
        // text, a URL or a Qt-recognised image. An item carrying non-empty text
        // is listed as plain text, never as an image, so the text/plain
        // companion stays empty — the shape Spectacle's copies have. That keeps
        // the shot out of the "unknown" bin while the history still renders it as
        // an image with a thumbnail.
        let offers = vec![("image/png".to_string(), png), ("text/plain".to_string(), Vec::new())];
        for (mime, _) in &offers {
            src.offer(mime.clone());
        }
        // The serial of the releasing click makes the compositor accept us.
        dev.set_selection(Some(&src), self.serial);
        for o in &mut self.outputs {
            if let Some(l) = o.lsurf.take() {
                l.destroy();
            }
            if let Some(s) = o.surface.take() {
                s.destroy();
            }
        }
        self.clip = offers;
        self.clip_dev = Some(dev);
        self.clip_src = Some(src);
        conn.flush()?;
        Ok(())
    }

    // Deliver the PNG to the clipboard, then hand the selection to a background
    // copy of ourselves and let this process return to the shell.
    fn finish_run(&mut self, qh: &Qh, conn: &Connection, eq: &mut EventQueue<State>) {
        let shot = match self.done {
            Some(Done::Shot(sel)) => self.crop(sel),
            _ => None,
        };
        let Some((w, h, rgba)) = shot else {
            notify("Screenshot cancelled", "No selection made");
            return;
        };
        let png = png_bytes(w, h, rgba);
        if let Err(e) = self.serve_clipboard(png, qh, conn) {
            notify("Screenshot failed", &format!("clipboard unavailable: {e}"));
            return;
        }
        // KWin cancels a data source whose serial it considers stale, so make sure
        // the selection survived before telling the user it is on the clipboard.
        if eq.roundtrip(self).is_err() || (self.clip_stop && !self.clip_served) {
            notify("Screenshot failed", "the compositor rejected the clipboard selection");
            return;
        }
        notify("Screenshot saved", &format!("{w}×{h} pixels copied to clipboard"));

        // fork() keeps the same Wayland connection — and therefore the input
        // serial the compositor validated — alive, which is why a plain child
        // process can keep serving the selection after we exit.
        unsafe {
            if libc::fork() == 0 {
                self.frame = None; // the frozen pixels are no longer needed
                for o in &mut self.outputs {
                    // drop the overlay buffers; only the PNG stays in memory
                    if let Some(bufs) = o.ovbufs.take() {
                        for b in bufs {
                            b.destroy();
                        }
                    }
                    o.pool = None;
                    o.dark = None;
                }
                libc::setsid(); // outlive the terminal that started us
                let lfd = crate::LOCK_FD.load(std::sync::atomic::Ordering::Relaxed);
                if lfd >= 0 {
                    libc::close(lfd); // release the single-instance lock
                }
                let dev_null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if dev_null >= 0 {
                    for fd in 0..3 {
                        libc::dup2(dev_null, fd);
                    }
                    if dev_null > 2 {
                        libc::close(dev_null);
                    }
                }
                while !self.clip_stop && eq.blocking_dispatch(self).is_ok() {}
            }
            libc::_exit(0); // neither process touches the shared connection again
        }
    }
}

// The frozen frame comes from src/dbus.rs (KWin ScreenShot2); here it is only
// consumed by the overlay and the crop.

// ---- dispatch impls -------------------------------------------------------

impl Dispatch<WlRegistry, ()> for State {
    fn event(state: &mut Self, reg: &WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &Qh) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_output" => {
                let output: WlOutput = reg.bind(name, version.min(4), qh, state.outputs.len());
                state.outputs.push(Out::new(output));
            }
            "wl_compositor" => {
                state.compositor = Some(reg.bind::<WlCompositor, _, _>(name, version.min(5), qh, ()));
            }
            "wl_shm" => {
                state.shm = Some(reg.bind::<WlShm, _, _>(name, version.min(2), qh, ()));
            }
            "zwlr_layer_shell_v1" => {
                state.lshell = Some(reg.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ()));
            }
            "wp_cursor_shape_manager_v1" => {
                state.curmgr = Some(reg.bind::<WpCursorShapeManagerV1, _, _>(name, version.min(1), qh, ()));
            }
            "wl_seat" => {
                state.seat = Some(reg.bind::<WlSeat, _, _>(name, version.min(9), qh, ()));
            }
            "wl_data_device_manager" => {
                state.datamgr = Some(reg.bind::<WlDataDeviceManager, _, _>(name, version.min(3), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, i: &usize, _: &Connection, _: &Qh) {
        if let (Some(o), wl_output::Event::Geometry { x, y, .. }) = (state.outputs.get_mut(*i), event) {
            o.pos = (x, y);
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &Qh) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = event else { return };
        if c.contains(Capability::Pointer) {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
        if c.contains(Capability::Keyboard) {
            seat.get_keyboard(qh, ()); // Esc only; keymap ignored
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, qh: &Qh) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                let i = state.out_idx_surface(&surface);
                if let Some(i) = i {
                    // the crosshair device is created on first entry, by which
                    // time both the pointer and the cursor-shape manager exist
                    if state.cursdev.is_none() {
                        if let (Some(mgr), Some(ptr)) = (state.curmgr.as_ref(), state.pointer.as_ref()) {
                            state.cursdev = Some(mgr.get_pointer(ptr, qh, ()));
                        }
                    }
                    if let Some(c) = state.cursdev.as_ref() {
                        c.set_shape(serial, Shape::Crosshair);
                    }
                    state.on_surface = Some(i);
                    let (ox, oy) = state.outputs[i].pos;
                    state.cursor = (ox + surface_x.round() as i32, oy + surface_y.round() as i32);
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                let Some(i) = state.on_surface else { return };
                let (ox, oy) = state.outputs[i].pos;
                state.on_motion(ox + surface_x.round() as i32, oy + surface_y.round() as i32);
            }
            wl_pointer::Event::Leave { .. } => {
                state.on_surface = None;
            }
            wl_pointer::Event::Button { serial, button, state: WEnum::Value(s), .. } => {
                if button != BTN_LEFT {
                    return;
                }
                state.serial = serial;
                match s {
                    ButtonState::Pressed => {
                        let p = state.cursor;
                        state.press = Some(p);
                        state.sel = Some(Sel { x1: p.0, y1: p.1, x2: p.0, y2: p.1 });
                    }
                    ButtonState::Released if state.press.take().is_some() => {
                        let sel = state.sel.unwrap();
                        state.done = Some(if sel.empty() { Done::Cancel } else { Done::Shot(sel) });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &Qh) {
        let wl_keyboard::Event::Key { key, state: WEnum::Value(KeyState::Pressed), .. } = event else { return };
        if key == KEY_ESC {
            state.done = Some(Done::Cancel);
        }
    }
}

impl Dispatch<WlBuffer, usize> for State {
    fn event(state: &mut Self, buf: &WlBuffer, event: wl_buffer::Event, _: &usize, _: &Connection, _: &Qh) {
        let wl_buffer::Event::Release = event else { return };
        for o in &mut state.outputs {
            if let Some(b) = o.ovbufs.as_ref() {
                for (k, bb) in b.iter().enumerate() {
                    if bb == buf {
                        o.busy[k] = false;
                    }
                }
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(state: &mut Self, ls: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, i: &usize, _: &Connection, _: &Qh) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                ls.ack_configure(serial);
                if width > 0 && height > 0 {
                    state.outputs[*i].logical.get_or_insert((width, height));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.done = Some(Done::Cancel);
            }
            _ => {}
        }
    }
}
impl Dispatch<WlDataDevice, ()> for State {
    // The compositor announces offered clipboard contents here (and in the
    // wl_data_source events). We only ever *own* the selection, so there is
    // nothing to read — but wl_data_device.data_offer carries a new_id, so the
    // child object has to be creatable or wayland-client aborts the dispatch.
    fn event(_: &mut Self, _: &WlDataDevice, _: wl_data_device::Event, _: &(), _: &Connection, _: &Qh) {}

    wayland_client::event_created_child!(State, WlDataDevice, [
        0 => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataSource, ()> for State {
    fn event(state: &mut Self, _: &WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &Qh) {
        match event {
            // a client is pasting: hand it the payload we offered for that type
            wl_data_source::Event::Send { mime_type, fd } => {
                if let Some((_, payload)) = state.clip.iter().find(|(mime, _)| *mime == mime_type) {
                    let mut file = std::fs::File::from(fd);
                    let _ = std::io::Write::write_all(&mut file, payload);
                    state.clip_served = true;
                }
            }
            // someone else took the selection: our job is done
            wl_data_source::Event::Cancelled => state.clip_stop = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore WlCompositor);
wayland_client::delegate_noop!(State: ignore WlDataDeviceManager);
wayland_client::delegate_noop!(State: ignore WlDataOffer);
wayland_client::delegate_noop!(State: ignore WlShm); // advertises its formats
wayland_client::delegate_noop!(State: ignore WlShmPool);
wayland_client::delegate_noop!(State: ignore WlSurface);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);
wayland_client::delegate_noop!(State: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(State: ignore WpCursorShapeDeviceV1);
