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
    /// The device only reports a coarse level (critical/low/normal/high/full,
    /// e.g. Switch controllers); `percent` is that level's midpoint.
    #[serde(default)]
    pub coarse: bool,
}

impl Battery {
    /// "75%", or the level's name when the device doesn't report a percentage.
    pub fn level_text(&self) -> String {
        if !self.coarse {
            return format!("{}%", self.percent);
        }
        match self.percent {
            0..=10 => "Critical",
            11..=35 => "Low",
            36..=60 => "Normal",
            61..=90 => "High",
            _ => "Full",
        }
        .into()
    }
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
    /// Read-only facts worth showing (firmware, ...).
    pub info: Vec<(String, String)>,
    /// evdev button codes the device has; non-empty means it can be remapped.
    pub buttons: Vec<u16>,
    /// What's printed on the buttons that send each code.
    #[serde(default)]
    pub layout: ButtonLayout,
}

/// Which printed label sits behind each evdev face-button code. Drivers
/// disagree, so this is decided per driver and verified on hardware.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonLayout {
    /// xpad and most others: 0x130 = A, 0x131 = B, 0x133 = X, 0x134 = Y.
    #[default]
    Xbox,
    /// hid-nintendo (Switch Pro and pads emulating it, like 8BitDo in
    /// Switch mode). Codes are positional, so the right button (A) is east:
    /// 0x130 = B, 0x131 = A, 0x133 = X, 0x134 = Y. Verified on an 8BitDo
    /// Ultimate over Bluetooth.
    Nintendo,
}

/// Button remapping for one gamepad. `map` sends a source evdev code to a
/// target code; a target of 0 disables the button.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemapProfile {
    pub enabled: bool,
    pub map: BTreeMap<u16, u16>,
    /// Present the pad to games as an Xbox 360 controller. Buttons keep their
    /// positions (bottom = A, right = B, left = X, top = Y), so a Switch-mode
    /// pad over Bluetooth behaves like it does on an xpad dongle.
    #[serde(default)]
    pub xbox: bool,
}

impl RemapProfile {
    /// Whether this profile needs the pad grabbed at all.
    pub fn active(&self) -> bool {
        self.enabled && (self.xbox || !self.map.is_empty())
    }
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

/// Switch names for the buttons hid-nintendo reports.
const SWITCH_BUTTONS: &[(u16, &str)] = &[
    (0x135, "Capture"),
    (0x136, "L"),
    (0x137, "R"),
    (0x138, "ZL"),
    (0x139, "ZR"),
    (0x13a, "\u{2212} (minus)"),
    (0x13b, "+ (plus)"),
    (0x13c, "Home"),
];

/// The Xbox code carrying the same printed label as `code` on `layout`.
pub fn label_code(code: u16, layout: ButtonLayout) -> u16 {
    match (layout, code) {
        (ButtonLayout::Nintendo, 0x130) => 0x131,
        (ButtonLayout::Nintendo, 0x131) => 0x130,
        _ => code,
    }
}

/// The code an xpad Xbox pad sends from the same position as `code` on
/// `layout`. xpad reports X (left) as 0x133 and Y (top) as 0x134, while
/// hid-nintendo is positional (0x133 = north/top, 0x134 = west/left).
pub fn xbox_position_code(code: u16, layout: ButtonLayout) -> u16 {
    match (layout, code) {
        (ButtonLayout::Nintendo, 0x133) => 0x134,
        (ButtonLayout::Nintendo, 0x134) => 0x133,
        _ => code,
    }
}

pub fn button_label(code: u16, layout: ButtonLayout) -> String {
    let switch = layout != ButtonLayout::Xbox;
    let code = label_code(code, layout);
    switch
        .then(|| SWITCH_BUTTONS.iter().find(|(c, _)| *c == code))
        .flatten()
        .or_else(|| GAMEPAD_BUTTONS.iter().find(|(c, _)| *c == code))
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("Button 0x{code:03x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_print() {
        use ButtonLayout::*;
        assert_eq!(button_label(0x130, Xbox), "A");
        assert_eq!(button_label(0x131, Nintendo), "A");
        assert_eq!(button_label(0x130, Nintendo), "B");
        assert_eq!(button_label(0x133, Xbox), "X");
        assert_eq!(button_label(0x133, Nintendo), "X");
        assert_eq!(button_label(0x138, Nintendo), "ZL");
        assert_eq!(button_label(0x138, Xbox), "LT (digital)");
    }

    #[test]
    fn code_maps_round_trip() {
        for layout in [ButtonLayout::Xbox, ButtonLayout::Nintendo] {
            for &(code, _) in GAMEPAD_BUTTONS {
                assert_eq!(label_code(label_code(code, layout), layout), code);
                assert_eq!(
                    xbox_position_code(xbox_position_code(code, layout), layout),
                    code
                );
            }
        }
    }
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
