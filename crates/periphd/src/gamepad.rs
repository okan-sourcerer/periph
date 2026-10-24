//! Gamepads through evdev: discovery, kernel-reported batteries, and button
//! remapping. Remapping grabs the real pad exclusively and replays its events
//! through a uinput clone with the buttons rewritten, so it works for any pad
//! the kernel drives (xpad, hid-generic, hid-playstation, ...).

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    UinputAbsSetup,
};
use log::{info, warn};
use periph_core::{
    Battery, ButtonLayout, Device, DeviceKind, RemapProfile, Transport, xbox_position_code,
};

/// Suffix on our virtual pads, so discovery skips them.
pub const VIRTUAL_SUFFIX: &str = " (periph)";
const BUS_BLUETOOTH: u16 = 0x05;

pub struct Pad {
    pub device: Device,
    pub path: PathBuf,
    /// Bluetooth address for BT pads, used to merge with the BlueZ entry.
    pub uniq: Option<String>,
}

pub fn enumerate() -> Vec<Pad> {
    let mut pads: Vec<Pad> = Vec::new();
    for (path, dev) in evdev::enumerate() {
        let Some(keys) = dev.supported_keys() else {
            continue;
        };
        if !keys.contains(KeyCode::BTN_SOUTH) {
            continue;
        }
        let name = dev.name().unwrap_or("Gamepad").to_string();
        let Some(sys) = input_sysfs(&path) else {
            continue;
        };
        // uinput pads (Steam Input, ours, other remappers) aren't hardware.
        if name.ends_with(VIRTUAL_SUFFIX) || sys.starts_with("/sys/devices/virtual") {
            continue;
        }
        let id = dev.input_id();
        // Over Bluetooth the nearest USB device is the PC's own adapter.
        let usb_product = (id.bus_type().0 != BUS_BLUETOOTH)
            .then(|| usb_product(&sys))
            .flatten();
        let transport = if id.bus_type().0 == BUS_BLUETOOTH {
            Transport::Bluetooth
        } else if usb_product.as_deref().is_some_and(is_receiver) {
            Transport::Dongle
        } else {
            Transport::Usb
        };
        let mut device_id = format!("pad:{:04x}:{:04x}:{}", id.vendor(), id.product(), name);
        let same = pads
            .iter()
            .filter(|p| p.device.id.starts_with(&device_id))
            .count();
        if same > 0 {
            device_id.push_str(&format!("#{}", same + 1));
        }
        let uniq = dev
            .unique_name()
            .filter(|u| !u.is_empty())
            .map(|u| u.to_uppercase());
        let layout = detect_layout(&sys);
        let buttons = keys
            .iter()
            .map(|k| k.code())
            .filter(|c| (0x100..0x300).contains(c))
            .collect();
        pads.push(Pad {
            device: Device {
                id: device_id,
                // The USB product string names the actual model; drivers like
                // xpad share one evdev name across several products.
                name: usb_product.unwrap_or(name),
                kind: DeviceKind::Gamepad,
                transport,
                connected: true,
                battery: battery_for(&path),
                settings: Vec::new(),
                info: Vec::new(),
                buttons,
                layout,
            },
            uniq,
            path,
        });
    }
    pads
}

/// Which printed label each face-button code carries, from the driver bound
/// to the device the input hangs off.
fn detect_layout(sys: &Path) -> ButtonLayout {
    // The input sits under the driver's device (HID device or USB interface).
    let driver = sys
        .ancestors()
        .find_map(|p| fs::read_link(p.join("driver")).ok())
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()));
    match driver.as_deref() {
        Some("nintendo") => ButtonLayout::Nintendo,
        _ => ButtonLayout::Xbox,
    }
}

/// Real sysfs directory of the input device behind an event node.
fn input_sysfs(devnode: &Path) -> Option<PathBuf> {
    let event = devnode.file_name()?;
    fs::canonicalize(Path::new("/sys/class/input").join(event).join("device")).ok()
}

/// "Manufacturer Product" of the USB device this input hangs off, if any.
fn usb_product(sys: &Path) -> Option<String> {
    let usb = sys.ancestors().find(|dir| dir.join("idVendor").exists())?;
    let read = |f: &str| {
        fs::read_to_string(usb.join(f))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
    };
    let product = read("product")?;
    Some(match read("manufacturer") {
        Some(maker) if !product.starts_with(&maker) => format!("{maker} {product}"),
        _ => product,
    })
}

/// A pad that reaches the PC through a 2.4 GHz receiver rather than a cable.
fn is_receiver(product: &str) -> bool {
    let p = product.to_lowercase();
    ["wireless", "receiver", "dongle", "adapter"]
        .iter()
        .any(|w| p.contains(w))
}

/// Find a device-scoped power_supply whose device is an ancestor of this
/// input node (hid-playstation, hid-nintendo, xpadneo, ... register one).
fn battery_for(devnode: &Path) -> Option<Battery> {
    battery_in(Path::new("/sys/class/power_supply"), &input_sysfs(devnode)?)
}

