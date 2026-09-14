use crate::onewire::{
    get_w1_device_name, OneWireTask, TaskCommand, FAMILY_CODE_DS18B20, FAMILY_CODE_DS18S20,
    FAMILY_CODE_DS2438, W1_ROOT_PATH,
};
use flume::Sender;
use simplelog::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub const TEMP_CHECK_INTERVAL_SECS: f32 = 300.0; //secs between measuring temperature
pub const HUMID_CHECK_INTERVAL_SECS: f32 = 60.0; //secs between measuring humidity

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct EnvSensor {
    pub id_sensor: i32,
    pub id_kind: i32,
    pub name: String,
    pub tags: Vec<String>,
    pub associated_relays: Vec<i32>,
    pub associated_yeelights: Vec<i32>,
    pub ow_family: u8,
    pub ow_address: u64,
}

impl EnvSensor {
    fn is_temp_sensor(&self) -> bool {
        self.ow_family == FAMILY_CODE_DS18B20 || self.ow_family == FAMILY_CODE_DS18S20
    }

    fn is_humid_sensor(&self) -> bool {
        self.ow_family == FAMILY_CODE_DS2438
    }
}

//Free functions rather than &self/&mut self methods on EnvSensor, and no
//cached file handle anymore (the previous open()/self.file did one syscall
//less every 300s/60s check -- not worth it): both are now async tokio::fs
//I/O, and the caller (OneWireEnv::worker()) snapshots the (family, address)
//pairs it needs out of the env_sensor_devices lock *before* calling these,
//then releases the lock immediately. That's the point of taking plain
//values here instead of &EnvSensor -- holding any guard from that
//std::sync::RwLock across an .await would risk stalling database.rs's
//load_devices() (which also needs to write-lock the same
//Arc<RwLock<EnvSensorDevices>> on every reload) for as long as this I/O
//takes.
async fn read_temperature(ow_family: u8, ow_address: u64) -> Option<f32> {
    let path = format!(
        "{}/{}/w1_slave",
        W1_ROOT_PATH,
        get_w1_device_name(ow_family, ow_address)
    );

    let data = match tokio::fs::read_to_string(&path).await {
        Ok(data) => data,
        Err(e) => {
            error!(
                "{}: error reading: {:?}",
                get_w1_device_name(ow_family, ow_address),
                e,
            );
            return None;
        }
    };

    debug!(
        "{}: temperature data: {}",
        get_w1_device_name(ow_family, ow_address),
        data,
    );
    for line in data.lines() {
        if line.contains("crc") {
            if line.contains("YES") {
                continue;
            } else if line.contains("NO") {
                error!(
                    "{}: got CRC error in temperature data",
                    get_w1_device_name(ow_family, ow_address),
                );
                break;
            }
        } else if line.contains("t=") {
            let v: Vec<&str> = line.split("=").collect();
            let val = match v.get(1) {
                Some(&temp_value) => temp_value.parse::<f32>().ok(),
                _ => None,
            };
            return val.and_then(|x| Some(x / 1000.0));
        }
    }

    None
}

async fn read_humidity(ow_family: u8, ow_address: u64) -> Option<(f32, f32)> {
    let mut temp_data: Option<f32> = None;
    let mut vdd_data: Option<f32> = None;
    let mut vad_data: Option<f32> = None;

    let temp_path = format!(
        "{}/{}/temperature",
        W1_ROOT_PATH,
        get_w1_device_name(ow_family, ow_address)
    );
    let vdd_path = format!(
        "{}/{}/vdd",
        W1_ROOT_PATH,
        get_w1_device_name(ow_family, ow_address)
    );
    let vad_path = format!(
        "{}/{}/vad",
        W1_ROOT_PATH,
        get_w1_device_name(ow_family, ow_address)
    );

    match tokio::fs::read_to_string(&temp_path).await {
        Ok(data) => {
            temp_data = data.trim().parse::<f32>().ok();
            debug!(
                "{}: temperature data: {:?}, parsed: {:?}",
                get_w1_device_name(ow_family, ow_address),
                data.trim(),
                temp_data,
            );
        }
        Err(e) => {
            error!(
                "{}: error reading: {:?}",
                get_w1_device_name(ow_family, ow_address),
                e,
            );
        }
    }
    match tokio::fs::read_to_string(&vdd_path).await {
        Ok(data) => {
            vdd_data = data.trim().parse::<f32>().ok();
            debug!(
                "{}: vdd data: {:?}, parsed: {:?}",
                get_w1_device_name(ow_family, ow_address),
                data.trim(),
                vdd_data,
            );
        }
        Err(e) => {
            error!(
                "{}: error reading: {:?}",
                get_w1_device_name(ow_family, ow_address),
                e,
            );
        }
    }
    match tokio::fs::read_to_string(&vad_path).await {
        Ok(data) => {
            vad_data = data.trim().parse::<f32>().ok();
            debug!(
                "{}: vad data: {:?}, parsed: {:?}",
                get_w1_device_name(ow_family, ow_address),
                data.trim(),
                vad_data,
            );
        }
        Err(e) => {
            error!(
                "{}: error reading: {:?}",
                get_w1_device_name(ow_family, ow_address),
                e,
            );
        }
    }

    if temp_data.is_some() && vdd_data.is_some() && vad_data.is_some() {
        let temp = temp_data.unwrap() / 256.0;
        let vdd = vdd_data.unwrap() / 100.0;
        let vad = vad_data.unwrap() / 100.0;

        //magic computation here, see the HIH-4000-003 pdf for details
        let humid = (vad / vdd - 0.16) / 0.0062 / (1.0546 - 0.00216 * temp);

        return Some((humid, temp));
    }

    None
}

