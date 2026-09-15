use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::database::{CommandCode, DbTask};
use crate::deye::{Category, DeyeStatusQuery, Parameter};
use crate::onewire::{DeviceStatus, DeviceStatusKind, OneWireTask, StatusQuery, TaskCommand};
use crate::util::HumantimeSecs;
use flume::Sender;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::response::content;
use rocket::response::Redirect;
use rocket::{get, routes, Request, Response, State};
use simplelog::*;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

//Where all routes below are mounted. Every generated link/redirect in this
//file is built from this constant rather than hardcoded, so the two can't
//drift apart (a hardcoded "/cmd/..." link against a "/" mount is exactly how
//the status page's action links ended up 404ing). Empty means "mounted at
//the root"; set it to e.g. "/cmd" to move everything under a prefix.
const MOUNT_BASE: &str = "";

fn mount_point() -> &'static str {
    if MOUNT_BASE.is_empty() {
        "/"
    } else {
        MOUNT_BASE
    }
}

fn status_uri() -> String {
    format!("{}/status", MOUNT_BASE)
}

fn deye_uri() -> String {
    format!("{}/deye", MOUNT_BASE)
}

//Rocket's own built-in request logging ("GET /status ...", "Matched: ...",
//"Outcome: ...", "Response succeeded.") is all-or-nothing -- there's no way
//to exclude a single route from it. /status gets polled far more often than
//it's interesting to see in the log, so instead we turn Rocket's built-in
//logging off entirely (see worker() below) and replace it with this: one
//line per request, after the fact (so it has the real response status),
//skipping whatever paths are considered "quiet".
fn is_quiet_path(path: &str) -> bool {
    path == status_uri() || path == deye_uri()
}

pub struct RequestLogger;

#[rocket::async_trait]
impl Fairing for RequestLogger {
    fn info(&self) -> Info {
        Info {
            name: "Request Logger",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if is_quiet_path(request.uri().path().as_str()) {
            return;
        }
        info!(
            "{} {} -> {}",
            request.method(),
            request.uri(),
            response.status()
        );
    }
}

//shared with all route handlers via Rocket's State/manage(); a plain tuple
//of one-way command channels, plus status_tx for the request/response
//status query below
type Transmitters = Arc<Mutex<(Sender<OneWireTask>, Sender<DbTask>, Sender<StatusQuery>)>>;

pub struct WebServer {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
    pub db_transmitter: Sender<DbTask>,
    pub status_transmitter: Sender<StatusQuery>,
    pub deye_status_transmitter: Sender<DeyeStatusQuery>,
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

//Sends a StatusQuery over the shared channel and awaits its one-shot reply.
//The actual device state lives exclusively inside the onewire coordinator
//task (no lock, by design -- see OneWire's doc comment in onewire.rs), so
//this is the only way any handler gets to see it. If onewire is disabled
//(disable_onewire in hard.conf) or busy for more than a few seconds, nobody
//will ever answer, so the wait is capped rather than hanging the request
//forever.
async fn query_status(transmitters: &State<Transmitters>) -> Option<Vec<DeviceStatus>> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    let sent = match transmitters.lock() {
        Ok(trans) => trans.2.send(StatusQuery { reply_tx }).is_ok(),
        Err(_) => false,
    };
    if !sent {
        return None;
    }

    match tokio::time::timeout(Duration::from_secs(5), reply_rx).await {
        Ok(Ok(devices)) => Some(devices),
        //either the timeout elapsed, or the sender was dropped without
        //replying (e.g. onewire is disabled and nothing ever picks up
        //StatusQuery messages from that channel)
        _ => None,
    }
}

//Lists every configured relay and yeelight, plus a summary of which ones are
//currently away from their default (off, non-override) state -- e.g. a
//relay a PIR turned on and how much longer it'll stay on, or a switch
//that's put something into override an hour ago. Each row's Actions column
//links to device_action() below for direct ON/OFF/TOGGLE control.
#[get("/status")]
pub async fn status(transmitters: &State<Transmitters>) -> content::RawHtml<String> {
    let devices = query_status(transmitters).await;
    content::RawHtml(render_status_page(devices))
}

//Lists every parameter from the deye inverter's last successful poll,
//grouped by category (PV/Battery/Grid/...). Same request/response pattern
//as /status: deye's live values live exclusively inside Deye::worker()'s own
//state, so this sends a DeyeStatusQuery and awaits its one-shot reply,
//capped by a timeout in case the deye task isn't configured/running at all
//(disable via omitting [deye] host in hard.conf) or is busy reconnecting.
#[get("/deye")]
pub async fn deye_status(
    deye_transmitter: &State<Sender<DeyeStatusQuery>>,
) -> content::RawHtml<String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let sent = deye_transmitter.send(DeyeStatusQuery { reply_tx }).is_ok();

