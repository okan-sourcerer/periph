//! Connected Bluetooth devices and their battery levels, from BlueZ.

use std::collections::HashMap;

use periph_core::{Battery, Device, DeviceKind, Transport};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::OwnedValue;

pub struct BtDevice {
    pub address: String,
    pub device: Device,
}

pub async fn connected(system: &Connection) -> zbus::Result<Vec<BtDevice>> {
    let manager = ObjectManagerProxy::builder(system)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let mut out = Vec::new();
    for interfaces in manager.get_managed_objects().await?.into_values() {
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
        let battery = interfaces
            .iter()
            .find(|(name, _)| name.as_str() == "org.bluez.Battery1")
            .and_then(|(_, p)| get::<u8>(p, "Percentage"))
            .map(|percent| Battery {
                percent: percent.min(100),
                charging: None,
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
            },
            address,
        });
    }
    out.sort_by(|a, b| a.device.name.cmp(&b.device.name));
    Ok(out)
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
