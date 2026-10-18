# screentaker

a lightweight and reliable screenshooter for Linux written in Rust

> [!WARNING]
> **X11 support is legacy and no longer tested.**
>
> use **Wayland** — tested on KDE Plasma (KWin), selected automatically via
> `$WAYLAND_DISPLAY` / `$DISPLAY`.

## installation

### from aur (stable version)

using your aur manager:

```
yay -S screentaker
```

or manually:

```
git clone https://aur.archlinux.org/screentaker.git
cd screentaker
makepkg -si
```

### build from source

```
https://github.com/w1zx1/screentaker.git
cd screentaker
cargo build --release
```

## usage

```
screentaker
```

- **click & drag** to select a region
- **release** to capture (copies to clipboard as image/png)
- press **Esc** to cancel

on KDE the shot lands in the clipboard history as an image with a thumbnail
(Meta+V), the same way Spectacle's copies do.

## license

[MIT](LICENSE)