    if !sent {
        return content::RawHtml(render_deye_page(None));
    }

    match tokio::time::timeout(Duration::from_secs(5), reply_rx).await {
        Ok(Ok(params)) => content::RawHtml(render_deye_page(Some(params))),
        _ => content::RawHtml(render_deye_page(None)),
    }
}

fn kind_str(kind: DeviceStatusKind) -> &'static str {
    match kind {
        DeviceStatusKind::Relay => "relay",
        DeviceStatusKind::Yeelight => "yeelight",
    }
}

fn kind_matches(kind: DeviceStatusKind, kind_param: &str) -> bool {
    kind_str(kind) == kind_param
}

//Direct control from the status page: /device/<relay|yeelight>/<id>/<on|off|toggle>.
//"toggle" queries current status first (same mechanism as the page itself)
//to decide which way to flip -- there's a small window between that read and
//the command actually being applied where a PIR or another click could beat
//it to it, but for a manual convenience button that's an acceptable
//trade-off rather than reason to add a dedicated toggle command in onewire.rs.
//Uses TurnOnProlong/TurnOff with duration: None so onewire.rs falls back to
//each device's own configured pir_hold_secs/switch_hold_secs, the same as
//any other remote-triggered action.
#[get("/device/<kind>/<id>/<action>")]
pub async fn device_action(
    transmitters: &State<Transmitters>,
    kind: String,
    id: i32,
    action: String,
) -> Redirect {
    let (id_relay, id_yeelight) = match kind.as_str() {
        "relay" => (Some(id), None),
        "yeelight" => (None, Some(id)),
        _ => return Redirect::to(status_uri()),
    };

    let command = match action.as_str() {
        "on" => Some(TaskCommand::TurnOnProlong),
        "off" => Some(TaskCommand::TurnOff),
        "toggle" => {
            let is_on = query_status(transmitters)
                .await
                .and_then(|devices| {
                    devices
                        .into_iter()
                        .find(|d| d.id == id && kind_matches(d.kind, kind.as_str()))
                })
                .map(|d| d.is_on)
                .unwrap_or(false);
            Some(if is_on {
                TaskCommand::TurnOff
            } else {
                TaskCommand::TurnOnProlong
            })
        }
        _ => None,
    };

    if let Some(command) = command {
        let task = OneWireTask {
            command,
            id_relay,
            tag_group: None,
            id_yeelight,
            duration: None,
        };
        if let Ok(trans) = transmitters.lock() {
            let _ = trans.0.send(task);
        }
    }

    Redirect::to(status_uri())
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
                remaining.humantime_secs()
            )
        }
        Err(_) => "-".to_string(),
    }
}

fn render_table(devices: &[&DeviceStatus], empty_message: &str) -> String {
    if devices.is_empty() {
        return format!("<p>{}</p>", empty_message);
    }

    let mut rows = String::new();
    for d in devices {
        let kind = kind_str(d.kind);
        let state = if d.is_on {
            r#"<span class="on">ON</span>"#
        } else {
            r#"<span class="off">off</span>"#
        };
        let mode = if d.override_mode { "override" } else { "auto" };
        let until = match d.remaining {
            Some(remaining) => format_until(remaining),
            //override with no stop_after means "stays like this until
            //manually changed" -- there's no ETA to show, so use an
            //infinity symbol rather than a bare "-", which would be
            //indistinguishable from "not toggled"
            None if d.override_mode => "∞".to_string(),
            None => "-".to_string(),
        };
        let actions = format!(
            r#"<a href="{base}/device/{kind}/{id}/on">ON</a> | <a href="{base}/device/{kind}/{id}/off">OFF</a> | <a href="{base}/device/{kind}/{id}/toggle">TOGGLE</a>"#,
            base = MOUNT_BASE,
            kind = kind,
            id = d.id,
        );
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            html_escape(&d.name),
            kind,
            state,
            mode,
            until,
            actions,
        ));
    }

    format!(
        "<table>\n<tr><th>Name</th><th>Type</th><th>State</th><th>Mode</th><th>Until</th><th>Actions</th></tr>\n{}</table>",
        rows
    )
}

fn category_label(cat: Category) -> &'static str {
    match cat {
        Category::Device => "Device",
        Category::Pv => "PV",
        Category::Battery => "Battery",
        Category::Grid => "Grid",
        Category::Load => "Load",
        Category::Inverter => "Inverter",
        Category::Generator => "Generator",
        Category::Ups => "UPS",
        Category::Tou => "TOU",
        Category::WorkMode => "Work Mode",
    }
}

fn render_deye_table(params: &[&Parameter]) -> String {
    let mut rows = String::new();
    for p in params {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            html_escape(p.name),
            html_escape(&p.get_text_value()),
            html_escape(p.unit.unwrap_or("")),
        ));
    }
    format!(
        "<table>\n<tr><th>Name</th><th>Value</th><th>Unit</th></tr>\n{}</table>",
        rows
    )
}

