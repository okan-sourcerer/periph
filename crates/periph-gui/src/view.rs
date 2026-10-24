//! Widgets for one device: battery, info, settings and button remapping.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use periph_core::{DaemonProxy, Device, RemapProfile, Setting, SettingKind, button_label};

/// Sends changes to the daemon and reports failures as toasts.
#[derive(Clone)]
pub struct Client {
    pub proxy: DaemonProxy<'static>,
    pub toasts: adw::ToastOverlay,
}

impl Client {
    fn set_setting(&self, id: &str, key: &str, value: i32) {
        let (this, id, key) = (self.clone(), id.to_string(), key.to_string());
        glib::spawn_future_local(async move {
            if let Err(e) = this.proxy.set_setting(&id, &key, value).await {
                this.toast(&format!("Couldn't apply setting: {}", short_error(&e)));
            }
        });
    }

    fn set_remap(&self, id: &str, profile: &RemapProfile) {
        let Ok(json) = serde_json::to_string(profile) else {
            return;
        };
        let (this, id) = (self.clone(), id.to_string());
        glib::spawn_future_local(async move {
            if let Err(e) = this.proxy.set_remap(&id, &json).await {
                this.toast(&format!("Couldn't save remap: {}", short_error(&e)));
            }
        });
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }
}

fn short_error(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

pub fn device_group(
    d: &Device,
    remap: Option<RemapProfile>,
    client: &Client,
) -> adw::PreferencesGroup {
    let mut description = d.transport.label().to_string();
    if !d.connected {
        description.push_str(" · not connected");
    }
    let group = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(&d.name).as_str())
        .description(description)
        .build();
    let icon = gtk::Image::from_icon_name(d.kind.icon_name());
    icon.set_pixel_size(24);
    group.set_header_suffix(Some(&icon));

    if let Some(b) = d.battery {
        let row = adw::ActionRow::builder().title("Battery").build();
        row.set_subtitle(&match b.charging {
            Some(true) => format!("{}% · charging", b.percent),
            _ => format!("{}%", b.percent),
        });
        let bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(100.0)
            .value(b.percent as f64)
            .valign(gtk::Align::Center)
            .width_request(140)
            .build();
        row.add_suffix(&bar);
        group.add(&row);
    }

    for (key, value) in &d.info {
        let row = adw::ActionRow::builder()
            .title(key)
            .subtitle(glib::markup_escape_text(value).as_str())
            .build();
        row.add_css_class("property");
        group.add(&row);
    }

    for s in &d.settings {
        group.add(&setting_row(&d.id, s, client));
    }

    if !d.buttons.is_empty() {
        group.add(&remap_row(d, remap.unwrap_or_default(), client));
    }
    group
}

fn setting_row(id: &str, s: &Setting, client: &Client) -> gtk::Widget {
    let (id, key) = (id.to_string(), s.key.clone());
    match &s.kind {
        SettingKind::Toggle => {
            let row = adw::SwitchRow::builder()
                .title(&s.label)
                .active(s.value != 0)
                .build();
            let client = client.clone();
            row.connect_active_notify(move |r| client.set_setting(&id, &key, r.is_active() as i32));
            row.upcast()
        }
        SettingKind::Range { min, max } => {
            let row = adw::SpinRow::with_range(*min as f64, *max as f64, 1.0);
            row.set_title(&s.label);
            row.set_value(s.value as f64);
            // Each write is a few radio round-trips; wait until the value settles.
            let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
            let client = client.clone();
            row.connect_value_notify(move |r| {
                let value = r.value() as i32;
                if let Some(old) = pending.borrow_mut().take() {
                    old.remove();
                }
                let (client, id, key, slot) =
                    (client.clone(), id.clone(), key.clone(), pending.clone());
                let source = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(400),
                    move || {
                        slot.borrow_mut().take();
                        client.set_setting(&id, &key, value);
                    },
                );
                *pending.borrow_mut() = Some(source);
            });
            row.upcast()
        }
        SettingKind::Choice { options } => {
            let labels: Vec<&str> = options.iter().map(|(_, l)| l.as_str()).collect();
            let row = adw::ComboRow::builder()
                .title(&s.label)
                .model(&gtk::StringList::new(&labels))
                .build();
            if let Some(i) = options.iter().position(|(v, _)| *v == s.value) {
                row.set_selected(i as u32);
            }
            let values: Vec<i32> = options.iter().map(|(v, _)| *v).collect();
            let client = client.clone();
            row.connect_selected_notify(move |r| {
                if let Some(v) = values.get(r.selected() as usize) {
                    client.set_setting(&id, &key, *v);
                }
            });
            row.upcast()
        }
    }
}

/// Choice index 0 = default, 1 = disabled, 2.. = map to `buttons[i - 2]`.
fn remap_row(d: &Device, profile: RemapProfile, client: &Client) -> gtk::Widget {
    let expander = adw::ExpanderRow::builder()
        .title("Button remapping")
        .subtitle("Takes over the controller and exposes a remapped virtual one")
        .show_enable_switch(true)
        .enable_expansion(profile.enabled)
        .build();

    let profile = Rc::new(RefCell::new(profile));
    let buttons = d.buttons.clone();
    let mut labels = vec!["Default".to_string(), "Disabled".to_string()];
    labels.extend(buttons.iter().map(|b| button_label(*b)));
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();

    {
        let (profile, client, id) = (profile.clone(), client.clone(), d.id.clone());
        expander.connect_enable_expansion_notify(move |e| {
            profile.borrow_mut().enabled = e.enables_expansion();
            client.set_remap(&id, &profile.borrow());
        });
    }

    for &source in &buttons {
        let row = adw::ComboRow::builder()
            .title(button_label(source))
            .model(&gtk::StringList::new(&labels))
            .build();
        let selected = match profile.borrow().map.get(&source) {
            None => 0,
            Some(0) => 1,
            Some(target) => buttons
                .iter()
                .position(|b| b == target)
                .map_or(0, |i| i + 2),
        };
        row.set_selected(selected as u32);

        let (profile, client, id, buttons) = (
            profile.clone(),
            client.clone(),
            d.id.clone(),
            buttons.clone(),
        );
        row.connect_selected_notify(move |r| {
            let mut p = profile.borrow_mut();
            match r.selected() as usize {
                0 => {
                    p.map.remove(&source);
                }
                1 => {
                    p.map.insert(source, 0);
                }
                i => match buttons.get(i - 2) {
                    Some(&t) if t == source => {
                        p.map.remove(&source);
                    }
                    Some(&t) => {
                        p.map.insert(source, t);
                    }
                    None => return,
                },
            }
            client.set_remap(&id, &p);
        });
        expander.add_row(&row);
    }
    expander.upcast()
}
