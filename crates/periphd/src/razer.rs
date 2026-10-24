//! Razer BlackShark V2 Pro 2.4 (dongle 1532:0555).
//!
//! This headset doesn't use Razer's usual 90-byte feature reports. It speaks a
//! "PA"-framed command protocol in 64-byte interrupt reports (report id 2) on
//! the dongle's vendor HID interface. Protocol facts come from the OpenRazer
//! work in openrazer/openrazer#2862, which decoded Synapse USB captures.
//!
//! Request:  [0]=0x02 [1]=0x80 [2]=8+len [5]='P' [6]='A' [7]=0x08 [9]=type
//!           [10]=cmd [12]=len [13..]=payload
//! Reply:    [0]=0x02 [12]=cmd [13]=0x01 (ack) [14]=len [15..]=payload
//!
//! The 2.4 GHz link dozes after ~0.3 s idle and drops the first frame sent
//! into it, so every exchange starts with a sacrificial "remote mode on".

use std::io;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::debug;
use periph_core::{Battery, Device, DeviceKind, Setting, SettingKind, Transport};

use crate::hidraw::{Hidraw, HidrawNode};

pub const VENDOR: u16 = 0x1532;
pub const PRODUCT: u16 = 0x0555;

const REPORT_LEN: usize = 64;
const REPORT_ID: u8 = 0x02;
const REPLY_TIMEOUT: Duration = Duration::from_millis(150);
const ATTEMPTS: usize = 3;
const REMOTE_SETTLE: Duration = Duration::from_millis(35);
const STATUS_INTERVAL: Duration = Duration::from_secs(30);
/// While the headset is off, look for it more often so switching it on shows up quickly.
const OFFLINE_INTERVAL: Duration = Duration::from_secs(5);

const TYPE_REMOTE: u8 = 0x02;
const TYPE_QUERY: u8 = 0x03;
const TYPE_AUDIO: u8 = 0x04;

const CMD_REMOTE: u8 = 0xe1;
const CMD_PREP: u8 = 0x1e;
const CMD_FW_VER: u8 = 0x02;
const CMD_BATTERY: u8 = 0x21;
const CMD_CHARGING: u8 = 0x2a;
const CMD_MIC_MUTE: u8 = 0x55;
const CMD_PRESET_GET: u8 = 0x13;
const CMD_SIDETONE_GET: u8 = 0x18;
const CMD_SIDETONE_LVL_GET: u8 = 0x19;
const CMD_DND_GET: u8 = 0x27;
const CMD_IDLE_GET: u8 = 0x2c;
// Setters are getter + 0x80.
const CMD_PRESET: u8 = 0x93;
const CMD_SIDETONE: u8 = 0x98;
const CMD_SIDETONE_LVL: u8 = 0x99;
const CMD_ENHANCE: u8 = 0x9d;
const CMD_DND: u8 = 0xa7;
const CMD_IDLE_SET: u8 = 0xac;

const SIDETONE_MAX: i32 = 10;

const PRESETS: &[(i32, &str)] = &[
    (0x07, "Game"),
    (0x08, "Music"),
    (0x09, "Movie"),
    (0xff, "Custom"),
    (0xfa, "Game EQ 1"),
    (0xfe, "Game EQ 2"),
    (0xfb, "Game EQ 3"),
    (0xfd, "Game EQ 4"),
    (0xfc, "Game EQ 5"),
];

const AUTO_OFF: &[(i32, &str)] = &[
    (0, "Never"),
    (15, "15 minutes"),
    (30, "30 minutes"),
    (45, "45 minutes"),
    (60, "60 minutes"),
];

pub fn matches(node: &HidrawNode) -> bool {
    node.vendor == VENDOR && node.product == PRODUCT && node.has_vendor_page()
}