fn battery_in(power_supplies: &Path, input: &Path) -> Option<Battery> {
    for ps in fs::read_dir(power_supplies).ok()?.flatten() {
        let ps = ps.path();
        let read = |f: &str| {
            fs::read_to_string(ps.join(f))
                .map(|s| s.trim().to_string())
                .ok()
        };
        if read("scope").as_deref() != Some("Device") {
            continue;
        }
        let Ok(owner) = fs::canonicalize(ps.join("device")) else {
            continue;
        };
        if !input.starts_with(&owner) {
            continue;
        }
        let charging = read("status").map(|s| s == "Charging");
        if let Some(percent) = read("capacity").and_then(|c| c.parse::<u8>().ok()) {
            return Some(Battery {
                percent: percent.min(100),
                charging,
                coarse: false,
            });
        }
        // hid-nintendo and a few others only report a level.
        let percent = match read("capacity_level")?.as_str() {
            "Critical" => 5,
            "Low" => 25,
            "Normal" => 50,
            "High" => 75,
            "Full" => 100,
            _ => return None,
        };
        return Some(Battery {
            percent,
            charging,
            coarse: true,
        });
    }
    None
}

/// A running remap: one thread owning the grabbed pad and its uinput clone.
pub struct Remapper {
    pub path: PathBuf,
    pub profile: RemapProfile,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Remapper {
    pub fn start(path: PathBuf, profile: RemapProfile, layout: ButtonLayout) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, map, stop, done) =
                (path.clone(), profile.clone(), stop.clone(), done.clone());
            thread::spawn(move || {
                if let Err(e) = run(&path, &map, layout, &stop) {
                    warn!("remap on {} stopped: {e}", path.display());
                }
                done.store(true, Ordering::Relaxed);
            })
        };
        Self {
            path,
            profile,
            stop,
            done,
            thread: Some(thread),
        }
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl Drop for Remapper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Where one source button ends up on the virtual pad.
#[derive(Debug, PartialEq)]
enum Out {
    Drop,
    Key(u16),
    /// Xbox mode turns digital ZL/ZR into the analog trigger axes.
    Trigger(AbsoluteAxisCode),
}

/// User remap first (in the pad's own codes), then, in Xbox mode, the code
/// an Xbox pad sends from the same position.
fn translate(code: u16, profile: &RemapProfile, layout: ButtonLayout) -> Out {
    let mapped = if profile.enabled {
        profile.map.get(&code)
    } else {
        None
    };
    let code = match mapped {
        Some(0) => return Out::Drop,
        Some(&target) => target,
        None => code,
    };
    if !profile.xbox {
        return Out::Key(code);
    }
    match xbox_position_code(code, layout) {
        0x138 => Out::Trigger(AbsoluteAxisCode::ABS_Z),
        0x139 => Out::Trigger(AbsoluteAxisCode::ABS_RZ),
        c => Out::Key(c),
    }
}

