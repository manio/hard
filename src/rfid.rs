use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct Rfid {
    pub name: String,
}

impl Rfid {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            //todo

            thread::sleep(Duration::from_micros(500));
        }
        info!("{}: Stopping thread", self.name);
    }
}
