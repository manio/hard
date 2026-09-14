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
use flume::{Receiver, Sender};
use futures::future::join_all;
use humantime::format_duration;
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::task;
use tokio::task::JoinSet;

mod database;
mod deye;
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env::set_var("RUST_BACKTRACE", "full");
    let started = Instant::now();
    logging_init();
    info!("💎 Welcome to hard (home automation rust-daemon)");

    //Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    //common thread stuff
    let influxdb_url = get_config_string("influxdb_url", None);
    let mut futures = JoinSet::new();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    //sensor_devices/relay_devices/relays are now owned directly by the onewire
    //coordinator task (no more Arc<RwLock<...>>): the database task only ever
    //*sends* a fresh snapshot over reload_rx/reload_tx below, it never shares
    //mutable access to these anymore.
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
    let rfid_pending_tags: Vec<u32> = vec![];
    let onewire_env_sensor_devices = Arc::new(RwLock::new(env_sensor_devices));
    let onewire_rfid_pending_tags = Arc::new(RwLock::new(rfid_pending_tags));
    let (tx, rx): (Sender<DbTask>, Receiver<DbTask>) = flume::unbounded(); //database thread comm channel
    let (deye_yield_tx, deye_yield_rx): (
        Sender<database::DeyeDailyYield>,
        Receiver<database::DeyeDailyYield>,
    ) = flume::unbounded(); //deye daily-yield comm channel
    let (ow_tx, ow_rx): (Sender<OneWireTask>, Receiver<OneWireTask>) = flume::unbounded(); //onewire thread comm channel
    let (lcd_tx, lcd_rx): (Sender<LcdTask>, Receiver<LcdTask>) = flume::unbounded(); //lcdproc comm channel
    let (reload_tx, reload_rx): (
        Sender<onewire::DeviceReloadData>,
        Receiver<onewire::DeviceReloadData>,
    ) = flume::unbounded(); //database -> onewire device-reload comm channel
    let (status_tx, status_rx): (Sender<onewire::StatusQuery>, Receiver<onewire::StatusQuery>) =
        flume::unbounded(); //webserver -> onewire status-query comm channel

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
            reload_transmitter: reload_tx,
            env_sensor_devices: onewire_env_sensor_devices.clone(),
            sensor_counters: Default::default(),
            relay_counters: Default::default(),
            yeelight_counters: Default::default(),
            influx_sensor_counters: Default::default(),
            influxdb_url: influxdb_url.clone(),
            influx_sensor_values: Default::default(),
            influx_relay_values: Default::default(),
            influx_cesspool_level: None,
            daily_yield_energy: None,
            deye_yield_receiver: deye_yield_rx,
            deye_daily_yield: None,
        };
        let worker_cancel_flag = cancel_flag.clone();
        let db_future = async move { db.worker(worker_cancel_flag).await };
        futures.spawn(db_future);
    }

    if !get_config_bool("disable_onewire", None) {
        //creating onewire coordinator task -- runs on the same tokio runtime
        //as everything else now (no dedicated std::thread anymore); the only
        //blocking work it does (sysfs sensor reads) is confined to
        //spawn_blocking inside its own poll loop, so it can't stall this
        //current_thread runtime or any other task on it.
        let onewire = onewire::OneWire {
            name: "onewire".to_string(),
            transmitter: tx.clone(),
            ow_receiver: ow_rx,
            lcd_transmitter: lcd_tx.clone(),
            reload_receiver: reload_rx,
            status_receiver: status_rx,
            sensor_devices,
            relay_devices,
            relays,
        };
        let worker_cancel_flag = cancel_flag.clone();
        let rfid_pending_tags_cloned = onewire_rfid_pending_tags.clone();
        let onewire_future = async move {
            onewire
                .worker(worker_cancel_flag, ethlcd, rfid_pending_tags_cloned)
                .await
        };
        futures.spawn(onewire_future);

        //creating onewire_env task
        let onewire_env = onewire_env::OneWireEnv {
            name: "onewire_env".to_string(),
            ow_transmitter: ow_tx.clone(),
            env_sensor_devices: onewire_env_sensor_devices.clone(),
        };
        let worker_cancel_flag = cancel_flag.clone();
        let onewire_env_future = async move { onewire_env.worker(worker_cancel_flag).await };
        futures.spawn(onewire_env_future);
    }

    if !get_config_bool("disable_webserver", None) {
        //creating webserver task
        let mut webserver = webserver::WebServer {
            name: "webserver".to_string(),
            ow_transmitter: ow_tx,
            db_transmitter: tx.clone(),
            status_transmitter: status_tx,
        };
        let worker_cancel_flag = cancel_flag.clone();
        let webserver_future = async move { webserver.worker(worker_cancel_flag).await };
        futures.spawn(webserver_future);
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
            let rfid_future = async move { rfid.worker(worker_cancel_flag).await };
            futures.spawn(rfid_future);
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
            let skymax_future = async move { skymax.worker(worker_cancel_flag).await };
            futures.spawn(skymax_future);
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
            let sun2000_future = async move { sun2000.worker(worker_cancel_flag).await };
            futures.spawn(sun2000_future);
        }
        _ => {}
    }

    //deye async task
    match get_config_string("host", Some("deye")) {
        Some(host) => {
            let worker_cancel_flag = cancel_flag.clone();
            let mut deye = deye::Deye::new(deye::DeyeConfig {
                name: "deye".to_string(),
                host_port: host,
                dongle_connection: get_config_bool("dongle_connection", Some("deye")),
                enable_write: get_config_bool("enable_write", Some("deye")),
                deye_yield_transmitter: Some(deye_yield_tx.clone()),
                influxdb_url: influxdb_url.clone(),
            });
            info!("config = {:?}", deye);
            let deye_future = async move { deye.worker(worker_cancel_flag).await };
            futures.spawn(deye_future);
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
            let lcdproc_future = async move { lcdproc.worker(worker_cancel_flag).await };
            futures.spawn(lcdproc_future);
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
            let remeha_future = async move { remeha.worker(worker_cancel_flag).await };
            futures.spawn(remeha_future);
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

    info!("🏁 Stopping all tasks...");
    //inform all tasks about termination
    cancel_flag.store(true, Ordering::SeqCst);
    //wait for tokio async tasks (onewire is now one of them too, no more
    //separate std::thread to join here)
    let mut cnt = 2;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), futures.join_next()).await {
            Ok(None) => break,       // JoinSet is empty - done, exit immediately
            Ok(Some(_)) => continue, // one task has finished, move on to the next one
            Err(_) => {
                // timeout - nothing finished within 10s
                cnt -= 1;
                if cnt == 0 {
                    error!("Unable to gracefully stop all tasks, forcing stop...");
                    break;
                }
                warn!("Still waiting for task(s) to stop...");
            }
        }
    }

    info!(
        "🚩 hard terminated, daemon running time: {}",
        format_duration(started.elapsed()).to_string()
    );
}
