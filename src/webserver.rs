use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::database::{CommandCode, DbTask};
use crate::onewire::{DeviceStatus, DeviceStatusKind, OneWireTask, StatusQuery, TaskCommand};
use flume::Sender;
use rocket::response::content;
use rocket::{get, routes, State};
use simplelog::*;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

//shared with all route handlers via Rocket's State/manage(); a plain tuple
//of one-way command channels, plus status_tx for the request/response
//status query below
type Transmitters = Arc<Mutex<(Sender<OneWireTask>, Sender<DbTask>, Sender<StatusQuery>)>>;

pub struct WebServer {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
    pub db_transmitter: Sender<DbTask>,
    pub status_transmitter: Sender<StatusQuery>,
}

#[get("/hello")]
pub fn hello() -> &'static str {
    "Hello world!"
}

#[get("/reload")]
pub fn reload(transmitters: &State<Transmitters>) -> String {
    let task = DbTask {
        command: CommandCode::ReloadDevices,
        value: None,
    };
    if let Ok(trans) = transmitters.lock() {
        let _ = trans.1.send(task);
    }

    "Reloading config...".to_string()
}

#[get("/fan-on")]
pub fn fan_on(transmitters: &State<Transmitters>) -> String {
    let task = OneWireTask {
        command: TaskCommand::TurnOnProlong,
        id_relay: Some(14),
        tag_group: None,
        id_yeelight: None,
        duration: Some(Duration::from_secs(60 * 5)),
    };
    if let Ok(trans) = transmitters.lock() {
        let _ = trans.0.send(task);
    }

    "Turning ON fan".to_string()
}

#[get("/fan-off")]
pub fn fan_off(transmitters: &State<Transmitters>) -> String {
    let task = OneWireTask {
        command: TaskCommand::TurnOff,
        id_relay: Some(14),
        tag_group: None,
        id_yeelight: None,
        duration: None,
    };
    if let Ok(trans) = transmitters.lock() {
        let _ = trans.0.send(task);
    }

    "Turning OFF fan".to_string()
}

//Lists every relay/yeelight currently away from its default (off,
//non-override) state -- e.g. a relay a PIR turned on and how much longer
//it'll stay on, or a switch that's put something into override an hour ago.
//
//The actual device state lives exclusively inside the onewire coordinator
//task (no lock, by design -- see OneWire's doc comment in onewire.rs), so
//this handler can't just read it directly: it sends a StatusQuery over the
//shared channel and awaits a one-shot reply. If onewire is disabled
//(disable_onewire in hard.conf) or busy for more than a few seconds, nobody
//will ever answer that query, so the wait is capped with a timeout rather
//than hanging the request forever.
#[get("/status")]
pub async fn status(transmitters: &State<Transmitters>) -> content::RawHtml<String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    let sent = match transmitters.lock() {
        Ok(trans) => trans.2.send(StatusQuery { reply_tx }).is_ok(),
        Err(_) => false,
    };

    if !sent {
        return content::RawHtml(render_status_page(None));
    }

    match tokio::time::timeout(Duration::from_secs(5), reply_rx).await {
        Ok(Ok(devices)) => content::RawHtml(render_status_page(Some(devices))),
        //either the timeout elapsed, or the sender was dropped without
        //replying (e.g. onewire is disabled and nothing ever picks up
        //StatusQuery messages from that channel) -- both look the same to
        //the visitor: "couldn't get a fresh status right now"
        _ => content::RawHtml(render_status_page(None)),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

//remaining is a Duration computed from a monotonic Instant, so it carries no
//wall-clock meaning by itself -- add it to the current wall-clock time here,
//right when we're about to display it, to get an ETA that's accurate to
//within however long this request took to handle (milliseconds, in practice)
fn format_until(remaining: Duration) -> String {
    match chrono::Duration::from_std(remaining) {
        Ok(d) => {
            let eta = chrono::Local::now() + d;
            format!(
                "{} (in {})",
                eta.format("%Y-%m-%d %H:%M:%S"),
                humantime::format_duration(remaining)
            )
        }
        Err(_) => "-".to_string(),
    }
}

fn render_status_page(devices: Option<Vec<DeviceStatus>>) -> String {
    let body = match devices {
        None => {
            r#"<p class="error">Could not reach the onewire task (it may be disabled, or busy) -- try again in a moment.</p>"#
                .to_string()
        }
        Some(devices) if devices.is_empty() => {
            r#"<p>Nothing is currently in a non-default state.</p>"#.to_string()
        }
        Some(mut devices) => {
            devices.sort_by(|a, b| a.name.cmp(&b.name));
            let mut rows = String::new();
            for d in &devices {
                let kind = match d.kind {
                    DeviceStatusKind::Relay => "relay",
                    DeviceStatusKind::Yeelight => "yeelight",
                };
                let state = if d.is_on {
                    r#"<span class="on">ON</span>"#
                } else {
                    r#"<span class="off">off</span>"#
                };
                let mode = if d.override_mode {
                    "override"
                } else {
                    "auto"
                };
                let until = match d.remaining {
                    Some(remaining) => format_until(remaining),
                    //override with no stop_after means "stays like this
                    //until manually changed" -- there's no ETA to show,
                    //so use an infinity symbol rather than a bare "-",
                    //which would be indistinguishable from "not toggled"
                    None if d.override_mode => "∞".to_string(),
                    None => "-".to_string(),
                };
                rows.push_str(&format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    html_escape(&d.name),
                    kind,
                    state,
                    mode,
                    until,
                ));
            }
            format!(
                "<table>\n<tr><th>Name</th><th>Type</th><th>State</th><th>Mode</th><th>Until</th></tr>\n{}</table>",
                rows
            )
        }
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>hard - device status</title>
<style>
body {{ font-family: sans-serif; margin: 2em; color: #222; }}
table {{ border-collapse: collapse; width: 100%; max-width: 60em; }}
th, td {{ border: 1px solid #ccc; padding: 0.4em 0.8em; text-align: left; }}
th {{ background: #eee; }}
.on {{ color: #1a7a1a; font-weight: bold; }}
.off {{ color: #888; }}
.error {{ color: #a00; }}
</style>
</head>
<body>
<h1>Devices in a non-default state</h1>
{}
</body>
</html>
"#,
        body
    )
}

impl WebServer {
    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        //put a transmitter into a mutex and share to handlers
        let transmitters: Transmitters = Arc::new(Mutex::new((
            self.ow_transmitter.clone(),
            self.db_transmitter.clone(),
            self.status_transmitter.clone(),
        )));

        info!("{}: Starting task", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            let result = rocket::build()
                .mount("/cmd", routes![hello, reload, fan_on, fan_off, status])
                .manage(transmitters.clone())
                .launch()
                .await;
            result.expect("server failed unexpectedly");

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        info!("{}: task stopped", self.name);
        Ok(())
    }
}
