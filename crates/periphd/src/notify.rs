//! Low-battery desktop notifications (org.freedesktop.Notifications).
//!
//! Each device notifies once when it drops to "low" and once more at
//! "critical". It re-arms only after charging or climbing back above
//! `REARM_PERCENT`, so a level hovering around a threshold doesn't spam.

use std::collections::HashMap;

use log::warn;
use periph_core::{Battery, Device};
use zbus::Connection;
use zbus::zvariant::Value;

const LOW_PERCENT: u8 = 20;
const CRITICAL_PERCENT: u8 = 10;
const REARM_PERCENT: u8 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum Tier {
    #[default]
    Ok,
    Low,
    Critical,
}

/// Where a battery reading puts a device, given where it was before.
fn tier(battery: Battery, previous: Tier) -> Tier {
    if battery.charging == Some(true) {
        return Tier::Ok;
    }
    // Level-only batteries (Switch pads) report "Low" as 25: warn on it too.
    let low = if battery.coarse {
        REARM_PERCENT
    } else {
        LOW_PERCENT
    };
    let now = match battery.percent {
        p if p <= CRITICAL_PERCENT => Tier::Critical,
        p if p <= low => Tier::Low,
        _ => Tier::Ok,
    };
    // Don't step back down until the level has clearly recovered.
    if now < previous && battery.percent <= REARM_PERCENT {
        previous
    } else {
        now
    }
}

#[derive(Default)]
struct Seen {
    tier: Tier,
    /// Server-side id, so a critical warning replaces the low one.
    notification: u32,
}

pub struct Notifier {
    session: Connection,
    seen: HashMap<String, Seen>,
}

impl Notifier {
    pub fn new(session: Connection) -> Self {
        Self {
            session,
            seen: HashMap::new(),
        }
    }

    pub async fn update(&mut self, devices: &[Device]) {
        for d in devices {
            let Some(battery) = d.battery.filter(|_| d.connected) else {
                continue;
            };
            let seen = self.seen.entry(d.id.clone()).or_default();
            let now = tier(battery, seen.tier);
            let rose = now > seen.tier;
            seen.tier = now;
            if !rose {
                continue;
            }
            let replaces = seen.notification;
            match send(&self.session, d, battery, now, replaces).await {
                Ok(id) => {
                    if let Some(seen) = self.seen.get_mut(&d.id) {
                        seen.notification = id;
                    }
                }
                Err(e) => warn!("couldn't show battery notification: {e}"),
            }
        }
    }
}

async fn send(
    session: &Connection,
    d: &Device,
    battery: Battery,
    tier: Tier,
    replaces: u32,
) -> zbus::Result<u32> {
    let (summary, icon, urgency) = match tier {
        Tier::Critical => (format!("{} battery critical", d.name), "battery-empty", 2u8),
        _ => (format!("{} battery low", d.name), "battery-caution", 1u8),
    };
    let body = format!("{} left. Charge it soon.", battery.level_text());
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::U8(urgency));
    hints.insert("category", Value::from("device"));
    let reply = session
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Periph",
                replaces,
                icon,
                summary.as_str(),
                body.as_str(),
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .await?;
    reply.body().deserialize::<u32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(percent: u8, charging: Option<bool>) -> Battery {
        Battery {
            percent,
            charging,
            coarse: false,
        }
    }

    #[test]
    fn crossing_thresholds() {
        assert_eq!(tier(b(50, None), Tier::Ok), Tier::Ok);
        assert_eq!(tier(b(20, None), Tier::Ok), Tier::Low);
        assert_eq!(tier(b(10, Some(false)), Tier::Low), Tier::Critical);
    }

    #[test]
    fn hovering_does_not_rearm() {
        // 21% after a low warning: still low, so no second notification.
        assert_eq!(tier(b(21, None), Tier::Low), Tier::Low);
        assert_eq!(tier(b(26, None), Tier::Low), Tier::Ok);
    }

    #[test]
    fn charging_rearms() {
        assert_eq!(tier(b(15, Some(true)), Tier::Low), Tier::Ok);
    }

    #[test]
    fn coarse_levels_map_to_tiers() {
        let low = Battery {
            percent: 25,
            charging: None,
            coarse: true,
        };
        let critical = Battery { percent: 5, ..low };
        let normal = Battery { percent: 50, ..low };
        assert_eq!(tier(low, Tier::Ok), Tier::Low);
        assert_eq!(tier(critical, Tier::Low), Tier::Critical);
        assert_eq!(tier(normal, Tier::Low), Tier::Ok);
    }
}
