//! periphd: owns device access and publishes it on the session bus.

mod bluez;
mod gamepad;
mod hidraw;
mod razer;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use log::{info, warn};
use periph_core::{BUS_NAME, Device, OBJECT_PATH, RemapProfile};
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::gamepad::Remapper;
use crate::razer::BlackShark;

const SCAN_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    devices: Mutex<Vec<Device>>,
    razers: Mutex<HashMap<String, Arc<Mutex<BlackShark>>>>,
    remaps: Mutex<BTreeMap<String, RemapProfile>>,
    remappers: Mutex<HashMap<String, Remapper>>,
}

impl State {
    /// Everything that needs blocking I/O: hidraw dongles and evdev pads.
    fn scan_local(&self, force: bool) -> (Vec<Device>, Vec<gamepad::Pad>) {
        let mut devices = Vec::new();

        let nodes: Vec<_> = hidraw::enumerate()
            .into_iter()
            .filter(razer::matches)
            .collect();
        let razers: Vec<_> = {
            let mut known = self.razers.lock().unwrap();
            let mut keep = HashMap::new();
            for (i, node) in nodes.iter().enumerate() {
                let id = if i == 0 {
                    "razer:1532:0555".to_string()
                } else {
                    format!("razer:1532:0555#{}", i + 1)
                };
                let entry = match known.remove(&id) {
                    Some(d) if d.lock().unwrap().devnode == node.devnode => d,
                    _ => Arc::new(Mutex::new(BlackShark::new(
                        id.clone(),
                        node.devnode.clone(),
                    ))),
                };
                keep.insert(id, entry);
            }
            *known = keep;
            known.values().cloned().collect()
        };
        for r in razers {
            let mut r = r.lock().unwrap();
            r.poll(force);
            devices.push(r.snapshot());
        }

        let pads = gamepad::enumerate();
        self.sync_remappers(&pads);
        (devices, pads)
    }

    /// Start, restart or stop remap threads to match profiles and present pads.
    fn sync_remappers(&self, pads: &[gamepad::Pad]) {
        let remaps = self.remaps.lock().unwrap();
        let mut running = self.remappers.lock().unwrap();
        let wanted: HashMap<&str, (&gamepad::Pad, &RemapProfile)> = pads
            .iter()
            .filter_map(|p| {
                let profile = remaps.get(&p.device.id)?;
                profile
                    .active()
                    .then_some((p.device.id.as_str(), (p, profile)))
            })
            .collect();
        running.retain(|id, r| {
            wanted.get(id.as_str()).is_some_and(|(pad, profile)| {
                !r.finished() && r.path == pad.path && r.profile == **profile
            })
        });
        for (id, (pad, profile)) in wanted {
            if !running.contains_key(id) {
                running.insert(
                    id.to_string(),
                    Remapper::start(pad.path.clone(), profile.clone(), pad.device.layout),
                );
            }
        }
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("periph/remaps.json")
}

fn load_remaps() -> BTreeMap<String, RemapProfile> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_remaps(remaps: &BTreeMap<String, RemapProfile>) -> std::io::Result<()> {
    let path = config_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(remaps)?)?;
    std::fs::rename(tmp, path)
}

struct Api {
    state: Arc<State>,
    kick: mpsc::UnboundedSender<bool>,
}

#[zbus::interface(name = "io.github.periph.Daemon1")]
impl Api {
    async fn devices(&self) -> String {
        serde_json::to_string(&*self.state.devices.lock().unwrap()).unwrap_or_else(|_| "[]".into())
    }

    async fn set_setting(&self, id: String, key: String, value: i32) -> fdo::Result<()> {
        let razer = self.state.razers.lock().unwrap().get(&id).cloned();
        let razer =
            razer.ok_or_else(|| fdo::Error::InvalidArgs(format!("no configurable device {id}")))?;
        tokio::task::spawn_blocking(move || razer.lock().unwrap().set(&key, value))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?
            .map_err(fdo::Error::Failed)?;
        let _ = self.kick.send(false);
        Ok(())
    }

    async fn remap(&self, id: String) -> String {
        let profile = self
            .state
            .remaps
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        serde_json::to_string(&profile).unwrap_or_default()
    }

    async fn set_remap(&self, id: String, profile_json: String) -> fdo::Result<()> {
        let profile: RemapProfile = serde_json::from_str(&profile_json)
            .map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let mut remaps = self.state.remaps.lock().unwrap();
        if profile == RemapProfile::default() {
            remaps.remove(&id);
        } else {
            remaps.insert(id, profile);
        }
        save_remaps(&remaps).map_err(|e| fdo::Error::IOError(e.to_string()))?;
        drop(remaps);
        let _ = self.kick.send(false);
        Ok(())
    }

    async fn refresh(&self) {
        let _ = self.kick.send(true);
    }

    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Merge BlueZ entries into the local list: a BT pad already found through
/// evdev borrows its battery instead of being listed twice.
fn merge(
    mut local: Vec<Device>,
    mut pads: Vec<gamepad::Pad>,
    bt: Vec<bluez::BtDevice>,
) -> Vec<Device> {
    for b in bt {
        if let Some(pad) = pads
            .iter_mut()
            .find(|p| p.uniq.as_deref() == Some(&b.address))
        {
            pad.device.battery = pad.device.battery.or(b.device.battery);
            continue;
        }
        local.push(b.device);
    }
    local.extend(pads.into_iter().map(|p| p.device));
    local
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let state = Arc::new(State {
        remaps: Mutex::new(load_remaps()),
        ..Default::default()
    });
    let (kick, mut kicks) = mpsc::unbounded_channel::<bool>();

    let session = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(
            OBJECT_PATH,
            Api {
                state: state.clone(),
                kick,
            },
        )?
        .build()
        .await
        .context(
            "could not claim io.github.periph.Daemon on the session bus (is periphd already running?)",
        )?;
    let system = Connection::system().await.ok();
    if system.is_none() {
        warn!("no system bus: Bluetooth devices won't be listed");
    }
    info!("periphd ready on {BUS_NAME}");

    let iface = session
        .object_server()
        .interface::<_, Api>(OBJECT_PATH)
        .await?;
    let mut force = true;
    loop {
        let st = state.clone();
        let (local, pads) = tokio::task::spawn_blocking(move || st.scan_local(force)).await?;
        let bt = match &system {
            Some(sys) => bluez::connected(sys).await.unwrap_or_else(|e| {
                warn!("BlueZ: {e}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        let devices = merge(local, pads, bt);
        let changed = {
            let mut current = state.devices.lock().unwrap();
            let changed = *current != devices;
            *current = devices;
            changed
        };
        if changed {
            Api::changed(iface.signal_emitter()).await?;
        }

        tokio::select! {
            _ = tokio::time::sleep(SCAN_INTERVAL) => force = false,
            k = kicks.recv() => force = k.unwrap_or(false),
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    state.remappers.lock().unwrap().clear();
    Ok(())
}
