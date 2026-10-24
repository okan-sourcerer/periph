//! Minimal hidraw access: enumerate nodes through sysfs, and a non-blocking
//! handle with timed reads. No libudev or hidapi needed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct HidrawNode {
    pub devnode: PathBuf,
    pub vendor: u16,
    pub product: u16,
    pub report_descriptor: Vec<u8>,
}

impl HidrawNode {
    /// True when the descriptor opens a vendor-defined usage page (0xFF00+),
    /// which is where vendor command channels live.
    pub fn has_vendor_page(&self) -> bool {
        self.report_descriptor
            .windows(3)
            .any(|w| w[0] == 0x06 && w[2] == 0xff)
    }
}

pub fn enumerate() -> Vec<HidrawNode> {
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let sys = entry.path();
        let Ok(uevent) = fs::read_to_string(sys.join("device/uevent")) else {
            continue;
        };
        let mut ids = None;
        for line in uevent.lines() {
            if let Some(v) = line.strip_prefix("HID_ID=") {
                ids = parse_hid_id(v);
            }
        }
        let Some((_bus, vendor, product)) = ids else {
            continue;
        };
        nodes.push(HidrawNode {
            devnode: PathBuf::from("/dev").join(entry.file_name()),
            vendor,
            product,
            report_descriptor: fs::read(sys.join("device/report_descriptor")).unwrap_or_default(),
        });
    }
    nodes.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    nodes
}

/// `HID_ID=0003:00001532:00000555` -> (bus, vendor, product)
fn parse_hid_id(v: &str) -> Option<(u16, u16, u16)> {
    let mut it = v.split(':').map(|p| u32::from_str_radix(p, 16).ok());
    Some((it.next()?? as u16, it.next()?? as u16, it.next()?? as u16))
}

pub struct Hidraw {
    file: File,
}

impl Hidraw {
    pub fn open(path: &PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn write(&self, report: &[u8]) -> io::Result<()> {
        (&self.file).write_all(report)
    }

    /// Read one report, waiting at most `timeout`. `Ok(None)` on timeout.
    pub fn read_timeout(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        let deadline = Instant::now() + timeout;
        loop {
            match (&self.file).read(buf) {
                Ok(n) => return Ok(Some(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let mut pfd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, count 1.
            let r = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            } else if r > 0 && pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
        }
    }
}
