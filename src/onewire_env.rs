use crate::onewire::{
    get_w1_device_name, OneWireTask, TaskCommand, FAMILY_CODE_DS18B20, FAMILY_CODE_DS18S20,
    FAMILY_CODE_DS2438, W1_ROOT_PATH,
};
use simplelog::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use std::{fs, thread};

pub const TEMP_CHECK_INTERVAL_SECS: f32 = 300.0; //secs between measuring temperature
pub const HUMID_CHECK_INTERVAL_SECS: f32 = 60.0; //secs between measuring humidity

pub struct EnvSensor {
    pub id_sensor: i32,
    pub id_kind: i32,
    pub name: String,
    pub tags: Vec<String>,
    pub associated_relays: Vec<i32>,
    pub associated_yeelights: Vec<i32>,
    pub ow_family: u8,
    pub ow_address: u64,
    pub file: Option<File>,
}

impl EnvSensor {
    fn is_temp_sensor(&self) -> bool {
        self.ow_family == FAMILY_CODE_DS18B20 || self.ow_family == FAMILY_CODE_DS18S20
    }

    fn is_humid_sensor(&self) -> bool {
        self.ow_family == FAMILY_CODE_DS2438
    }

    fn open(&mut self) {
        if self.is_temp_sensor() {
            let path = format!(
                "{}/{}/w1_slave",
                W1_ROOT_PATH,
                get_w1_device_name(self.ow_family, self.ow_address)
            );
            let data_path = Path::new(&path);
            info!(
                "{}: opening temperature sensor file: {}",
                get_w1_device_name(self.ow_family, self.ow_address),
                data_path.display()
            );
            self.file = File::open(data_path).ok();
        } else {
            info!(
                "{}: not a temperature sensor, skipping file open",
                get_w1_device_name(self.ow_family, self.ow_address),
            );
        }
    }

