#![feature(proc_macro_hygiene, decl_macro)]
//#[macro_use] - line disabled
use rocket::get;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::sync::Arc;

pub struct WebServer {
    pub name: String,
}

#[get("/<name>/<age>")]
fn hello(name: String, age: u8) -> String {
    format!("Hello, {} year old named {}!", age, name)
}

impl WebServer {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
        // rocket::ignite().mount("/", routes![index]).launch();
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        info!("{}: Stopping thread", self.name);
    }
}
