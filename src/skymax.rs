use futures::future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::prelude::*;
use tokio::time::timeout;

pub const SKYMAX_POLL_INTERVAL_SECS: f32 = 10.0; //secs between polling

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Skymax {
    pub name: String,
    pub device_path: String,
}

impl Skymax {
    pub async fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);

        let mut poll_interval = Instant::now();
        let mut buffer = [0; 2];
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("{}: Got terminate signal from main", self.name);
                break;
            }

            if poll_interval.elapsed() > Duration::from_secs_f32(SKYMAX_POLL_INTERVAL_SECS) {
                poll_interval = Instant::now();
                info!(
                    "{}: opening skymax device: {:?}",
                    self.name, self.device_path
                );
                let mut f = File::open(&self.device_path).await?;
                info!("{}: device opened", self.name);
                // if let Err(e) = f.write_all(&buf[0..n]).await {
                //     println!("write error: {:?}", e);
                //     continue;
                // }
                let retval = f.read(&mut buffer[..]);
                match timeout(Duration::from_millis(100), retval).await {
                    Ok(res) => match res {
                        Ok(n) => {
                            debug!("{}: read {} bytes", self.name, n);
                        }
                        Err(e) => {
                            error!("{}: file read error: {}", self.name, e);
                        }
                    },
                    Err(e) => {
                        error!("{}: response timeout: {}", self.name, e);
                    }
                }
            }

            thread::sleep(Duration::from_millis(30));
        }
        info!("{}: Stopping task", self.name);

        Ok(())
    }
}
