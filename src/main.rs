#![feature(proc_macro_hygiene, decl_macro)]

extern crate ctrlc;
extern crate simplelog;
use simplelog::*;

extern crate ini;
use self::ini::Ini;

use crate::database::DbTask;
use crate::ethlcd::EthLcd;
use crate::lcdproc::LcdTask;
use crate::onewire::OneWireTask;
use crate::rfid::RfidTag;
use futures::future::join_all;
use humantime::format_duration;
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::task;
use tokio_compat_02::FutureExt;

mod database;
mod ethlcd;
mod lcdproc;
mod onewire;
mod onewire_env;
mod remeha;
mod rfid;
mod skymax;
mod sun2000;
mod webserver;

fn get_config_string(option_name: &str, section: Option<&str>) -> Option<String> {
    let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
    conf.section(Some(section.unwrap_or("general").to_owned()))
        .and_then(|x| x.get(option_name).cloned())
}

fn get_config_bool(option_name: &str, section: Option<&str>) -> bool {
    let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
    let value = conf
        .section(Some(section.unwrap_or("general").to_owned()))
        .and_then(|x| x.get(option_name).cloned());
    match value {
        Some(val) => match val.trim() {
            "yes" => true,
            "true" => true,
            "1" => true,
            _ => false,
        },
        _ => false,
    }
}

