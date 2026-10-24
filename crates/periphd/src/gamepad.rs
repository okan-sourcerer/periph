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
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, UinputAbsSetup};
use log::{info, warn};
use periph_core::{Battery, Device, DeviceKind, RemapProfile, Transport};

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
        if name.ends_with(VIRTUAL_SUFFIX) {
            continue;
        }
        let id = dev.input_id();
        let mut device_id = format!("pad:{:04x}:{:04x}:{}", id.vendor(), id.product(), name);
        let same = pads
            .iter()
            .filter(|p| p.device.id.starts_with(&device_id))
            .count();
        if same > 0 {
            device_id.push_str(&format!("#{}", same + 1));
        }
        let buttons = keys
            .iter()
            .map(|k| k.code())
            .filter(|c| (0x100..0x300).contains(c))
            .collect();
        pads.push(Pad {
            device: Device {
                id: device_id,
                name,
                kind: DeviceKind::Gamepad,
                transport: if id.bus_type().0 == BUS_BLUETOOTH {
                    Transport::Bluetooth
                } else {
                    Transport::Usb
                },
                connected: true,
                battery: battery_for(&path),
                settings: Vec::new(),
                info: Vec::new(),
                buttons,
            },
            uniq: dev
                .unique_name()
                .filter(|u| !u.is_empty())
                .map(|u| u.to_uppercase()),
            path,
        });
    }
    pads
}

/// Find a device-scoped power_supply whose device is an ancestor of this
/// input node (hid-playstation, hid-nintendo, xpadneo, ... register one).
fn battery_for(devnode: &Path) -> Option<Battery> {
    let event = devnode.file_name()?;
    let input = fs::canonicalize(Path::new("/sys/class/input").join(event).join("device")).ok()?;
    for ps in fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
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
        let percent = read("capacity").and_then(|c| c.parse::<u8>().ok())?;
        let charging = read("status").map(|s| s == "Charging");
        return Some(Battery {
            percent: percent.min(100),
            charging,
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
    pub fn start(path: PathBuf, profile: RemapProfile) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, map, stop, done) =
                (path.clone(), profile.clone(), stop.clone(), done.clone());
            thread::spawn(move || {
                if let Err(e) = run(&path, &map, &stop) {
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

fn run(path: &Path, profile: &RemapProfile, stop: &AtomicBool) -> io::Result<()> {
    let mut pad = evdev::Device::open(path)?;
    let name = format!("{}{VIRTUAL_SUFFIX}", pad.name().unwrap_or("Gamepad"));

    let mut keys: AttributeSet<KeyCode> = pad
        .supported_keys()
        .map(|k| k.iter().collect())
        .unwrap_or_default();
    for &target in profile.map.values().filter(|&&t| t != 0) {
        keys.insert(KeyCode::new(target));
    }
    let mut builder = VirtualDevice::builder()?
        .name(&name)
        .input_id(pad.input_id())
        .with_keys(&keys)?;
    let axes: Vec<_> = pad.get_absinfo()?.collect();
    for (axis, info) in axes {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
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
                EventType::KEY => match profile.map.get(&ev.code()) {
                    Some(0) => {}
                    Some(&target) => {
                        out.push(InputEvent::new(EventType::KEY.0, target, ev.value()))
                    }
                    None => out.push(ev),
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