fn render_deye_page(params: Option<Vec<Parameter>>) -> String {
    let params = match params {
        None => {
            return render_shell(
                r#"<p class="error">Could not reach the deye task (it may not be configured, or busy) -- try again in a moment.</p>"#
                    .to_string(),
            )
        }
        Some(p) => p,
    };

    if params.is_empty() {
        return render_shell(
            "<p>No readings yet -- the deye task hasn't completed its first poll.</p>".to_string(),
        );
    }

    //same order the sections appear in deye.rs's param_table(), not
    //alphabetical -- keeps related registers grouped the way whoever wrote
    //that table intended
    let categories = [
        Category::Device,
        Category::Pv,
        Category::Battery,
        Category::Grid,
        Category::Load,
        Category::Inverter,
        Category::Generator,
        Category::Ups,
        Category::Tou,
        Category::WorkMode,
    ];

    let mut body = String::new();
    for cat in categories {
        //preserves param_table()'s original definition order within the
        //category, since filter() on a Vec is stable
        let rows: Vec<&Parameter> = params.iter().filter(|p| p.category == cat).collect();
        if rows.is_empty() {
            continue;
        }
        body.push_str(&format!("<h2>{}</h2>\n", category_label(cat)));
        body.push_str(&render_deye_table(&rows));
        body.push('\n');
    }

    render_shell(body)
}

fn render_status_page(devices: Option<Vec<DeviceStatus>>) -> String {
    let devices = match devices {
        None => {
            return render_shell(
                r#"<p class="error">Could not reach the onewire task (it may be disabled, or busy) -- try again in a moment.</p>"#
                    .to_string(),
            )
        }
        Some(d) => d,
    };

    let mut non_default: Vec<&DeviceStatus> = devices
        .iter()
        .filter(|d| d.is_on || d.override_mode)
        .collect();
    non_default.sort_by(|a, b| a.name.cmp(&b.name));

    let mut relays: Vec<&DeviceStatus> = devices
        .iter()
        .filter(|d| d.kind == DeviceStatusKind::Relay)
        .collect();
    relays.sort_by(|a, b| a.name.cmp(&b.name));

    let mut yeelights: Vec<&DeviceStatus> = devices
        .iter()
        .filter(|d| d.kind == DeviceStatusKind::Yeelight)
        .collect();
    yeelights.sort_by(|a, b| a.name.cmp(&b.name));

    let body = format!(
        "<h2>Devices in a non-default state</h2>\n{}\n<h2>All relays</h2>\n{}\n<h2>All yeelights</h2>\n{}",
        render_table(&non_default, "Nothing is currently in a non-default state."),
        render_table(&relays, "No relays configured."),
        render_table(&yeelights, "No yeelights configured."),
    );

    render_shell(body)
}

fn render_shell(body: String) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>hard - device status</title>
<style>
body {{ font-family: sans-serif; margin: 2em; color: #222; }}
h1 {{ margin-bottom: 0.2em; }}
h2 {{ margin-top: 1.6em; }}
table {{ border-collapse: collapse; width: 100%; }}
th, td {{ border: 1px solid #ccc; padding: 0.4em 0.8em; text-align: left; white-space: nowrap; }}
th {{ background: #eee; }}
.on {{ color: #1a7a1a; font-weight: bold; }}
.off {{ color: #888; }}
.error {{ color: #a00; }}
a {{ margin-right: 0.3em; }}
</style>
</head>
<body>
<h1>hard - device status</h1>
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
        //flume::Sender is already Clone + Send + Sync and sends via &self,
        //so this one doesn't need the Arc<Mutex<...>> treatment the tuple
        //above gets -- it's managed as its own independent Rocket State
        let deye_transmitter = self.deye_status_transmitter.clone();

        info!("{}: Starting task", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            //rocket::build() reads Rocket.toml/env/defaults into a Figment;
            //we merge one override on top (log_level: off) instead of
            //replacing the whole config, so nothing else you've configured
            //there (port, address, ...) is affected. Rocket's own request
            //logging is then entirely replaced by RequestLogger below,
            //which can skip quiet paths -- Rocket's built-in logging has no
            //way to do that per-route.
            let figment = rocket::Config::figment().merge(("log_level", "off"));
            let result = rocket::custom(figment)
                .mount(
                    mount_point(),
                    routes![
                        hello,
                        reload,
                        fan_on,
                        fan_off,
                        status,
                        device_action,
                        deye_status
                    ],
                )
                .manage(transmitters.clone())
                .manage(deye_transmitter.clone())
                .attach(RequestLogger)
                .launch()
                .await;
            result.expect("server failed unexpectedly");

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        info!("{}: task stopped", self.name);
        Ok(())
    }
}