#[derive(Default, Clone)]
struct Cached {
    connected: bool,
    battery: Option<Battery>,
    mic_muted: Option<bool>,
    firmware: Option<String>,
    preset: Option<u8>,
    sidetone: Option<u8>,
    dnd: Option<bool>,
    idle_minutes: Option<u8>,
}

pub struct BlackShark {
    pub id: String,
    pub devnode: PathBuf,
    dev: Result<Hidraw, String>,
    cache: Cached,
    last_status: Option<Instant>,
    settings_stale: bool,
}

impl BlackShark {
    pub fn new(id: String, devnode: PathBuf) -> Self {
        let dev = Hidraw::open(&devnode).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => {
                "No permission to open the dongle. Install the periph udev rules.".to_string()
            }
            _ => e.to_string(),
        });
        Self {
            id,
            devnode,
            dev,
            cache: Cached::default(),
            last_status: None,
            settings_stale: true,
        }
    }

    /// Refresh what's due: status every 30 s while connected, every 5 s while
    /// the headset is off (or when forced), settings after a write.
    pub fn poll(&mut self, force: bool) {
        if self.dev.is_err() {
            return;
        }
        let interval = if self.cache.connected {
            STATUS_INTERVAL
        } else {
            OFFLINE_INTERVAL
        };
        let due = force || self.last_status.is_none_or(|t| t.elapsed() >= interval);
        if !due && !(self.cache.connected && self.settings_stale) {
            return;
        }
        self.last_status = Some(Instant::now());

        let was_connected = self.cache.connected;
        let battery = self.query1(CMD_BATTERY);
        self.cache.connected = battery.is_ok();
        let Ok(level) = battery else {
            debug!("{}: headset not answering", self.id);
            self.cache.battery = None;
            self.settings_stale = true;
            return;
        };
        let charging = self.query1(CMD_CHARGING).ok().map(|v| v != 0);
        self.cache.battery = Some(Battery {
            percent: level.min(100),
            charging,
        });
        self.cache.mic_muted = self.query1(CMD_MIC_MUTE).ok().map(|v| v != 0);

        if self.settings_stale || !was_connected {
            self.read_settings();
        }
    }

    fn read_settings(&mut self) {
        if self.cache.firmware.is_none()
            && let Ok(fw) = self.query(CMD_FW_VER)
            && fw.len() >= 2
        {
            self.cache.firmware = Some(format!("{}.{}", fw[0], fw[1]));
        }
        self.cache.preset = self.query1(CMD_PRESET_GET).ok();
        self.cache.sidetone = match self.query1(CMD_SIDETONE_GET) {
            Ok(0) => Some(0),
            Ok(_) => self.query1(CMD_SIDETONE_LVL_GET).ok(),
            Err(_) => self.cache.sidetone,
        };
        self.cache.dnd = self.query1(CMD_DND_GET).ok().map(|v| v != 0);
        self.cache.idle_minutes = self.query1(CMD_IDLE_GET).ok();
        self.settings_stale = false;
    }

    pub fn set(&mut self, key: &str, value: i32) -> Result<(), String> {
        if let Err(e) = &self.dev {
            return Err(e.clone());
        }
        let v = |lo: i32, hi: i32| value.clamp(lo, hi) as u8;
        let result = match key {
            "sidetone" => {
                let level = v(0, SIDETONE_MAX);
                self.write_value(CMD_SIDETONE, (level > 0) as u8)
                    .and_then(|_| {
                        if level > 0 {
                            self.write_value(CMD_SIDETONE_LVL, level)
                        } else {
                            Ok(())
                        }
                    })
            }
            "auto_off" => {
                let minutes = if value <= 0 { 0 } else { v(15, 60) };
                self.write_value(CMD_IDLE_SET, minutes)
            }
            "dnd" => self.write_value(CMD_DND, (value != 0) as u8),
            "eq_preset" => {
                if !PRESETS.iter().any(|(p, _)| *p == value) {
                    return Err(format!("unknown EQ preset {value}"));
                }
                self.apply_preset(value as u8)
            }
            _ => return Err(format!("unknown setting {key}")),
        };
        self.settings_stale = true;
        result.map_err(|e| e.to_string())
    }

    pub fn snapshot(&self) -> Device {
        let mut info = Vec::new();
        if let Err(e) = &self.dev {
            info.push(("Status".into(), e.clone()));
        } else if !self.cache.connected {
            info.push(("Status".into(), "Headset off or out of range".into()));
        }
        if let Some(fw) = &self.cache.firmware {
            info.push(("Firmware".into(), fw.clone()));
        }
        if let Some(m) = self.cache.mic_muted {
            info.push(("Microphone".into(), if m { "Muted" } else { "Live" }.into()));
        }

        let mut settings = Vec::new();
        if self.cache.connected {
            if let Some(p) = self.cache.preset {
                settings.push(choice("eq_preset", "Equalizer preset", PRESETS, p as i32));
            }
            if let Some(s) = self.cache.sidetone {
                settings.push(Setting {
                    key: "sidetone".into(),
                    label: "Mic monitoring (0 = off)".into(),
                    kind: SettingKind::Range {
                        min: 0,
                        max: SIDETONE_MAX,
                    },
                    value: s as i32,
                });
            }
            if let Some(m) = self.cache.idle_minutes {
                settings.push(choice("auto_off", "Auto power-off", AUTO_OFF, m as i32));
            }
            if let Some(d) = self.cache.dnd {
                settings.push(Setting {
                    key: "dnd".into(),
                    label: "Do not disturb".into(),
                    kind: SettingKind::Toggle,
                    value: d as i32,
                });
            }
        }

        Device {
            id: self.id.clone(),
            name: "Razer BlackShark V2 Pro".into(),
            kind: DeviceKind::Headset,
            transport: Transport::Dongle,
            connected: self.cache.connected,
            battery: self.cache.battery,
            settings,
            info,
            buttons: Vec::new(),
        }
    }

    // --- protocol ---------------------------------------------------------

    fn hid(&self) -> io::Result<&Hidraw> {
        self.dev
            .as_ref()
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.clone()))
    }

    fn send(&self, frame: &[u8; REPORT_LEN]) -> io::Result<()> {
        self.hid()?.write(frame)
    }

    fn set_remote(&self, on: bool) -> io::Result<()> {
        let mut b = envelope(TYPE_REMOTE, CMD_REMOTE);
        b[2] = 0x07;
        b[7] = 0x0e;
        b[11] = on as u8;
        self.send(&b)?;
        sleep(REMOTE_SETTLE);
        Ok(())
    }

    fn query1(&mut self, cmd: u8) -> io::Result<u8> {
        self.query(cmd)?
            .first()
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty reply"))
    }

    fn query(&mut self, cmd: u8) -> io::Result<Vec<u8>> {
        self.set_remote(true)?; // wakes a dozing link; may be dropped
        let result = self.query_attempts(cmd);
        let _ = self.set_remote(false);
        result
    }

    fn query_attempts(&mut self, cmd: u8) -> io::Result<Vec<u8>> {
        for attempt in 1..=ATTEMPTS {
            self.set_remote(true)?;
            self.drain()?;
            self.send(&frame(TYPE_QUERY, cmd, &[]))?;
            if let Some(payload) = self.await_reply(cmd)? {
                return Ok(payload);
            }
            debug!(
                "{}: no reply to 0x{cmd:02x} (attempt {attempt}/{ATTEMPTS})",
                self.id
            );
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "headset did not answer",
        ))
    }

    fn await_reply(&mut self, cmd: u8) -> io::Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut buf = [0u8; REPORT_LEN];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(n) = self.hid()?.read_timeout(&mut buf, left)? else {
                return Ok(None);
            };
            let r = &buf[..n];
            self.note_unsolicited(r);
            if n > 15 && r[0] == REPORT_ID && r[12] == cmd && r[13] == 0x01 {
                let len = (r[14] as usize).min(n - 15);
                return Ok(Some(r[15..15 + len].to_vec()));
            }
        }
    }

    /// Discard queued input so a stale report can't pass as the reply.
    fn drain(&mut self) -> io::Result<()> {
        let mut buf = [0u8; REPORT_LEN];
        while let Some(n) = self.hid()?.read_timeout(&mut buf, Duration::ZERO)? {
            self.note_unsolicited(&buf[..n]);
        }
        Ok(())
    }

    /// The headset announces EQ preset changes made with its own button.
    fn note_unsolicited(&mut self, r: &[u8]) {
        if r.len() > 15 && r[0] == REPORT_ID && r[12] == CMD_PRESET_GET && r[14] >= 1 {
            self.cache.preset = Some(r[15]);
        }
    }

    fn write_value(&mut self, cmd: u8, value: u8) -> io::Result<()> {
        self.set_remote(true)?;
        self.set_remote(true)?;
        self.send(&frame(TYPE_AUDIO, cmd, &[value]))?;
        self.set_remote(false)
    }

    /// Synapse's sequence: remote x2, prep query, selector, enhance flag,
    /// remote. A selector crossing between the classic and game-EQ families
    /// only switches family the first time, so retry until it reads back.
    fn apply_preset(&mut self, preset: u8) -> io::Result<()> {
        let flag = if (0xfa..=0xfe).contains(&preset) {
            2
        } else {
            1
        };
        for _ in 0..4 {
            self.set_remote(true)?;
            self.set_remote(true)?;
            self.send(&frame(TYPE_QUERY, CMD_PREP, &[]))?;
            self.set_remote(true)?;
            self.send(&frame(TYPE_AUDIO, CMD_PRESET, &[preset]))?;
            self.set_remote(true)?;
            self.send(&frame(TYPE_AUDIO, CMD_ENHANCE, &[flag]))?;
            self.set_remote(true)?;
            sleep(Duration::from_millis(100));
            if self.query1(CMD_PRESET_GET)? == preset {
                self.cache.preset = Some(preset);
                return Ok(());
            }
        }
        Err(io::Error::other("headset did not switch preset"))
    }
}

