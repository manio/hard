use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::onewire::{OneWireTask, TaskCommand};
use rocket::*;
use std::sync::mpsc::Sender;

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
        id_relay: 14,
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
        id_relay: 14,
        duration: None,
    };
    let trans = ow_transmitter.lock().unwrap();
    trans.send(task).unwrap();

    "Turning OFF fan".to_string()
}

impl WebServer {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        //put a transmitter into a mutex and share to handlers
        let transmitter = Arc::new(Mutex::new(self.ow_transmitter.clone()));

        info!("{}: Starting thread", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }
            rocket::ignite()
                .mount("/cmd", routes![hello, fan_on, fan_off])
                .manage(transmitter.clone())
                .launch();
            thread::sleep(Duration::from_millis(50));
        }
        info!("{}: Stopping thread", self.name);
    }
}
