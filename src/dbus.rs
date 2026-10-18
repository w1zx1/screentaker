// Capture through KWin's restricted D-Bus interface org.kde.KWin.ScreenShot2.
// KWin authorizes the caller by /proc/<pid>/exe matching the Exec= of a .desktop
// file that declares the interface in X-KDE-DBUS-Restricted-Interfaces. The first
// failed attempt writes that one file into ~/.local/share/applications and asks
// KDE's service cache to rescan; a few retries cover the asynchronous refresh.
//
// CaptureWorkspace asks KWin for the whole virtual desktop in a single frame: the
// bounding box of all outputs at device resolution, pixels written into a pipe fd.
// The D-Bus reply (size/format) arrives before the pixels, so EOF on the pipe —
// not the reply — is the frame boundary.
//
// KWin sends a QImage and reports its QImage::Format; the bytes are normalized to
// straight RGBA here so the overlay and the PNG writer need not care (workspace
// captures come as Format_RGBX8888, i.e. already R,G,B,X).

use std::{
    collections::HashMap,
    io::Read,
    fs,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    process::Command,
    thread::sleep,
    time::Duration,
};

use zbus::zvariant::{Fd, Value};

use crate::Img;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const SVC: &str = "org.kde.KWin.ScreenShot2"; // bus name and interface
const OBJ: &str = "/org/kde/KWin/ScreenShot2";
const RETRIES: usize = 10;

const ENTRY: &str = "\
[Desktop Entry]
Type=Application
Name=Screentaker
GenericName=Screen capture
Comment=Screenshooter capturing through KWin ScreenShot2
Exec={exe}
Icon=accessories-screenshot-tool
Terminal=false
NoDisplay=true
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
";

// 10 hex chars unique per binary path (FNV-1a)
fn tag(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:010x}")
}

fn exe_path() -> Res<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    Ok(exe.to_string_lossy().into_owned())
}

fn auth_file() -> Res<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let mut p = std::env::home_dir().unwrap_or_default();
            p.push(".local/share");
            p
        });
    let exe = exe_path()?;
    Ok(base.join("applications").join(format!("screentaker-{}.desktop", tag(&exe))))
}

fn entry_body(exe: &str) -> String {
    ENTRY.replace("{exe}", exe)
}

fn authorized() -> Res<bool> {
    let exe = exe_path()?;
    let path = auth_file()?;
    let Ok(text) = fs::read_to_string(&path) else { return Ok(false) };
    Ok(text == entry_body(&exe))
}

fn ensure_authorized() -> Res<()> {
    if authorized()? {
        return Ok(());
    }
    let exe = exe_path()?;
    let path = auth_file()?;
    fs::create_dir_all(path.parent().ok_or("bad auth path")?)?;
    fs::write(&path, entry_body(&exe))?;
    // KWin reads desktop files through KService; ask it to rescan (best effort)
    for tool in ["kbuildsycoca6", "kbuildsycoca5"] {
        if Command::new(tool).status().is_ok() {
            break;
        }
    }
    Ok(())
}

fn capture_once() -> Res<Img> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let mut reader = unsafe { fs::File::from_raw_fd(fds[0]) };

    let conn = zbus::blocking::Connection::session()?;
    let opts: HashMap<&str, Value> = HashMap::from([
        ("include-decoration", Value::Bool(false)),
        ("include-shadow", Value::Bool(false)),
        ("include-cursor", Value::Bool(false)),
        ("native-resolution", Value::Bool(true)),
        ("hide-caller-windows", Value::Bool(true)),
    ]);
    let reply = conn.call_method(
        Some(SVC),
        OBJ,
        Some(SVC),
        "CaptureWorkspace",
        &(&opts, Fd::from(writer.as_fd())),
    )?;

    let body = reply.body();
    let meta: HashMap<String, Value> = body.deserialize()?;
    let num = |k: &str| -> u64 {
        match meta.get(k) {
            Some(Value::U64(n)) => *n,
            Some(Value::U32(n)) => *n as u64,
            Some(Value::I32(n)) => *n as u64,
            _ => 0,
        }
    };
    let (w, h, stride) = (num("width") as u32, num("height") as u32, num("stride") as u32);
    let fmt = num("format");
    let want = stride as usize * h as usize;
    if w == 0 || h == 0 || want == 0 {
        return Err("KWin returned an empty capture frame".into());
    }

    // KWin writes from a thread pool after the reply; close our write end so its
    // close terminates our read
    drop(writer);
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        match reader.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    if got != want {
        return Err(format!("short capture: {got} of {want} bytes").into());
    }

    let mut rgba = if stride == w * 4 {
        buf
    } else {
        // drop padding between rows
        let mut tight = Vec::with_capacity(w as usize * h as usize * 4);
        for row in 0..h as usize {
            let s = row * stride as usize;
            tight.extend_from_slice(&buf[s..s + w as usize * 4]);
        }
        tight
    };

    // QImage's native 32-bit family (Format_RGB32=4, ARGB32=5,
    // ARGB32_Premultiplied=6) is 0xAARRGGBB, i.e. byte-swapped; the 8888 family
    // KWin actually uses for screenshots (RGBX8888=16, RGBA8888=17/18) is R,G,B,A.
    if matches!(fmt, 4..=6) {
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
    Ok(Img { w, h, rgba })
}

pub fn workspace_frame() -> Res<Img> {
    for _ in 0..RETRIES {
        match capture_once() {
            Ok(img) => return Ok(img),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("authoriz") {
                    // KService notices our file asynchronously — write and retry
                    ensure_authorized()?;
                    sleep(Duration::from_millis(250));
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err("KWin did not authorize the capture in time; try again in a few seconds".into())
}
