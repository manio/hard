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
use crate::onewire_env;
use crate::rfid::RfidTag;
use chrono::Utc;
use influxdb::InfluxDbWriteable;
use influxdb::{Client, Timestamp};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_compat_02::FutureExt;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Database {
    pub name: String,
    pub host: Option<String>,
    pub dbname: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub receiver: Receiver<DbTask>,
    pub conn: Option<postgres::Client>,
    pub disable_onewire: bool,
    pub sensor_devices: Arc<RwLock<onewire::SensorDevices>>,
    pub relay_devices: Arc<RwLock<onewire::RelayDevices>>,
    pub env_sensor_devices: Arc<RwLock<onewire_env::EnvSensorDevices>>,
    pub rfid_tags: Arc<RwLock<Vec<RfidTag>>>,
    pub sensor_counters: HashMap<i32, u32>,
    pub relay_counters: HashMap<i32, u32>,
    pub yeelight_counters: HashMap<i32, u32>,
    pub influx_sensor_counters: HashMap<i32, u32>,
    pub influxdb_url: Option<String>,
    pub influx_sensor_values: HashMap<i32, bool>,
    pub influx_relay_values: HashMap<i32, bool>,
    pub influx_cesspool_level: Option<u8>,
    pub daily_yield_energy: Option<i32>,
}

#[derive(Debug)]
pub enum CommandCode {
    ReloadDevices,
    IncrementSensorCounter,
    IncrementRelayCounter,
    IncrementYeelightCounter,
    UpdateSensorStateOn,
    UpdateSensorStateOff,
    UpdateRelayStateOn,
    UpdateRelayStateOff,
    UpdateCesspoolLevel,
    UpdateDailyEnergyYield,
}
pub struct DbTask {
    pub command: CommandCode,
    pub value: Option<i32>,
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
                let mut sensor_dev = self.sensor_devices.write().unwrap();
                let mut env_sensor_dev = self.env_sensor_devices.write().unwrap();
                let mut relay_dev = self.relay_devices.write().unwrap();
                let mut rfid_tag = self.rfid_tags.write().unwrap();

                info!("🦏 {}: Loading data from view 'kinds'...", self.name);
                sensor_dev.kinds.clear();
                env_sensor_dev.kinds.clear();
                for row in client.query("select * from kinds", &[]).unwrap() {
                    let id_kind: i32 = row.get("id_kind");
                    let name: String = row.get("name");
                    debug!("Got kind: {}: {}", id_kind, name);
                    sensor_dev.kinds.insert(id_kind, name.clone());
                    env_sensor_dev.kinds.insert(id_kind, name);
                }

                info!("🦏 {}: Loading data from view 'sensors'...", self.name);
                sensor_dev.sensor_boards.clear();
                for row in client.query("select * from sensors", &[]).unwrap() {
                    let id_sensor: i32 = row.get("id_sensor");
                    let id_kind: i32 = row.get("id_kind");
                    let name: String = row.get("name");
                    let family_code: Option<i16> = row.get("family_code");
                    let address: i32 = row.get("address");
                    let bit: i16 = row.get("bit");
                    let relay_agg: Vec<i32> = row.try_get("relay_agg").unwrap_or(vec![]);
                    let yeelight_agg: Vec<i32> = row.try_get("yeelight_agg").unwrap_or(vec![]);
                    let tags: Vec<String> = row.try_get("tags").unwrap_or(vec![]);
                    debug!(
                        "Got sensor: id_sensor={} kind={:?} name={:?} family_code={:?} address={} bit={} relay_agg={:?} yeelight_agg={:?} tags={:?}",
                        id_sensor,
                        sensor_dev.kinds.get(&id_kind).unwrap(),
                        name,
                        family_code,
                        address,
                        bit,
                        relay_agg,
                        yeelight_agg,
                        tags,
                    );
                    sensor_dev.add_sensor(
                        id_sensor,
                        id_kind,
                        name,
                        family_code,
                        address as u64,
                        bit as u8,
                        relay_agg,
                        yeelight_agg,
                        tags,
                    );
                }

