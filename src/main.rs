use std::os::unix::io::AsRawFd;
use std::process::exit;
use x11rb::{connect, connection::Connection};
use x11rb::cursor::Handle;
use x11rb::protocol::xproto::*;
use x11rb::protocol::{Event, present};

fn main() {
    if let Err(e) = run() {
        eprintln!("screentaker: {e}");
        exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // flock — protection from double launch
    let lock = format!("/tmp/screenshooter-{}.lock",
        std::env::var("DISPLAY").unwrap_or_default().replace(':', "_"));
    let lock_f = std::fs::File::create(&lock)?;
    if unsafe { libc::flock(lock_f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("already running".into());
    }

    let (conn, screen_num) = connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let w = screen.width_in_pixels;
    let h = screen.height_in_pixels;
    let depth = screen.root_depth;

    if depth < 24 {
        return Err("unsupported depth (< 24)".into());
    }

    let visual = screen.allowed_depths
        .iter().flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)
        .ok_or("root visual not found")?;
    let r_off = (visual.red_mask.trailing_zeros() / 8) as usize;
    let g_off = (visual.green_mask.trailing_zeros() / 8) as usize;
    let b_off = (visual.blue_mask.trailing_zeros() / 8) as usize;

    // Screenshot
    let img = get_image(&conn, ImageFormat::Z_PIXMAP, root, 0, 0, w, h, !0u32)?
        .reply()?;
    let bpp = 4usize;
    let stride = img.data.len() / h as usize;
    let original = img.data;
    let mut dark = original.clone();

    for y in 0..h as usize {
        for x in 0..w as usize {
            let o = y * stride + x * bpp;
            dark[o + r_off] = (dark[o + r_off] as u16 * 1 / 2) as u8;
            dark[o + g_off] = (dark[o + g_off] as u16 * 1 / 2) as u8;
            dark[o + b_off] = (dark[o + b_off] as u16 * 1 / 2) as u8;
        }
    }

    // Window
    let win = conn.generate_id()?;
    create_window(
        &conn, 0, win, root, 0, 0, w, h, 0,
        WindowClass::COPY_FROM_PARENT, 0,
        &CreateWindowAux::default()
            .override_redirect(1u32)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            ),
    )?.check()?;

    let opacity_atom = intern_atom(&conn, false, b"_NET_WM_WINDOW_OPACITY")?.reply()?.atom;
    change_property(
        &conn,
        PropMode::REPLACE,
        win,
        opacity_atom,
        AtomEnum::CARDINAL,
        32,
        1,
        &0xfffffffe_u32.to_ne_bytes(),
    )?.check()?;

    let gc = conn.generate_id()?;
    create_gc(&conn, gc, win, &CreateGCAux::default())?.check()?;

    // Background pixmap — dark frame (no black flash on map)
    let bg_pix = conn.generate_id()?;
    create_pixmap(&conn, depth, bg_pix, root, w, h)?.check()?;
    put_image(&conn, ImageFormat::Z_PIXMAP, bg_pix, gc, w, h, 0, 0, 0, depth, &dark)?.check()?;
    change_window_attributes(&conn, win, &ChangeWindowAttributesAux::default().background_pixmap(bg_pix))?
        .check()?;

    // Crosshair — themed cursor via Xcursor, fallback to font XC_crosshair
    let cursor = match (|| -> Result<_, Box<dyn std::error::Error>> {
        let db = x11rb::resource_manager::new_from_default(&conn)?;
        let handle = Handle::new(&conn, screen_num, &db)?.reply()?;
        Ok(handle.load_cursor(&conn, "crosshair")?)
    })() {
        Ok(c) => c,
        Err(_) => {
            let font = conn.generate_id()?;
            open_font(&conn, font, b"cursor")?.check()?;
            let c = conn.generate_id()?;
            create_glyph_cursor(&conn, c, font, font, 34, 35, 65535, 65535, 65535, 0, 0, 0)?
                .check()?;
            close_font(&conn, font)?.check()?;
            c
        }
    };
    change_window_attributes(&conn, win, &ChangeWindowAttributesAux::default().cursor(cursor))?
        .check()?;

    // Map, focus — in one batch, no round-trips (background already dark via bg_pix)
    map_window(&conn, win)?;
    set_input_focus(&conn, InputFocus::PARENT, win, 0u32)?;
    conn.flush()?;

    // Grab pointer (required for region selection)
    let ptr_mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
    let r = grab_pointer(&conn, false, win, ptr_mask, GrabMode::ASYNC, GrabMode::ASYNC, root, cursor, 0u32)?
        .reply()?;
    if r.status != GrabStatus::SUCCESS {
        return Err(format!("grab_pointer failed: {:?}", r.status).into());
    }

    // Try keyboard grab — if WM holds the grab (keybind still pressed), proceed without it
    grab_keyboard(&conn, false, win, 0u32, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()
        .map(|r| r.status == GrabStatus::SUCCESS)
        .ok();

    // Present vsync for initial frame if available
    let present_ok = query_extension(&conn, b"Present")?;
    let present_ok = present_ok.reply()?.present;

    if present_ok {
        let pixmap = conn.generate_id()?;
        create_pixmap(&conn, depth, pixmap, root, w, h)?.check()?;
        put_image(&conn, ImageFormat::Z_PIXMAP, pixmap, gc, w, h, 0, 0, 0, depth, &dark)?
            .check()?;
        present::pixmap(
            &conn, win, pixmap,
            0, 0, 0,
            0, 0,
            0, 0, 0,
            0,
            0, 0, 0,
            &[],
        )?.check()?;
    }
    conn.flush()?;

    struct Sel {
        active: bool,
        x1: i16, y1: i16, x2: i16, y2: i16,
    }
    let mut s = Sel { active: false, x1: 0, y1: 0, x2: 0, y2: 0 };

    loop {
        let event = if s.active {
            // During selection: polling without blocking
            if let Some(e) = conn.poll_for_event()? {
                Some(e)
            } else {
                // If no events, redraw current selection
                let (rx, ry, rw, rh) = norm(s.x1, s.y1, s.x2, s.y2);
                if rw > 0 && rh > 0 {
                    let mut frame = dark.clone();
                    for row in ry as usize..(ry + rh as i16) as usize {
                        let off = row * stride + rx as usize * bpp;
                        let len = rw as usize * bpp;
                        frame[off..off + len].copy_from_slice(&original[off..off + len]);
                    }
                    let _ = put_image(&conn, ImageFormat::Z_PIXMAP, win, gc, w, h, 0, 0, 0, depth, &frame);
                    let _ = conn.flush();
                }
                None
            }
        } else {
            Some(conn.wait_for_event()?)
        };
        
        let event = match event {
            Some(e) => e,
            None => continue,
        };
        match event {
            Event::Expose(_) => {
                let mut frame = dark.clone();
                if s.active {
                    let (rx, ry, rw, rh) = norm(s.x1, s.y1, s.x2, s.y2);
                    if rw > 0 && rh > 0 {
                        for row in ry as usize..(ry + rh as i16) as usize {
                            let off = row * stride + rx as usize * bpp;
                            let len = rw as usize * bpp;
                            frame[off..off + len].copy_from_slice(&original[off..off + len]);
                        }
                        //border(&mut frame, stride, rx, ry, rw, rh, bpp, r_off, g_off, b_off);
                    }
                }
                put_image(&conn, ImageFormat::Z_PIXMAP, win, gc, w, h, 0, 0, 0, depth, &frame)?
                    .check()?;
            }
            Event::ButtonPress(ev) => {
                if ev.detail != 1 { continue; }
                s = Sel { active: true, x1: ev.event_x, y1: ev.event_y, x2: ev.event_x, y2: ev.event_y };
            }
            Event::MotionNotify(ev) => {
                if !s.active { continue; }
                s.x2 = ev.event_x;
                s.y2 = ev.event_y;
            }
            Event::ButtonRelease(ev) => {
                if ev.detail != 1 { continue; }
                if !s.active { continue; }
                let (rx, ry, rw, rh) = norm(s.x1, s.y1, s.x2, s.y2);
                if rw > 0 && rh > 0 {
                    let mut rgba = Vec::with_capacity(rw as usize * rh as usize * 4);
                    for row in ry as usize..(ry + rh as i16) as usize {
                        for col in rx as usize..(rx + rw as i16) as usize {
                            let o = row * stride + col * bpp;
                            rgba.push(original[o + r_off]);
                            rgba.push(original[o + g_off]);
                            rgba.push(original[o + b_off]);
                            rgba.push(255);
                        }
                    }

                    let png_bytes = {
                        let img = image::RgbaImage::from_raw(rw as u32, rh as u32, rgba)
                            .expect("invalid image dimensions");
                        let mut cursor = std::io::Cursor::new(Vec::new());
                        image::DynamicImage::from(img)
                            .write_to(&mut cursor, image::ImageFormat::Png)?;
                        cursor.into_inner()
                    };

                    let _ = std::process::Command::new("xclip")
                        .args(["-selection", "clipboard", "-target", "image/png", "-i"])
                        .stdin(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .spawn()
                        .and_then(|mut child| {
                            child.stdin.as_mut()
                                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no stdin"))
                                .and_then(|stdin| std::io::Write::write_all(stdin, &png_bytes).map_err(Into::into))
                        });
                    
                    let _ = std::process::Command::new("notify-send")
                        .args(["Screenshot saved", &format!("{}×{} pixels copied to clipboard", rw, rh)])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                } else {
                    let _ = std::process::Command::new("notify-send")
                        .args(["Screenshot cancelled", "No selection made"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
                break;
            }
            Event::KeyPress(ev) => {
                let reply = get_keyboard_mapping(&conn, ev.detail, 1)?.reply()?;
                if reply.keysyms.first() == Some(&0xFF1B) { break; }
            }
            _ => {}
        }
        conn.flush()?;
    }

    exit(0);
}

fn norm(x1: i16, y1: i16, x2: i16, y2: i16) -> (i16, i16, u16, u16) {
    let x = x1.min(x2);
    let y = y1.min(y2);
    let w = (x1 - x2).unsigned_abs();
    let h = (y1 - y2).unsigned_abs();
    (x, y, w, h)
}

#[allow(dead_code)]
fn border(buf: &mut [u8], stride: usize, x: i16, y: i16, w: u16, h: u16, bpp: usize, r_off: usize, g_off: usize, b_off: usize) {
    let x = x as usize;
    let y = y as usize;
    let w = w as usize;
    let h = h as usize;
    let white = |buf: &mut [u8], off: usize| {
        buf[off + r_off] = 0xFF;
        buf[off + g_off] = 0xFF;
        buf[off + b_off] = 0xFF;
    };
    for col in x..x + w { white(buf, y * stride + col * bpp); }
    if h > 1 {
        for col in x..x + w { white(buf, (y + h - 1) * stride + col * bpp); }
    }
    if h > 2 {
        for row in (y + 1)..(y + h - 1) { white(buf, row * stride + x * bpp); }
    }
    if w > 1 && h > 2 {
        for row in (y + 1)..(y + h - 1) { white(buf, row * stride + (x + w - 1) * bpp); }
    }
}
