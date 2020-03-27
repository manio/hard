extern crate ini;
extern crate postgres;
extern crate postgres_openssl;

use self::ini::Ini;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock};

use crate::onewire;
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

pub struct Database {
    pub name: String,
    pub host: Option<String>,
    pub dbname: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub receiver: Receiver<DbTask>,
    pub conn: Option<postgres::Client>,
    pub devices: Arc<RwLock<onewire::Devices>>,
}

#[derive(Debug)]
pub enum CommandCode {
    ReloadDevices,
    IncrementSensorCounter,
    IncrementRelayCounter,
}
pub struct DbTask {
    pub command: CommandCode,
    pub value: Option<u32>,
}

impl Database {
    fn load_db_config(&mut self) {
        let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
        let section = conf
            .section(Some("postgres".to_owned()))
            .expect("Cannot find postgres section in config");
        self.host = section.get("host").cloned();
        self.dbname = section.get("dbname").cloned();
        self.username = section.get("username").cloned();
        self.password = section.get("password").cloned();
    }

    fn load_devices(&mut self) {
        match self.conn.borrow_mut() {
            Some(client) => {
                let mut dev = self.devices.write().unwrap();

                info!("{}: Loading data from table 'kind'...", self.name);
                dev.kinds.clear();
                for row in client.query("select * from kind", &[]).unwrap() {
                    let id_kind: i32 = row.get("id_kind");
                    let name: String = row.get("name");
                    debug!("Got kind: {}: {}", id_kind, name);
                    dev.kinds.insert(id_kind, name);
                }

                info!("{}: Loading data from table 'sensor'...", self.name);
                dev.sensor_boards.clear();
                for row in client.query("select * from sensor", &[]).unwrap() {
                    let id_sensor: i32 = row.get("id_sensor");
                    let id_kind: i32 = row.get("id_kind");
                    let name: String = row.get("name");
                    let address: i32 = row.get("address");
                    let bit: i16 = row.get("bit");
                    debug!(
                        "Got sensor: id_sensor={} kind={:?} name={:?} address={} bit={}",
                        id_sensor,
                        dev.kinds.get(&id_kind).unwrap(),
                        name,
                        address,
                        bit
                    );
                    dev.add_sensor(id_sensor, id_kind, name, address as u64, bit as u8);
                }
            }
            None => {
                error!(
                    "{}: no active database connection -> cannot load config",
                    self.name
                );
            }
        }
    }

    pub fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
        let mut reload_devices = true;
        let mut flush_data = Instant::now();
        let mut sensor_counters = HashMap::new();
        let mut relay_counters = HashMap::new();

        let mut builder =
            SslConnector::builder(SslMethod::tls()).expect("SslConnector::builder error");
        builder.set_verify(SslVerifyMode::NONE); //allow self-signed certificates
        let connector = MakeTlsConnector::new(builder.build());

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                //todo: check if we have all SQL data flushed
                debug!("Got terminate signal from main");
                break;
            }

            match self.receiver.try_recv() {
                Ok(t) => {
                    debug!(
                        "Received DbTask: command: {:?} value: {:?}",
                        t.command, t.value
                    );
                    match t.command {
                        CommandCode::ReloadDevices => {
                            info!("{}: Reload devices requested", self.name);
                            reload_devices = true;
                        }
                        CommandCode::IncrementSensorCounter => match t.value {
                            Some(id) => {
                                let counter = sensor_counters.entry(id).or_insert(0 as u32);
                                *counter += 1;
                            }
                            _ => {}
                        },
                        CommandCode::IncrementRelayCounter => match t.value {
                            Some(id) => {
                                let counter = relay_counters.entry(id).or_insert(0 as u32);
                                *counter += 1;
                            }
                            _ => {}
                        },
                    }
                }
                _ => (),
            }

            //(re)connect / load config when necessary
            if self.conn.is_none() {
                debug!("Loading db config...");
                self.load_db_config();

                if self.host.is_some()
                    && self.dbname.is_some()
                    && self.username.is_some()
                    && self.password.is_some()
                {
                    let connectionstring = format!(
                        "postgres://{}:{}@{}/{}?sslmode=require&application_name=hard",
                        self.username.as_ref().unwrap(),
                        self.password.as_ref().unwrap(),
                        self.host.as_ref().unwrap(),
                        self.dbname.as_ref().unwrap()
                    )
                    .to_string()
                    .clone();
                    info!("{}: Connecting to: {}", self.name, connectionstring);
                    let client = postgres::Client::connect(&connectionstring, connector.clone());
                    match client {
                        Ok(c) => {
                            self.conn = Some(c);
                            info!("{}: Connected successfully", self.name);
                        }
                        Err(e) => {
                            self.conn = None;
                            error!("{}: PostgreSQL connection error: {:?}", self.name, e);
                            info!("{}: Trying to reconnect...", self.name);
                        }
                    }
                } else {
                    error!(
                        "{}: postgres config is not OK, check the config file",
                        self.name
                    );
                }
            }

            //load devices / do idle SQL tasks
            if self.conn.is_some() {
                if reload_devices {
                    info!("{}: loading devices from database...", self.name);
                    self.load_devices();
                    reload_devices = false;
                }
                if flush_data.elapsed().as_secs() > 10 {
                    debug!("flushing local data to db...");
                    //todo
                    flush_data = Instant::now();
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
        info!("{}: Stopping thread", self.name);
    }
}