fn run(
    path: &Path,
    profile: &RemapProfile,
    layout: ButtonLayout,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut pad = evdev::Device::open(path)?;
    let (name, input_id) = if profile.xbox {
        // A wired Xbox 360 pad: every game and SDL knows this one.
        (
            format!("Xbox 360 Controller{VIRTUAL_SUFFIX}"),
            InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0114),
        )
    } else {
        (
            format!("{}{VIRTUAL_SUFFIX}", pad.name().unwrap_or("Gamepad")),
            pad.input_id(),
        )
    };

    let sources: Vec<u16> = pad
        .supported_keys()
        .map(|k| k.iter().map(|k| k.code()).collect())
        .unwrap_or_default();
    let mut keys = AttributeSet::<KeyCode>::new();
    let mut triggers = Vec::new();
    for out in sources
        .iter()
        .chain(profile.map.values())
        .map(|&c| translate(c, profile, layout))
    {
        match out {
            Out::Key(c) => keys.insert(KeyCode::new(c)),
            Out::Trigger(axis) => triggers.push(axis),
            Out::Drop => {}
        }
    }
    let mut builder = VirtualDevice::builder()?
        .name(&name)
        .input_id(input_id)
        .with_keys(&keys)?;
    let axes: Vec<_> = pad.get_absinfo()?.collect();
    for (axis, info) in &axes {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(*axis, *info))?;
    }
    for axis in triggers {
        if !axes.iter().any(|(a, _)| *a == axis) {
            let info = AbsInfo::new(0, 0, 255, 0, 0, 0);
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
    }
    let mut virt = builder.build()?;

    pad.grab()?;
    pad.set_nonblocking(true)?;
    info!("remapping {} -> {name}", path.display());

    let mut out: Vec<InputEvent> = Vec::with_capacity(16);
    while !stop.load(Ordering::Relaxed) {
        if !wait_readable(pad.as_raw_fd(), 250)? {
            continue;
        }
        let events = match pad.fetch_events() {
            Ok(events) => events,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        out.clear();
        for ev in events {
            match ev.event_type() {
                EventType::SYNCHRONIZATION => {}
                EventType::KEY => match translate(ev.code(), profile, layout) {
                    Out::Drop => {}
                    Out::Key(c) => out.push(InputEvent::new(EventType::KEY.0, c, ev.value())),
                    Out::Trigger(axis) => out.push(InputEvent::new(
                        EventType::ABSOLUTE.0,
                        axis.0,
                        if ev.value() != 0 { 255 } else { 0 },
                    )),
                },
                _ => out.push(ev),
            }
        }
        if !out.is_empty() {
            virt.emit(&out)?;
        }
    }
    let _ = pad.ungrab();
    Ok(())
}

/// poll(2) one fd. Errors on hangup, which is how an unplugged pad shows up.
fn wait_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, count 1.
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        };
    }
    if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::from_raw_os_error(libc::ENODEV));
    }
    Ok(r > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake sysfs: a HID device with an input child and one power_supply
    /// pointing at it, plus a laptop battery that must be ignored.
    fn sysfs(name: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("periph-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let hid = root.join("devices/usb1/0005:057E:2009.0001");
        let input = hid.join("input/input40");
        fs::create_dir_all(&input).unwrap();
        let class = root.join("class/power_supply");
        let pad = class.join("nintendo_switch_controller_battery_0");
        fs::create_dir_all(&pad).unwrap();
        symlink(&hid, pad.join("device")).unwrap();
        for (f, v) in files {
            fs::write(pad.join(f), format!("{v}\n")).unwrap();
        }
        let laptop = class.join("BAT1");
        fs::create_dir_all(&laptop).unwrap();
        fs::write(laptop.join("scope"), "System\n").unwrap();
        fs::write(laptop.join("capacity"), "42\n").unwrap();
        (class, input)
    }

    #[test]
    fn reads_percentage() {
        let (class, input) = sysfs(
            "pct",
            &[
                ("scope", "Device"),
                ("capacity", "83"),
                ("status", "Charging"),
            ],
        );
        let b = battery_in(&class, &input).unwrap();
        assert_eq!((b.percent, b.charging, b.coarse), (83, Some(true), false));
        assert_eq!(b.level_text(), "83%");
    }

    #[test]
    fn reads_coarse_level() {
        let (class, input) = sysfs(
            "lvl",
            &[
                ("scope", "Device"),
                ("capacity_level", "High"),
                ("status", "Discharging"),
            ],
        );
        let b = battery_in(&class, &input).unwrap();
        assert_eq!((b.percent, b.charging, b.coarse), (75, Some(false), true));
        assert_eq!(b.level_text(), "High");
    }

    #[test]
    fn xbox_mode_keeps_positions() {
        let xbox = RemapProfile {
            enabled: true,
            xbox: true,
            ..Default::default()
        };
        let layout = ButtonLayout::Nintendo;
        // Positions carry over, as on the xpad dongle: bottom is Xbox A...
        assert_eq!(translate(0x130, &xbox, layout), Out::Key(0x130));
        assert_eq!(translate(0x131, &xbox, layout), Out::Key(0x131));
        // ...and top (north, 0x133) is Xbox Y, which xpad sends as 0x134.
        assert_eq!(translate(0x133, &xbox, layout), Out::Key(0x134));
        assert_eq!(translate(0x134, &xbox, layout), Out::Key(0x133));
        assert_eq!(
            translate(0x138, &xbox, layout),
            Out::Trigger(AbsoluteAxisCode::ABS_Z)
        );
    }

    #[test]
    fn user_map_applies_before_xbox_translation() {
        let mut p = RemapProfile {
            enabled: true,
            xbox: true,
            ..Default::default()
        };
        p.map.insert(0x131, 0x134); // right -> left
        p.map.insert(0x130, 0); // bottom disabled
        let layout = ButtonLayout::Nintendo;
        // Left (west, 0x134) leaves as Xbox X, 0x133.
        assert_eq!(translate(0x131, &p, layout), Out::Key(0x133));
        assert_eq!(translate(0x130, &p, layout), Out::Drop);
    }

    #[test]
    fn xbox_mode_works_without_remapping() {
        let mut p = RemapProfile {
            enabled: false,
            xbox: true,
            ..Default::default()
        };
        p.map.insert(0x130, 0); // saved but switched off
        assert!(p.active());
        let layout = ButtonLayout::Nintendo;
        assert_eq!(translate(0x130, &p, layout), Out::Key(0x130));
        assert_eq!(translate(0x133, &p, layout), Out::Key(0x134));
    }

    #[test]
    fn ignores_other_devices() {
        let (class, _) = sysfs("other", &[("scope", "Device"), ("capacity", "83")]);
        assert_eq!(
            battery_in(&class, Path::new("/elsewhere/input/input9")),
            None
        );
    }
}
