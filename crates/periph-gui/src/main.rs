//! periph: GTK window and tray icon, a client of periphd.

mod tray;
mod view;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use futures_lite::StreamExt;
use gtk::glib;
use ksni::TrayMethods;
use periph_core::{DaemonProxy, Device, DeviceKind, RemapProfile};

use crate::tray::{PeriphTray, TrayCommand};
use crate::view::Client;

const APP_ID: &str = "io.github.periph";
const POLL: Duration = Duration::from_secs(5);

struct Ui {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    list: gtk::Box,
    last: RefCell<Option<Vec<Device>>>,
    tray: RefCell<Option<ksni::Handle<PeriphTray>>>,
}

fn main() -> glib::ExitCode {
    // `--hidden` starts in the tray only (for autostart); GTK doesn't know it.
    let mut args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hidden");
    args.retain(|a| a != "--hidden");

    let app = adw::Application::builder().application_id(APP_ID).build();
    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    app.connect_activate(move |app| {
        if let Some(ui) = ui.borrow().as_ref() {
            ui.window.present();
            return;
        }
        let built = build(app);
        if !start_hidden {
            built.window.present();
        }
        *ui.borrow_mut() = Some(built);
    });
    app.run_with_args(&args)
}

fn build(app: &adw::Application) -> Rc<Ui> {
    // Keep running in the tray when the window is closed.
    std::mem::forget(app.hold());

    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh now"));
    let header = adw::HeaderBar::new();
    header.pack_start(&refresh);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 24);
    list.set_margin_top(24);
    list.set_margin_bottom(24);
    list.set_margin_start(12);
    list.set_margin_end(12);
    let clamp = adw::Clamp::builder().maximum_size(640).child(&list).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .vexpand(true)
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("devices"));
    stack.add_named(
        &adw::StatusPage::builder()
            .icon_name("input-gaming-symbolic")
            .title("No devices found")
            .description("Plug in a dongle or connect a Bluetooth device.")
            .build(),
        Some("empty"),
    );
    stack.add_named(
        &adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Daemon not running")
            .description("Start it with <tt>systemctl --user start periphd</tt>")
            .build(),
        Some("nodaemon"),
    );
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .halign(gtk::Align::Center)
        .build();
    stack.add_named(&spinner, Some("loading"));
    stack.set_visible_child_name("loading");

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Periph")
        .default_width(560)
        .default_height(720)
        .hide_on_close(true)
        .content(&view)
        .build();

    let ui = Rc::new(Ui {
        window,
        stack,
        list,
        last: RefCell::default(),
        tray: RefCell::default(),
    });

    let (tray_tx, tray_rx) = async_channel::unbounded();
    glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            match (PeriphTray {
                devices: Vec::new(),
                tx: tray_tx,
            })
            .spawn()
            .await
            {
                Ok(handle) => *ui.tray.borrow_mut() = Some(handle),
                Err(e) => eprintln!("periph: no tray icon ({e})"),
            }
        }
    });
    glib::spawn_future_local({
        let (ui, app) = (ui.clone(), app.clone());
        async move {
            while let Ok(cmd) = tray_rx.recv().await {
                match cmd {
                    TrayCommand::Show => ui.window.present(),
                    TrayCommand::Quit => app.quit(),
                }
            }
        }
    });

    glib::spawn_future_local(run(ui.clone(), toasts, refresh));
    ui
}

/// Connect to the daemon, then re-render whenever it signals a change (or
/// every few seconds, which also notices the daemon going away).
async fn run(ui: Rc<Ui>, toasts: adw::ToastOverlay, refresh: gtk::Button) {
    let proxy = loop {
        match connect().await {
            Ok(p) => break p,
            Err(e) => {
                eprintln!("periph: session bus: {e}");
                ui.stack.set_visible_child_name("nodaemon");
                glib::timeout_future(POLL).await;
            }
        }
    };
    let client = Client {
        proxy: proxy.clone(),
        toasts,
    };
    refresh.connect_clicked({
        let client = client.clone();
        move |_| {
            let client = client.clone();
            glib::spawn_future_local(async move {
                if client.proxy.refresh().await.is_err() {
                    client.toast("Daemon not running");
                }
            });
        }
    });

    let mut changes = proxy.receive_changed().await.ok();
    loop {
        reload(&ui, &client).await;
        match changes.as_mut() {
            Some(stream) => {
                let _ = glib::future_with_timeout(POLL, stream.next()).await;
            }
            None => glib::timeout_future(POLL).await,
        }
    }
}

async fn connect() -> zbus::Result<DaemonProxy<'static>> {
    let conn = zbus::Connection::session().await?;
    DaemonProxy::new(&conn).await
}

async fn reload(ui: &Rc<Ui>, client: &Client) {
    let devices: Vec<Device> = match client.proxy.devices().await {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => {
            ui.last.borrow_mut().take();
            ui.stack.set_visible_child_name("nodaemon");
            return;
        }
    };
    if ui.last.borrow().as_ref() == Some(&devices) {
        return;
    }

    let mut remaps = HashMap::new();
    for d in devices
        .iter()
        .filter(|d| d.kind == DeviceKind::Gamepad && !d.buttons.is_empty())
    {
        if let Ok(json) = client.proxy.remap(&d.id).await {
            remaps.insert(
                d.id.clone(),
                serde_json::from_str::<RemapProfile>(&json).unwrap_or_default(),
            );
        }
    }

    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }
    for d in &devices {
        ui.list
            .append(&view::device_group(d, remaps.remove(&d.id), client));
    }
    ui.stack.set_visible_child_name(if devices.is_empty() {
        "empty"
    } else {
        "devices"
    });

    if let Some(tray) = ui.tray.borrow().clone() {
        let snapshot = devices.clone();
        glib::spawn_future_local(async move {
            tray.update(|t| t.devices = snapshot).await;
        });
    }
    *ui.last.borrow_mut() = Some(devices);
}
