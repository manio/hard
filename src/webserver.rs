use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio_compat_02::FutureExt;

use crate::onewire::{OneWireTask, TaskCommand};
use rocket::*;
use std::sync::mpsc::Sender;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct WebServer {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
}

#[get("/hello")]
pub fn hello() -> &'static str {
    "Hello world!"
}

#[get("/fan-on")]
pub fn fan_on(ow_transmitter: State<Arc<Mutex<Sender<OneWireTask>>>>) -> String {
    let task = OneWireTask {
        command: TaskCommand::TurnOnProlong,
        id_relay: Some(14),
        tag_group: None,
        duration: Some(Duration::from_secs(60 * 5)),
    };
    let trans = ow_transmitter.lock().unwrap();
    trans.send(task).unwrap();

    "Turning ON fan".to_string()
}

#[get("/fan-off")]
pub fn fan_off(ow_transmitter: State<Arc<Mutex<Sender<OneWireTask>>>>) -> String {
    let task = OneWireTask {
        command: TaskCommand::TurnOff,
        id_relay: Some(14),
        tag_group: None,
        duration: None,
    };
    let trans = ow_transmitter.lock().unwrap();
    trans.send(task).unwrap();

    "Turning OFF fan".to_string()
}

impl WebServer {
    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        //put a transmitter into a mutex and share to handlers
        let transmitter = Arc::new(Mutex::new(self.ow_transmitter.clone()));

        info!("{}: Starting task", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }
            let result = rocket::ignite()
                .mount("/cmd", routes![hello, fan_on, fan_off])
                .manage(transmitter.clone())
                .launch()
                .compat()
                .await;
            result.expect("server failed unexpectedly");

            thread::sleep(Duration::from_millis(50));
        }

        info!("{}: Stopping task", self.name);
        Ok(())
    }
}
