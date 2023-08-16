use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_compat_02::FutureExt;

use crate::database::{CommandCode, DbTask};
use crate::onewire::{OneWireTask, TaskCommand};
use rocket::{get, routes, State};
use simplelog::*;
use std::sync::mpsc::Sender;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct WebServer {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
    pub db_transmitter: Sender<DbTask>,
}

#[get("/hello")]
pub fn hello() -> &'static str {
    "Hello world!"
}

#[get("/reload")]
pub fn reload(transmitters: &State<Arc<Mutex<(Sender<OneWireTask>, Sender<DbTask>)>>>) -> String {
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
pub fn fan_on(transmitters: &State<Arc<Mutex<(Sender<OneWireTask>, Sender<DbTask>)>>>) -> String {
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
pub fn fan_off(transmitters: &State<Arc<Mutex<(Sender<OneWireTask>, Sender<DbTask>)>>>) -> String {
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

impl WebServer {
    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        //put a transmitter into a mutex and share to handlers
        let transmitters = Arc::new(Mutex::new((
            self.ow_transmitter.clone(),
            self.db_transmitter.clone(),
        )));

        info!("{}: Starting task", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            let result = rocket::build()
                .mount("/cmd", routes![hello, reload, fan_on, fan_off])
                .manage(transmitters.clone())
                .launch()
                .compat()
                .await;
            result.expect("server failed unexpectedly");

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        info!("{}: task stopped", self.name);
        Ok(())
    }
}
