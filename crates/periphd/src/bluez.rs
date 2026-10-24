//! Connected Bluetooth devices and their battery levels, from BlueZ.
//!
//! BlueZ normally publishes a GATT device's battery as Battery1.Percentage.
//! Its battery plugin sometimes fails to register a device ("Trying to update
//! an unregistered battery"), leaving Battery1 without a percentage; for those
//! we read the Battery Level characteristic (0x2A19) ourselves.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use periph_core::{Battery, Device, DeviceKind, Transport};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const BATTERY_LEVEL_UUID: &str = "00002a19-0000-1000-8000-00805f9b34fb";
/// Our own GATT reads wake the peripheral's radio, so keep them rare.
const GATT_READ_INTERVAL: Duration = Duration::from_secs(300);

pub struct BtDevice {
    pub address: String,
    pub device: Device,
}

pub struct Bluez {
    system: Connection,
    /// Last Battery Level read straight from GATT, per device address.
    gatt_levels: HashMap<String, (Instant, u8)>,
}

impl Bluez {
    pub fn new(system: Connection) -> Self {
        Self {
            system,
            gatt_levels: HashMap::new(),
        }
    }

    pub async fn connected(&mut self) -> zbus::Result<Vec<BtDevice>> {
        let manager = ObjectManagerProxy::builder(&self.system)
            .destination("org.bluez")?
            .path("/")?
            .build()
            .await?;
        let objects = manager.get_managed_objects().await?;
        let mut out = Vec::new();
        for (path, interfaces) in &objects {
            let Some(dev) = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == "org.bluez.Device1")
                .map(|(_, p)| p)
            else {
                continue;
            };
            if get::<bool>(dev, "Connected") != Some(true) {
                continue;
            }
            let address = get::<String>(dev, "Address")
                .unwrap_or_default()
                .to_uppercase();
            let name = get::<String>(dev, "Alias")
                .or_else(|| get::<String>(dev, "Name"))
                .map(|n| n.trim().to_string())
                .unwrap_or_else(|| address.clone());
            let icon = get::<String>(dev, "Icon").unwrap_or_default();
            let battery1 = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == "org.bluez.Battery1")
                .map(|(_, p)| p);
            let mut percent = battery1.and_then(|p| get::<u8>(p, "Percentage"));
            if percent.is_none() && battery1.is_some() {
                percent = self.gatt_level(&address, path, &objects).await;
            }
            let battery = percent.map(|percent| Battery {
                percent: percent.min(100),
                charging: None,
                coarse: false,
            });
            out.push(BtDevice {
                device: Device {
                    id: format!("bt:{address}"),
                    name,
                    kind: kind_for_icon(&icon),
                    transport: Transport::Bluetooth,
                    connected: true,
                    battery,
                    settings: Vec::new(),
                    info: vec![("Address".into(), address.clone())],
                    buttons: Vec::new(),
                    layout: Default::default(),
                },
                address,
            });
        }
        out.sort_by(|a, b| a.device.name.cmp(&b.device.name));
        Ok(out)
    }

    /// Battery Level read from the device's GATT characteristic, cached.
    async fn gatt_level(
        &mut self,
        address: &str,
        device: &OwnedObjectPath,
        objects: &zbus::fdo::ManagedObjects,
    ) -> Option<u8> {
        if let Some((at, level)) = self.gatt_levels.get(address)
            && at.elapsed() < GATT_READ_INTERVAL
        {
            return Some(*level);
        }
        let prefix = format!("{}/", device.as_str());
        let characteristic = objects.iter().find_map(|(path, ifaces)| {
            let props = ifaces
                .iter()
                .find(|(n, _)| n.as_str() == "org.bluez.GattCharacteristic1")?
                .1;
            (path.as_str().starts_with(&prefix)
                && get::<String>(props, "UUID").as_deref() == Some(BATTERY_LEVEL_UUID))
            .then_some(path)
        })?;
        let options: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        let reply = self
            .system
            .call_method(
                Some("org.bluez"),
                characteristic.as_str(),
                Some("org.bluez.GattCharacteristic1"),
                "ReadValue",
                &(options,),
            )
            .await
            .ok()?;
        let level = *reply.body().deserialize::<Vec<u8>>().ok()?.first()?;
        self.gatt_levels
            .insert(address.to_string(), (Instant::now(), level));
        Some(level)
    }
}

fn get<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn kind_for_icon(icon: &str) -> DeviceKind {
    match icon {
        "audio-headset" | "audio-headphones" => DeviceKind::Headset,
        "input-gaming" => DeviceKind::Gamepad,
        "input-keyboard" => DeviceKind::Keyboard,
        "input-mouse" | "input-tablet" => DeviceKind::Mouse,
        "audio-card" | "audio-speakers" => DeviceKind::Audio,
        _ => DeviceKind::Other,
    }
}