pub struct EnvSensorDevices {
    pub kinds: HashMap<i32, String>,
    pub env_sensors: Vec<EnvSensor>,
}

impl EnvSensorDevices {
    pub fn add_sensor(
        &mut self,
        id_sensor: i32,
        id_kind: i32,
        name: String,
        family_code: Option<i16>,
        address: u64,
        associated_relays: Vec<i32>,
        associated_yeelights: Vec<i32>,
        tags: Vec<String>,
    ) {
        //create a env sensor
        let env_sensor = EnvSensor {
            id_sensor,
            id_kind,
            name,
            tags,
            associated_relays,
            associated_yeelights,
            ow_family: match family_code {
                Some(family) => family as u8,
                None => FAMILY_CODE_DS18B20,
            },
            ow_address: address,
        };
        self.env_sensors.push(env_sensor);
    }
}

pub struct OneWireEnv {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
    pub env_sensor_devices: Arc<RwLock<EnvSensorDevices>>,
}

impl OneWireEnv {
    pub async fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);
        let mut last_temp_check = Instant::now();
        let mut last_humid_check = Instant::now();

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            if last_temp_check.elapsed() > Duration::from_secs_f32(TEMP_CHECK_INTERVAL_SECS) {
                last_temp_check = Instant::now();

                debug!("measuring temperatures...");
                //snapshot just the (name, family, address) of temp sensors
                //under a short-lived read lock, then release it *before*
                //doing any I/O below -- database.rs's load_devices() needs
                //to write-lock this same Arc<RwLock<EnvSensorDevices>> on
                //every reload, and holding any guard here across an .await
                //would risk stalling that reload for as long as this loop's
                //I/O takes
                let targets: Vec<(String, u8, u64)> = {
                    let env_sensor_dev = self.env_sensor_devices.read().unwrap();
                    env_sensor_dev
                        .env_sensors
                        .iter()
                        .filter(|s| s.is_temp_sensor())
                        .map(|s| (s.name.clone(), s.ow_family, s.ow_address))
                        .collect()
                };

                for (name, ow_family, ow_address) in targets {
                    if let Some(temp) = read_temperature(ow_family, ow_address).await {
                        info!(
                            "{}: {}: 🌡️ temperature: {} °C",
                            get_w1_device_name(ow_family, ow_address),
                            name,
                            temp,
                        );
                    }
                }
            }

            if last_humid_check.elapsed() > Duration::from_secs_f32(HUMID_CHECK_INTERVAL_SECS) {
                last_humid_check = Instant::now();

                debug!("measuring humidity...");
                //same snapshot-then-release pattern as the temperature
                //check above
                let targets: Vec<(String, u8, u64, Vec<String>, Vec<i32>)> = {
                    let env_sensor_dev = self.env_sensor_devices.read().unwrap();
                    env_sensor_dev
                        .env_sensors
                        .iter()
                        .filter(|s| s.is_humid_sensor())
                        .map(|s| {
                            (
                                s.name.clone(),
                                s.ow_family,
                                s.ow_address,
                                s.tags.clone(),
                                s.associated_relays.clone(),
                            )
                        })
                        .collect()
                };

                for (name, ow_family, ow_address, tags, associated_relays) in targets {
                    if let Some(humid) = read_humidity(ow_family, ow_address).await {
                        info!(
                            "{}: {}: 💧 humidity: {} %RH, 🌡️ temperature: {} °C",
                            get_w1_device_name(ow_family, ow_address),
                            name,
                            humid.0,
                            humid.1,
                        );
                        for tag in &tags {
                            if tag.starts_with("humid_threshold:") {
                                let v: Vec<&str> = tag.split(":").collect();
                                match v.get(1) {
                                    Some(&float_string) => match float_string.parse::<f32>() {
                                        Ok(threshold) => {
                                            if humid.0 > threshold {
                                                warn!(
                                                    "{}: {}: humidity: {} %RH is above {} %RH threshold, triggering associated relays...",
                                                    get_w1_device_name(ow_family, ow_address),
                                                    name,
                                                    humid.0,
                                                    threshold,
                                                );
                                                for id_relay in &associated_relays {
                                                    let task = OneWireTask {
                                                        command: TaskCommand::TurnOnProlong,
                                                        id_relay: Some(*id_relay),
                                                        tag_group: None,
                                                        id_yeelight: None,
                                                        duration: None, //take default
                                                    };
                                                    let _ = self.ow_transmitter.send(task);
                                                }
                                            }
                                        }
                                        Err(_) => (),
                                    },
                                    _ => (),
                                };
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        info!("{}: task stopped", self.name);
        Ok(())
    }
}
