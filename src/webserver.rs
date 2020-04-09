use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rocket::*;

pub struct WebServer {
    pub name: String,
}

#[get("/hello")]
pub fn hello() -> &'static str {
    "Hello world!"
}

impl WebServer {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }
            rocket::ignite().mount("/hello", routes![hello]).launch();
            thread::sleep(Duration::from_millis(50));
        }
        info!("{}: Stopping thread", self.name);
    }
}
