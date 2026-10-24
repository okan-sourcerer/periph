//! Types shared by the daemon (`periphd`) and the GUI (`periph`).
//!
//! The daemon owns all hardware access and publishes a snapshot of every
//! device it knows about over the session bus. Structured values cross the
//! bus as JSON strings, so adding a field never changes the D-Bus signature.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const BUS_NAME: &str = "io.github.periph.Daemon";
pub const OBJECT_PATH: &str = "/io/github/periph/Daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Headset,
    Gamepad,
    Keyboard,
    Mouse,
    Audio,
    Other,
}

impl DeviceKind {
    pub fn icon_name(self) -> &'static str {
        match self {
            DeviceKind::Headset => "audio-headset-symbolic",
            DeviceKind::Gamepad => "input-gaming-symbolic",
            DeviceKind::Keyboard => "input-keyboard-symbolic",
            DeviceKind::Mouse => "input-mouse-symbolic",
            DeviceKind::Audio => "audio-speakers-symbolic",
            DeviceKind::Other => "bluetooth-symbolic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    Dongle,
    Bluetooth,
    Usb,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Dongle => "2.4 GHz dongle",
            Transport::Bluetooth => "Bluetooth",
            Transport::Usb => "USB",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    pub percent: u8,
    /// `None` when the transport can't tell (BlueZ only reports a level).
    pub charging: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SettingKind {
    Toggle,
    Range { min: i32, max: i32 },
    Choice { options: Vec<(i32, String)> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub label: String,
    pub kind: SettingKind,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Stable across reconnects, so saved remaps follow the device.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub transport: Transport,
    /// False when the receiver is present but the device behind it is off.
    pub connected: bool,
    pub battery: Option<Battery>,
    pub settings: Vec<Setting>,
    /// Read-only facts worth showing (firmware, mic mute, ...).
    pub info: Vec<(String, String)>,
    /// evdev button codes the device has; non-empty means it can be remapped.
    pub buttons: Vec<u16>,
}

/// Button remapping for one gamepad. `map` sends a source evdev code to a
/// target code; a target of 0 disables the button.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemapProfile {
    pub enabled: bool,
    pub map: BTreeMap<u16, u16>,
}

/// Human names for the evdev gamepad buttons, in display order. Labels follow
/// the Xbox layout, which is what xpad-style dongles (8BitDo in X-input mode,
/// Xbox receivers) report.
pub const GAMEPAD_BUTTONS: &[(u16, &str)] = &[
    (0x130, "A"),
    (0x131, "B"),
    (0x133, "X"),
    (0x134, "Y"),
    (0x132, "C"),
    (0x135, "Z"),
    (0x136, "LB"),
    (0x137, "RB"),
    (0x138, "LT (digital)"),
    (0x139, "RT (digital)"),
    (0x13a, "Select / Back"),
    (0x13b, "Start"),
    (0x13c, "Home / Guide"),
    (0x13d, "Left stick press"),
    (0x13e, "Right stick press"),
    (0x220, "D-pad up"),
    (0x221, "D-pad down"),
    (0x222, "D-pad left"),
    (0x223, "D-pad right"),
    (0x2c0, "Paddle 1"),
    (0x2c1, "Paddle 2"),
    (0x2c2, "Paddle 3"),
    (0x2c3, "Paddle 4"),
];

pub fn button_label(code: u16) -> String {
    GAMEPAD_BUTTONS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("Button 0x{code:03x}"))
}

#[zbus::proxy(
    interface = "io.github.periph.Daemon1",
    default_service = "io.github.periph.Daemon",
    default_path = "/io/github/periph/Daemon"
)]
pub trait Daemon {
    /// JSON array of [`Device`].
    fn devices(&self) -> zbus::Result<String>;
    fn set_setting(&self, id: &str, key: &str, value: i32) -> zbus::Result<()>;
    /// JSON [`RemapProfile`].
    fn remap(&self, id: &str) -> zbus::Result<String>;
    fn set_remap(&self, id: &str, profile_json: &str) -> zbus::Result<()>;
    fn refresh(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
}