                info!("🦏 {}: Loading data from view 'env_sensors'...", self.name);
                env_sensor_dev.env_sensors.clear();
                for row in client.query("select * from env_sensors", &[]).unwrap() {
                    let id_sensor: i32 = row.get("id_sensor");
                    let id_kind: i32 = row.get("id_kind");
                    let name: String = row.get("name");
                    let family_code: Option<i16> = row.get("family_code");
                    let address: i32 = row.get("address");
                    let relay_agg: Vec<i32> = row.try_get("relay_agg").unwrap_or(vec![]);
                    let yeelight_agg: Vec<i32> = row.try_get("yeelight_agg").unwrap_or(vec![]);
                    let tags: Vec<String> = row.try_get("tags").unwrap_or(vec![]);
                    debug!(
                        "Got env sensor: id_sensor={} kind={:?} name={:?} family_code={:?} address={} relay_agg={:?} yeelight_agg={:?} tags={:?}",
                        id_sensor,
                        env_sensor_dev.kinds.get(&id_kind).unwrap(),
                        name,
                        family_code,
                        address,
                        relay_agg,
                        yeelight_agg,
                        tags,
                    );
                    env_sensor_dev.add_sensor(
                        id_sensor,
                        id_kind,
                        name,
                        family_code,
                        address as u64,
                        relay_agg,
                        yeelight_agg,
                        tags,
                    );
                }

                info!("🦏 {}: Loading data from view 'relays'...", self.name);
                for row in client.query("select * from relays", &[]).unwrap() {
                    let id_relay: i32 = row.get("id_relay");
                    let name: String = row.get("name");
                    let family_code: Option<i16> = row.get("family_code");
                    let address: i32 = row.get("address");
                    let bit: i16 = row.get("bit");
                    let pir_exclude: bool = row.get("pir_exclude");
                    let pir_hold_secs = row.get("pir_hold_secs");
                    let switch_hold_secs = row.get("switch_hold_secs");
                    let initial_state: bool = row.get("initial_state");
                    let pir_all_day: bool = row.get("pir_all_day");
                    let tags: Vec<String> = row.try_get("tags").unwrap_or(vec![]);
                    debug!(
                        "Got relay: id_relay={} name={:?} family_code={:?} address={} bit={} pir_exclude={} pir_hold_secs={:?} switch_hold_secs={:?} initial_state={} pir_all_day={} tags={:?}",
                        id_relay, name, family_code, address, bit, pir_exclude, pir_hold_secs, switch_hold_secs, initial_state, pir_all_day, tags
                    );
                    relay_dev.add_relay(
                        id_relay,
                        name,
                        family_code,
                        address as u64,
                        bit as u8,
                        pir_exclude,
                        pir_hold_secs,
                        switch_hold_secs,
                        initial_state,
                        pir_all_day,
                        tags,
                    );
                }

                info!("🦏 {}: Loading data from view 'yeelights'...", self.name);
                relay_dev.yeelight.clear();
                for row in client.query("select * from yeelights", &[]).unwrap() {
                    let id_yeelight: i32 = row.get("id_yeelight");
                    let name: String = row.get("name");
                    let ip_address: String = row.get("ip_address");
                    let pir_exclude: bool = row.get("pir_exclude");
                    let pir_hold_secs = row.get("pir_hold_secs");
                    let switch_hold_secs = row.get("switch_hold_secs");
                    let pir_all_day: bool = row.get("pir_all_day");
                    let tags: Vec<String> = row.try_get("tags").unwrap_or(vec![]);
                    debug!(
                        "Got yeelight: id_yeelight={} name={:?} ip_address={} pir_exclude={} pir_hold_secs={:?} switch_hold_secs={:?} pir_all_day={} tags={:?}",
                        id_yeelight, name, ip_address, pir_exclude, pir_hold_secs, switch_hold_secs, pir_all_day, tags
                    );
                    relay_dev.add_yeelight(
                        id_yeelight,
                        name,
                        ip_address,
                        pir_exclude,
                        pir_hold_secs,
                        switch_hold_secs,
                        pir_all_day,
                        tags,
                    );
                }

                info!("🦏 {}: Loading data from view 'rfid_tags'...", self.name);
                rfid_tag.clear();
                for row in client.query("select * from rfid_tags", &[]).unwrap() {
                    let id_tag: i32 = row.get("id_tag");
                    let name: String = row.get("name");
                    let tags: Vec<String> = row.try_get("tags").unwrap_or(vec![]);
                    let relay_agg: Vec<i32> = row.try_get("relay_agg").unwrap_or(vec![]);
                    debug!(
                        "Got RFID tag: id_tag={} name={:?}, tags={:?}, relay_agg={:?}",
                        id_tag, name, tags, relay_agg
                    );
                    let new_tag = RfidTag {
                        id_tag,
                        name,
                        tags,
                        associated_relays: relay_agg,
                    };
                    rfid_tag.push(new_tag);
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

    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);
        let mut reload_devices = true;
        let mut flush_data = Instant::now();
        let mut influx_interval = Instant::now();