fn envelope(cmd_type: u8, cmd: u8) -> [u8; REPORT_LEN] {
    let mut b = [0u8; REPORT_LEN];
    b[0] = REPORT_ID;
    b[1] = 0x80;
    b[5] = b'P';
    b[6] = b'A';
    b[9] = cmd_type;
    b[10] = cmd;
    b
}

fn frame(cmd_type: u8, cmd: u8, data: &[u8]) -> [u8; REPORT_LEN] {
    let mut b = envelope(cmd_type, cmd);
    b[2] = 8 + data.len() as u8;
    b[7] = 0x08;
    b[12] = data.len() as u8;
    b[13..13 + data.len()].copy_from_slice(data);
    b
}

fn choice(key: &str, label: &str, options: &[(i32, &str)], value: i32) -> Setting {
    let mut options: Vec<(i32, String)> = options
        .iter()
        .map(|(v, l)| (*v, (*l).to_string()))
        .collect();
    if !options.iter().any(|(v, _)| *v == value) {
        options.push((value, format!("Other ({value})")));
    }
    Setting {
        key: key.into(),
        label: label.into(),
        kind: SettingKind::Choice { options },
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_frame_layout() {
        let f = frame(TYPE_QUERY, CMD_BATTERY, &[]);
        assert_eq!(
            &f[..14],
            &[2, 0x80, 8, 0, 0, b'P', b'A', 8, 0, 3, 0x21, 0, 0, 0]
        );
    }

    #[test]
    fn write_frame_carries_payload() {
        let f = frame(TYPE_AUDIO, CMD_SIDETONE_LVL, &[7]);
        assert_eq!((f[2], f[9], f[10], f[12], f[13]), (9, 4, 0x99, 1, 7));
    }
}
