# Periph

Battery, settings and button remapping for wireless peripherals on Linux:
2.4 GHz dongles, Bluetooth devices and gamepads.

## Layout

| Crate | Binary | Role |
|---|---|---|
| `crates/periph-core` | – | Shared types (`Device`, `Setting`, `RemapProfile`) and the D-Bus proxy |
| `crates/periphd` | `periphd` | User service. Owns all hardware access, publishes `io.github.periph.Daemon` on the session bus |
| `crates/periph-gui` | `periph` | GTK4/libadwaita window plus a tray icon (StatusNotifierItem) |

The daemon keeps running when the window is closed, so remaps stay active.
Nothing runs as root: a udev rule grants the logged-in user access to the
dongle's hidraw node and to `/dev/uinput`.

### Where devices come from

- **Vendor dongles**: `hidraw`, one module per protocol (`periphd/src/razer.rs`).
- **Bluetooth**: BlueZ over D-Bus (`org.bluez.Device1`, `Battery1`).
- **Gamepads**: evdev. Battery comes from a kernel `power_supply` when the driver
  registers one. Remapping grabs the pad and replays it through a uinput clone
  (`periphd/src/gamepad.rs`).

## Build and install

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev   # once
./install.sh                                     # asks for sudo only for the udev rule
```

For development, run `cargo run -p periphd` in one terminal and
`cargo run -p periph-gui` in another.

## Supported hardware

| Device | Battery | Settings |
|---|---|---|
| Razer BlackShark V2 Pro 2.4 (`1532:0555`) | level and charging | EQ preset, mic monitoring, auto power-off, do not disturb |
| Any Bluetooth device | if BlueZ reports it | – |
| Any evdev gamepad (8BitDo, Xbox, DualSense, ...) | if the kernel driver reports it | button remapping |

The Razer protocol is based on the reverse-engineering in
[openrazer/openrazer#2862](https://github.com/openrazer/openrazer/pull/2862).

## Adding a device

1. Capture what the vendor tool sends, using Wireshark with `usbmon` on Windows or in a VM.
2. Add a module next to `razer.rs` that matches on VID/PID and implements
   `poll`, `set` and `snapshot`.
3. Add its VID/PID to `dist/70-periph.rules`.

## Remapping caveats

- Rumble isn't forwarded to the virtual pad yet.
- Games that read the pad through hidraw (Steam, some SDL builds) still see
  the original device next to the virtual one.
- D-pads that report as a hat axis (xpad, 8BitDo in X-input mode) can't be
  remapped yet; only buttons can.