        let mut builder =
            SslConnector::builder(SslMethod::tls()).expect("SslConnector::builder error");
        builder.set_verify(SslVerifyMode::NONE); //allow self-signed certificates
        let connector = MakeTlsConnector::new(builder.build());

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                self.flush_counter_data();
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
                                let counter = self.sensor_counters.entry(id).or_insert(0 as u32);
                                *counter += 1;
                                if self.influxdb_url.is_some() {
                                    let counter =
                                        self.influx_sensor_counters.entry(id).or_insert(0 as u32);
                                    *counter += 1;
                                }
                            }
                            _ => {}
                        },
                        CommandCode::IncrementRelayCounter => match t.value {
                            Some(id) => {
                                let counter = self.relay_counters.entry(id).or_insert(0 as u32);
                                *counter += 1;
                            }
                            _ => {}
                        },
                        CommandCode::IncrementYeelightCounter => match t.value {
                            Some(id) => {
                                let counter = self.yeelight_counters.entry(id).or_insert(0 as u32);
                                *counter += 1;
                            }
                            _ => {}
                        },
                        CommandCode::UpdateSensorStateOn => match t.value {
                            Some(id) => {
                                if self.influxdb_url.is_some() {
                                    let value =
                                        self.influx_sensor_values.entry(id).or_insert(false);
                                    *value = true;
                                }
                            }
                            _ => {}
                        },
                        CommandCode::UpdateSensorStateOff => match t.value {
                            Some(id) => {
                                if self.influxdb_url.is_some() {
                                    let value =
                                        self.influx_sensor_values.entry(id).or_insert(false);
                                    *value = false;
                                }
                            }
                            _ => {}
                        },
                        CommandCode::UpdateRelayStateOn => match t.value {
                            Some(id) => {
                                if self.influxdb_url.is_some() {
                                    let value = self.influx_relay_values.entry(id).or_insert(false);
                                    *value = true;
                                }
                            }
                            _ => {}
                        },
                        CommandCode::UpdateRelayStateOff => match t.value {
                            Some(id) => {
                                if self.influxdb_url.is_some() {
                                    let value = self.influx_relay_values.entry(id).or_insert(false);
                                    *value = false;
                                }
                            }
                            _ => {}
                        },
                        CommandCode::UpdateCesspoolLevel => match t.value {
                            Some(level) => {
                                if self.influxdb_url.is_some() {
                                    self.influx_cesspool_level = Some(level as u8);
                                }
                            }
                            _ => {}
                        },
                        CommandCode::UpdateDailyEnergyYield => {
                            self.daily_yield_energy = t.value;
                        }
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
                    info!("🦏 {}: Connecting to: {}", self.name, connectionstring);
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
                if reload_devices && !self.disable_onewire {
                    info!("{}: loading devices from database...", self.name);
                    self.load_devices();
                    reload_devices = false;
                }
                if flush_data.elapsed().as_secs() > 10 {
                    //flush all data from hashmaps to database
                    debug!("flushing local data to db...");
                    self.flush_counter_data();

                    //flush daily energy yield from sun2000
                    if let Some(val) = self.daily_yield_energy {
                        if self.update_daily_energy_yield(val as f64 / 100.0) {
                            self.daily_yield_energy = None;
                        }
                    }

                    flush_data = Instant::now();
                }
            }

            //write data to influxdb if configured
            if self.influxdb_url.is_some()
                && !self.influx_sensor_counters.is_empty()
                && influx_interval.elapsed().as_secs() > 10
            {
                debug!("flushing sensor counters to influxdb...");
                let _ = self.influx_flush_counter_data().compat().await;
                influx_interval = Instant::now();
            }
            //write monitored sensor/relay values to influxdb
            if self.influxdb_url.is_some()
                && (!self.influx_sensor_values.is_empty() || !self.influx_relay_values.is_empty())
            {
                debug!("flushing sensor/relay values to influxdb...");
                let _ = self.influx_flush_values_data().compat().await;
            }
            //write cesspool level to postgres & influxdb
            if self.influxdb_url.is_some() && self.influx_cesspool_level.is_some() {
                debug!("flushing cesspool level to postgres...");
                self.pg_update_cesspool_level(self.influx_cesspool_level.unwrap() as i16);
                debug!("flushing cesspool level to influxdb...");
                let _ = self.influx_flush_cesspool_level().compat().await;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        info!("{}: task stopped", self.name);
        Ok(())
    }

    fn increment_cycles(&mut self, table_name: String, id_sensor: i32, counter: u32) -> bool {
        match self.conn.borrow_mut() {
            Some(client) => {
                let query = format!(
                    "update {} set cycles=cycles+$1 where id_{}=$2",
                    table_name, table_name
                );
                let result = client.execute(query.as_str(), &[&(counter as i64), &id_sensor]);
                match result {
                    Ok(_) => {
                        return true;
                    }
                    Err(e) => {
                        error!("{}: SQL error, query={:?}, error: {}", self.name, query, e);
                        self.conn = None;
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn update_daily_energy_yield(&mut self, value: f64) -> bool {
        match self.conn.borrow_mut() {
            Some(client) => {
                let query = "select * from daily_energy_yield_upsert($1)";
                let result = client.execute(query, &[&(value)]);
                match result {
                    Ok(_) => {
                        return true;
                    }
                    Err(e) => {
                        error!("{}: SQL error, query={:?}, error: {}", self.name, query, e);
                        self.conn = None;
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn pg_update_cesspool_level(&mut self, value: i16) -> bool {
        match self.conn.borrow_mut() {
            Some(client) => {
                let query = "insert into cesspool (val) values ($1)";
                let result = client.execute(query, &[&(value)]);
                match result {
                    Ok(_) => {
                        return true;
                    }
                    Err(e) => {
                        error!("{}: SQL error, query={:?}, error: {}", self.name, query, e);
                        self.conn = None;
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn flush_counter_data(&mut self) {
        let mut flush_map = self.sensor_counters.clone();
        flush_map
            .retain(|&id, &mut counter| !self.increment_cycles("sensor".to_string(), id, counter));
        self.sensor_counters = flush_map;

        flush_map = self.relay_counters.clone();
        flush_map
            .retain(|&id, &mut counter| !self.increment_cycles("relay".to_string(), id, counter));
        self.relay_counters = flush_map;

        flush_map = self.yeelight_counters.clone();
        flush_map.retain(|&id, &mut counter| {
            !self.increment_cycles("yeelight".to_string(), id, counter)
        });
        self.yeelight_counters = flush_map;
    }

    async fn influx_flush_counter_data(&mut self) -> Result<()> {
        // connect to influxdb
        let client = Client::new(self.influxdb_url.as_ref().unwrap(), "hard");

        // construct a write query with all sensors
        let mut write_query = Timestamp::from(Utc::now()).into_query("counter");
        for (id, counter) in self.influx_sensor_counters.iter() {
            write_query = write_query.add_field(format!("sensor-{}", id), counter);
        }

        // send query to influxdb
        let write_result = client.query(&write_query).await;
        match write_result {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", self.name, msg);
                self.influx_sensor_counters.clear();
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", self.name, e);
            }
        }

        Ok(())
    }

    async fn influx_flush_values_data(&mut self) -> Result<()> {
        // connect to influxdb
        let client = Client::new(self.influxdb_url.as_ref().unwrap(), "hard");

        // construct a write query
        let mut write_query = Timestamp::from(Utc::now()).into_query("state");
        // add sensors
        for (id, state) in self.influx_sensor_values.iter() {
            write_query = write_query.add_field(format!("sensor-{}", id), state);
        }
        // add relays
        for (id, state) in self.influx_relay_values.iter() {
            write_query = write_query.add_field(format!("relay-{}", id), state);
        }

        // send query to influxdb
        let write_result = client.query(&write_query).await;
        match write_result {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", self.name, msg);
                self.influx_sensor_values.clear();
                self.influx_relay_values.clear();
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", self.name, e);
            }
        }

        Ok(())
    }

    async fn influx_flush_cesspool_level(&mut self) -> Result<()> {
        // connect to influxdb
        let client = Client::new(self.influxdb_url.as_ref().unwrap(), "hard");

        // construct a write query with cesspool level
        let write_query = Timestamp::from(Utc::now()).into_query("state").add_field(
            format!("cesspool-level"),
            self.influx_cesspool_level.unwrap(),
        );

        // send query to influxdb
        let write_result = client.query(&write_query).await;
        match write_result {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", self.name, msg);
                self.influx_cesspool_level = None;
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", self.name, e);
            }
        }

        Ok(())
    }
}
