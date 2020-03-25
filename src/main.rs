#[macro_use]
extern crate log;
extern crate ctrlc;
extern crate simplelog;
use simplelog::*;

extern crate ini;
use self::ini::Ini;

use crate::database::DbTask;
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

mod database;
mod onewire;

fn log_location() -> Option<String> {
    let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
    conf.section(Some("general".to_owned()))
        .and_then(|x| x.get("log").cloned())
}

fn logging_init() {
    let mut conf = Config::default();
    conf.time_format = Some("%F, %H:%M:%S%.3f");

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = match TermLogger::new(LevelFilter::Debug, conf) {
        Some(logger) => logger,
        None => {
            let mut conf_no_date = conf.clone();
            conf_no_date.time_format = Some("");
            SimpleLogger::new(LevelFilter::Debug, conf_no_date)
        }
    };
    loggers.push(console_logger);

    let mut logfile_error: Option<String> = None;
    match log_location() {
        Some(ref log_path) => {
            let logfile = OpenOptions::new().create(true).append(true).open(log_path);
            match logfile {
                Ok(logfile) => {
                    loggers.push(WriteLogger::new(LevelFilter::Info, conf, logfile));
                }
                Err(e) => {
                    logfile_error = Some(format!(
                        "Error creating/opening log file: {:?}: {:?}",
                        log_path, e
                    ));
                }
            }
        }
        _ => {}
    };

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
    if logfile_error.is_some() {
        error!("{}", logfile_error.unwrap());
        warn!("Will do console logging only...");
    }
}

fn main() {
    logging_init();
    info!("Welcome to hard (home automation rust-daemon)");

    //Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    //common thread stuff
    let mut threads = vec![];
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let devices = onewire::Devices {
        sensors: vec![],
        sensor_boards: vec![],
        relays: vec![],
        relay_boards: vec![],
    };
    let onewire_devices = Arc::new(RwLock::new(devices));
    let (tx, rx): (Sender<DbTask>, Receiver<DbTask>) = mpsc::channel(); //thread comm channel

    //creating db thread
    let mut db = database::Database {
        name: "postgres".to_string(),
        host: None,
        dbname: None,
        username: None,
        password: None,
        receiver: rx,
    };
    let worker_cancel_flag = cancel_flag.clone();
    let devices_cloned = onewire_devices.clone();
    let thread_builder = thread::Builder::new().name("db".into()); //thread name
    let thread_handler = thread_builder
        .spawn(move || {
            db.worker(worker_cancel_flag, devices_cloned);
        })
        .unwrap();
    threads.push(thread_handler);

    //creating onewire thread
    let mut onewire = onewire::OneWire {
        name: "onewire".to_string(),
        transmitter: tx,
    };
    let worker_cancel_flag = cancel_flag.clone();
    let devices_cloned = onewire_devices.clone();
    let thread_builder = thread::Builder::new().name("onewire".into()); //thread name
    let thread_handler = thread_builder
        .spawn(move || {
            onewire.worker(worker_cancel_flag, devices_cloned);
        })
        .unwrap();
    threads.push(thread_handler);

    debug!("Entering main loop...");
    loop {
        if !running.load(Ordering::SeqCst) {
            info!("Ctrl-C or SIGTERM signal detected, exiting...");
            break;
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("Stopping all threads...");
    //inform all threads about termination
    cancel_flag.store(true, Ordering::SeqCst);
    //wait for termination
    for t in threads {
        // Wait for the thread to finish. Returns a result.
        let _ = t.join();
    }
    info!("Done, exiting");
}