    fn read_temperature(&mut self) -> Option<f32> {
        if self.file.is_none() {
            self.open();
        }

        match &mut self.file {
            Some(file) => {
                match file.seek(SeekFrom::Start(0)) {
                    Err(e) => {
                        error!(
                            "{}: file seek error: {:?}",
                            get_w1_device_name(self.ow_family, self.ow_address),
                            e,
                        );
                    }
                    _ => {}
                }
                let mut data = String::new();
                match file.read_to_string(&mut data) {
                    Ok(_) => {
                        debug!(
                            "{}: temperature data: {}",
                            get_w1_device_name(self.ow_family, self.ow_address),
                            data,
                        );
                        for line in data.lines() {
                            if line.contains("crc") {
                                if line.contains("YES") {
                                    continue;
                                } else if line.contains("NO") {
                                    error!(
                                        "{}: got CRC error in temperature data",
                                        get_w1_device_name(self.ow_family, self.ow_address),
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
                    }
                    Err(e) => {
                        error!(
                            "{}: error reading: {:?}",
                            get_w1_device_name(self.ow_family, self.ow_address),
                            e,
                        );
                    }
                }
            }
            None => (),
        }

        return None;
    }

    fn read_humidity(&mut self) -> Option<(f32, f32)> {
        let mut temp_data: Option<f32> = None;
        let mut vdd_data: Option<f32> = None;
        let mut vad_data: Option<f32> = None;

        let temp_path = format!(
            "{}/{}/temperature",
            W1_ROOT_PATH,
            get_w1_device_name(self.ow_family, self.ow_address)
        );
        let vdd_path = format!(
            "{}/{}/vdd",
            W1_ROOT_PATH,
            get_w1_device_name(self.ow_family, self.ow_address)
        );
        let vad_path = format!(
            "{}/{}/vad",
            W1_ROOT_PATH,
            get_w1_device_name(self.ow_family, self.ow_address)
        );

        match fs::read_to_string(temp_path) {
            Ok(data) => {
                temp_data = data.trim().parse::<f32>().ok();
                debug!(
                    "{}: temperature data: {:?}, parsed: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    data.trim(),
                    temp_data,
                );
            }
            Err(e) => {
                error!(
                    "{}: error reading: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    e,
                );
            }
        }
        match fs::read_to_string(vdd_path) {
            Ok(data) => {
                vdd_data = data.trim().parse::<f32>().ok();
                debug!(
                    "{}: vdd data: {:?}, parsed: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    data.trim(),
                    vdd_data,
                );
            }
            Err(e) => {
                error!(
                    "{}: error reading: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    e,
                );
            }
        }
        match fs::read_to_string(vad_path) {
            Ok(data) => {
                vad_data = data.trim().parse::<f32>().ok();
                debug!(
                    "{}: vad data: {:?}, parsed: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    data.trim(),
                    vad_data,
                );
            }
            Err(e) => {
                error!(
                    "{}: error reading: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
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

        return None;
    }
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
        let mut env_sensor = EnvSensor {
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
            file: None,
        };
        env_sensor.open();
        self.env_sensors.push(env_sensor);
    }
}

pub struct OneWireEnv {
    pub name: String,
    pub ow_transmitter: Sender<OneWireTask>,
    pub env_sensor_devices: Arc<RwLock<EnvSensorDevices>>,
}

impl OneWireEnv {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
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
                {
                    let mut env_sensor_dev = self.env_sensor_devices.write().unwrap();

                    //fixme: do we really need to clone this HashMap to use it below?
                    let _kinds_cloned = env_sensor_dev.kinds.clone();

                    for sensor in &mut env_sensor_dev.env_sensors {
                        if sensor.is_temp_sensor() {
                            match sensor.read_temperature() {
                                Some(temp) => {
                                    info!(
                                        "{}: {}: 🌡️temperature: {} °C",
                                        get_w1_device_name(sensor.ow_family, sensor.ow_address),
                                        sensor.name,
                                        temp,
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            if last_humid_check.elapsed() > Duration::from_secs_f32(HUMID_CHECK_INTERVAL_SECS) {
                last_humid_check = Instant::now();

                debug!("measuring humidity...");
                {
                    let mut env_sensor_dev = self.env_sensor_devices.write().unwrap();

                    //fixme: do we really need to clone this HashMap to use it below?
                    let _kinds_cloned = env_sensor_dev.kinds.clone();

                    for sensor in &mut env_sensor_dev.env_sensors {
                        if sensor.is_humid_sensor() {
                            match sensor.read_humidity() {
                                Some(humid) => {
                                    info!(
                                        "{}: {}: 💧 humidity: {} %RH, 🌡️temperature: {} °C",
                                        get_w1_device_name(sensor.ow_family, sensor.ow_address),
                                        sensor.name,
                                        humid.0,
                                        humid.1,
                                    );
                                    for tag in &sensor.tags {
                                        if tag.starts_with("humid_threshold:") {
                                            let v: Vec<&str> = tag.split(":").collect();
                                            match v.get(1) {
                                                Some(&float_string) => {
                                                    match float_string.parse::<f32>() {
                                                        Ok(threshold) => {
                                                            if humid.0 > threshold {
                                                                warn!(
                                                                    "{}: {}: humidity: {} %RH is above {} %RH threshold, triggering associated relays...",
                                                                    get_w1_device_name(sensor.ow_family, sensor.ow_address),
                                                                    sensor.name,
                                                                    humid.0,
                                                                    threshold,
                                                                );
                                                                for id_relay in
                                                                    &sensor.associated_relays
                                                                {
                                                                    let task = OneWireTask {
                                                                        command: TaskCommand::TurnOnProlong,
                                                                        id_relay: Some(*id_relay),
                                                                        tag_group: None,
                                                                        id_yeelight: None,
                                                                        duration: None, //take default
                                                                    };
                                                                    let _ = self
                                                                        .ow_transmitter
                                                                        .send(task);
                                                                }
                                                            }
                                                        }
                                                        Err(_) => (),
                                                    }
                                                }
                                                _ => (),
                                            };
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(100));
        }
        info!("{}: thread stopped", self.name);
    }
}
