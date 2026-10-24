//! StatusNotifierItem tray icon (works on Cinnamon, KDE, and GNOME with the
//! AppIndicator extension).

use async_channel::Sender;
use ksni::menu::StandardItem;
use ksni::{MenuItem, ToolTip, Tray};
use periph_core::Device;

pub enum TrayCommand {
    Show,
    Quit,
}

pub struct PeriphTray {
    pub devices: Vec<Device>,
    pub tx: Sender<TrayCommand>,
}

fn summary(d: &Device) -> String {
    match (d.connected, d.battery) {
        (false, _) => format!("{}: off", d.name),
        (true, Some(b)) if b.charging == Some(true) => {
            format!("{}: {} (charging)", d.name, b.level_text())
        }
        (true, Some(b)) => format!("{}: {}", d.name, b.level_text()),
        (true, None) => d.name.clone(),
    }
}

impl Tray for PeriphTray {
    fn id(&self) -> String {
        "io.github.periph".into()
    }

    fn title(&self) -> String {
        "Periph".into()
    }

    fn icon_name(&self) -> String {
        // Warn at a glance when anything is about to run out.
        let low = self
            .devices
            .iter()
            .any(|d| d.connected && d.battery.is_some_and(|b| b.percent <= 15));
        if low {
            "battery-caution-symbolic"
        } else {
            "input-gaming-symbolic"
        }
        .into()
    }

    fn tool_tip(&self) -> ToolTip {
        let lines: Vec<String> = self.devices.iter().map(summary).collect();
        ToolTip {
            title: "Periph".into(),
            description: if lines.is_empty() {
                "No devices".into()
            } else {
                lines.join("\n")
            },
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayCommand::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = self
            .devices
            .iter()
            .map(|d| {
                StandardItem {
                    label: summary(d).replace('_', "__"),
                    enabled: false,
                    icon_name: d.kind.icon_name().into(),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }
        items.push(
            StandardItem {
                label: "Open Periph".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCommand::Show);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}
