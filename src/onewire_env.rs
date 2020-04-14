use crate::onewire::{get_w1_device_name, FAMILY_CODE_DS18B20, FAMILY_CODE_DS18S20, W1_ROOT_PATH};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

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
    fn open(&mut self) {
        if self.ow_family == FAMILY_CODE_DS18B20 || self.ow_family == FAMILY_CODE_DS18S20 {
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
                //todo
            }
            None => (),
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
    pub env_sensor_devices: Arc<RwLock<EnvSensorDevices>>,
}

impl OneWireEnv {
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            debug!("doing stuff");
            {
                let mut env_sensor_dev = self.env_sensor_devices.write().unwrap();

                //fixme: do we really need to clone this HashMap to use it below?
                let kinds_cloned = env_sensor_dev.kinds.clone();

                for sensor in &mut env_sensor_dev.env_sensors {
                    //todo
                }
            }
            thread::sleep(Duration::from_micros(500));
        }
        info!("{}: Stopping thread", self.name);
    }
}
