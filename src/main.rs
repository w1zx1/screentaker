mod dbus;
mod wayland;
mod x11;

use std::os::unix::io::AsRawFd;
use std::process::exit;
use std::sync::atomic::{AtomicI32, Ordering};

// Whole-desktop frozen frame: device-pixel bounding box, straight RGBA order
// (normalized by the capture backend).
pub struct Img {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

// Single-instance flock, so the Wayland backend can drop it in its forked
// clipboard holder (otherwise the lock would outlive the interactive run).
pub static LOCK_FD: AtomicI32 = AtomicI32::new(-1);

fn main() {
    if let Err(e) = run() {
        eprintln!("screentaker: {e}");
        exit(1);
    }
}

// ---- helpers shared by both backends ------------------------------------

pub fn notify(title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args([title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// Straight-RGBA pixels → PNG.
pub fn png_bytes(w: u32, h: u32, rgba: Vec<u8>) -> Vec<u8> {
    let img = image::RgbaImage::from_raw(w, h, rgba).expect("invalid image dimensions");
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::from(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .expect("png encoding");
    cursor.into_inner()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // flock — protection from double launch (session-wide)
    let lock = format!("/tmp/screenshooter-{}.lock",
        std::env::var("WAYLAND_DISPLAY")
            .or_else(|_| std::env::var("DISPLAY"))
            .unwrap_or_default()
            .replace(&[':', '/'][..], "_"));
    let lock_f = std::fs::File::create(&lock)?;
    if unsafe { libc::flock(lock_f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("already running".into());
    }
    LOCK_FD.store(lock_f.as_raw_fd(), Ordering::Relaxed);

    // Wayland preferred; falls back to X11
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        wayland::run()
    } else {
        x11::run()
    }
}
