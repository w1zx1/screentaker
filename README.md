# screentaker

a lightweight and reliable screenshooter for X11 written in Rust

### warning

**this screenshoot tool is still in alpha state**

## installation

### from aur

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

### building from source

```
cargo build --release
```

## usage

```
screentaker
```

- **click & drag** to select a region
- **release** to capture (copies to clipboard as image/png)
- press **Esc** to cancel

## license

[MIT](LICENSE)