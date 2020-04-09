#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;
extern crate ctrlc;
extern crate simplelog;
use simplelog::*;

extern crate ini;
use self::ini::Ini;

use crate::database::DbTask;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

mod database;
mod onewire;
mod webserver;

fn log_location() -> Option<String> {
    let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
    conf.section(Some("general".to_owned()))
        .and_then(|x| x.get("log").cloned())
}

fn logging_init() {
    let mut conf = Config::default();
    conf.time_format = Some("%F, %H:%M:%S%.3f");

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = match TermLogger::new(LevelFilter::Info, conf) {
        Some(logger) => logger,
        None => {
            let mut conf_no_date = conf.clone();
            conf_no_date.time_format = Some("");
            SimpleLogger::new(LevelFilter::Info, conf_no_date)
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
    let sensor_devices = onewire::SensorDevices {
        kinds: HashMap::new(),
        sensor_boards: vec![],
    };
    let relay_devices = onewire::RelayDevices {
        relay_boards: vec![],
        yeelight: vec![],
    };
    let onewire_sensor_devices = Arc::new(RwLock::new(sensor_devices));
    let onewire_relay_devices = Arc::new(RwLock::new(relay_devices));
    let (tx, rx): (Sender<DbTask>, Receiver<DbTask>) = mpsc::channel(); //thread comm channel

    //creating db thread
    let mut db = database::Database {
        name: "postgres".to_string(),
        host: None,
        dbname: None,
        username: None,
        password: None,
        receiver: rx,
        conn: None,
        sensor_devices: onewire_sensor_devices.clone(),
        relay_devices: onewire_relay_devices.clone(),
        sensor_counters: Default::default(),
        relay_counters: Default::default(),
        yeelight_counters: Default::default(),
    };
    let worker_cancel_flag = cancel_flag.clone();
    let thread_builder = thread::Builder::new().name("db".into()); //thread name
    let thread_handler = thread_builder
        .spawn(move || {
            db.worker(worker_cancel_flag);
        })
        .unwrap();
    threads.push(thread_handler);

    //creating onewire thread
    let onewire = onewire::OneWire {
        name: "onewire".to_string(),
        transmitter: tx,
        sensor_devices: onewire_sensor_devices.clone(),
        relay_devices: onewire_relay_devices.clone(),
    };
    let worker_cancel_flag = cancel_flag.clone();
    let thread_builder = thread::Builder::new().name("onewire".into()); //thread name
    let thread_handler = thread_builder
        .spawn(move || {
            onewire.worker(worker_cancel_flag);
        })
        .unwrap();
    threads.push(thread_handler);

    //creating webserver thread
    let webserver = webserver::WebServer {
        name: "webserver".to_string(),
    };
    let worker_cancel_flag = cancel_flag.clone();
    let thread_builder = thread::Builder::new().name("webserver".into()); //thread name
    let thread_handler = thread_builder
        .spawn(move || {
            webserver.worker(worker_cancel_flag);
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