fn logging_init() {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        LevelFilter::Info,
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    let mut logfile_error: Option<String> = None;
    match get_config_string("log", None) {
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

#[tokio::main]
async fn main() {
    env::set_var("RUST_BACKTRACE", "full");
    let started = Instant::now();
    logging_init();
    info!("🛡️ Welcome to hard (home automation rust-daemon)");

    //Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    //common thread stuff
    let influxdb_url = get_config_string("influxdb_url", None);
    let mut threads = vec![];
    let mut futures = vec![];
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let sensor_devices = onewire::SensorDevices {
        kinds: HashMap::new(),
        sensor_boards: vec![],
        max_cesspool_level: 0,
    };
    let relay_devices = onewire::RelayDevices {
        relay_boards: vec![],
        yeelight: vec![],
    };
    let relays = onewire::Relays { relay: vec![] };
    let env_sensor_devices = onewire_env::EnvSensorDevices {
        kinds: HashMap::new(),
        env_sensors: vec![],
    };
    let rfid_tags: Vec<RfidTag> = vec![];
    let rfid_pending_tags: Vec<u32> = vec![];
    let onewire_sensor_devices = Arc::new(RwLock::new(sensor_devices));
    let onewire_relay_devices = Arc::new(RwLock::new(relay_devices));
    let onewire_relays = Arc::new(RwLock::new(relays));
    let onewire_env_sensor_devices = Arc::new(RwLock::new(env_sensor_devices));
    let onewire_rfid_tags = Arc::new(RwLock::new(rfid_tags));
    let onewire_rfid_pending_tags = Arc::new(RwLock::new(rfid_pending_tags));
    let (tx, rx): (Sender<DbTask>, Receiver<DbTask>) = mpsc::channel(); //database thread comm channel
    let (ow_tx, ow_rx): (Sender<OneWireTask>, Receiver<OneWireTask>) = mpsc::channel(); //onewire thread comm channel
    let (lcd_tx, lcd_rx): (Sender<LcdTask>, Receiver<LcdTask>) = mpsc::channel(); //lcdproc comm channel

    //ethlcd struct
    let ethlcd = match get_config_string("ethlcd_host", None) {
        Some(hostname) => Some(EthLcd {
            struct_name: "ethlcd".to_string(),
            host: hostname,
            in_progress: Arc::new(AtomicBool::new(false)),
        }),
        _ => None,
    };

    if !get_config_bool("disable_postgres", None) {
        //creating db task
        let mut db = database::Database {
            name: "postgres".to_string(),
            host: None,
            dbname: None,
            username: None,
            password: None,
            receiver: rx,
            conn: None,
            disable_onewire: get_config_bool("disable_onewire", None),
            sensor_devices: onewire_sensor_devices.clone(),
            relay_devices: onewire_relay_devices.clone(),
            relays: onewire_relays.clone(),
            env_sensor_devices: onewire_env_sensor_devices.clone(),
            rfid_tags: onewire_rfid_tags.clone(),
            sensor_counters: Default::default(),
            relay_counters: Default::default(),
            yeelight_counters: Default::default(),
            influx_sensor_counters: Default::default(),
            influxdb_url: influxdb_url.clone(),
            influx_sensor_values: Default::default(),
            influx_relay_values: Default::default(),
            influx_cesspool_level: None,
            daily_yield_energy: None,
        };
        let worker_cancel_flag = cancel_flag.clone();
        let db_future = task::spawn(async move { db.worker(worker_cancel_flag).await });
        futures.push(db_future);
    }

    if !get_config_bool("disable_onewire", None) {
        //creating onewire thread
        let onewire = onewire::OneWire {
            name: "onewire".to_string(),
            transmitter: tx.clone(),
            ow_receiver: ow_rx,
            lcd_transmitter: lcd_tx.clone(),
            sensor_devices: onewire_sensor_devices.clone(),
            relay_devices: onewire_relay_devices.clone(),
            relays: onewire_relays.clone(),
        };
        let worker_cancel_flag = cancel_flag.clone();
        let thread_builder = thread::Builder::new().name("onewire".into()); //thread name
        let rfid_pending_tags_cloned = onewire_rfid_pending_tags.clone();
        let thread_handler = thread_builder
            .spawn(move || {
                onewire.worker(
                    worker_cancel_flag,
                    ethlcd,
                    onewire_rfid_tags.clone(),
                    rfid_pending_tags_cloned,
                );
            })
            .unwrap();
        threads.push(thread_handler);

        //creating onewire_env thread
        let onewire_env = onewire_env::OneWireEnv {
            name: "onewire_env".to_string(),
            ow_transmitter: ow_tx.clone(),
            env_sensor_devices: onewire_env_sensor_devices.clone(),
        };
        let worker_cancel_flag = cancel_flag.clone();
        let thread_builder = thread::Builder::new().name("onewire_env".into()); //thread name
        let thread_handler = thread_builder
            .spawn(move || {
                onewire_env.worker(worker_cancel_flag);
            })
            .unwrap();
        threads.push(thread_handler);
    }

    if !get_config_bool("disable_webserver", None) {
        //creating webserver task
        let mut webserver = webserver::WebServer {
            name: "webserver".to_string(),
            ow_transmitter: ow_tx,
            db_transmitter: tx.clone(),
        };
        let worker_cancel_flag = cancel_flag.clone();
        let webserver_future =
            task::spawn(async move { webserver.worker(worker_cancel_flag).await });
        futures.push(webserver_future);
    }

    //rfid task
    match get_config_string("rfid_event_path", None) {
        Some(event_path) => {
            let rfid = rfid::Rfid {
                name: "rfid".to_string(),
                event_path,
                rfid_pending_tags: onewire_rfid_pending_tags.clone(),
            };
            let worker_cancel_flag = cancel_flag.clone();
            let rfid_future = task::spawn(async move { rfid.worker(worker_cancel_flag).await });
            futures.push(rfid_future);
        }
        _ => {}
    };

    //skymax async task
    match get_config_string("skymax_device", None) {
        Some(path) => {
            let worker_cancel_flag = cancel_flag.clone();
            let mut skymax = skymax::Skymax {
                name: "skymax".to_string(),
                device_path: path,
                device_usbid: get_config_string("skymax_usbid", None).unwrap_or_default(),
                poll_ok: 0,
                poll_errors: 0,
                influxdb_url: influxdb_url.clone(),
                lcd_transmitter: lcd_tx.clone(),
                mode_change_script: get_config_string("skymax_mode_change_script", None),
            };
            let skymax_future = task::spawn(async move { skymax.worker(worker_cancel_flag).await });
            futures.push(skymax_future);
        }
        _ => {}
    }

    //sun2000 async task
    match get_config_string("host", Some("sun2000")) {
        Some(host) => {
            let worker_cancel_flag = cancel_flag.clone();
            let mut sun2000 = sun2000::Sun2000 {
                name: "sun2000".to_string(),
                host_port: host,
                poll_ok: 0,
                poll_errors: 0,
                influxdb_url: influxdb_url.clone(),
                lcd_transmitter: lcd_tx.clone(),
                db_transmitter: tx.clone(),
                mode_change_script: get_config_string("mode_change_script", Some("sun2000")),
                optimizers: get_config_bool("optimizers", Some("sun2000")),
                battery_installed: get_config_bool("battery_installed", Some("sun2000")),
                dongle_connection: get_config_bool("dongle_connection", Some("sun2000")),
            };
            let sun2000_future =
                task::spawn(async move { sun2000.worker(worker_cancel_flag).compat().await });
            futures.push(sun2000_future);
        }
        _ => {}
    }

    //lcdproc async task
    match get_config_string("lcdproc", None) {
        Some(host) => {
            let worker_cancel_flag = cancel_flag.clone();
            let mut lcdproc = lcdproc::Lcdproc {
                name: "lcdproc".to_string(),
                lcdproc_host_port: host,
                lcd_receiver: lcd_rx,
                lcd_lines: vec![],
                level: None,
            };
            let lcdproc_future =
                task::spawn(async move { lcdproc.worker(worker_cancel_flag).await });
            futures.push(lcdproc_future);
        }
        _ => {}
    }

    //remeha async task
    match get_config_string("remeha_device", None) {
        Some(host) => {
            let worker_cancel_flag = cancel_flag.clone();
            let mut remeha = remeha::Remeha {
                display_name: "<i><bright-black>remeha:</>".to_string(),
                device_host_port: host,
                poll_ok: 0,
                poll_errors: 0,
                influxdb_url: influxdb_url.clone(),
                state_change_script: get_config_string("remeha_state_change_script", None),
            };
            let remeha_future = task::spawn(async move { remeha.worker(worker_cancel_flag).await });
            futures.push(remeha_future);
        }
        _ => {}
    }

    debug!("Entering main loop...");
    loop {
        if !running.load(Ordering::SeqCst) {
            info!("🛑 Ctrl-C or SIGTERM signal detected, exiting...");
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    info!("🏁 Stopping all threads...");
    //inform all threads about termination
    cancel_flag.store(true, Ordering::SeqCst);
    //wait for termination
    for t in threads {
        // Wait for the thread to finish. Returns a result.
        let _ = t.join();
    }
    //wait for tokio async tasks
    let _ = join_all(futures).await;

    info!(
        "🚩 hard terminated, daemon running time: {}",
        format_duration(started.elapsed()).to_string()
    );
}
