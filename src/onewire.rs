use crate::database::{CommandCode, DbTask};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

//family codes for devices
pub const FAMILY_CODE_DS2413: u8 = 0x3a;
pub const FAMILY_CODE_DS2408: u8 = 0x29;

static W1_ROOT_PATH: &str = "/sys/bus/w1/devices";

fn get_w1_device_name(family_code: u8, address: u64) -> String {
    format!("{:02x}-{:012x}", family_code, address)
}

pub struct Sensor {
    pub id_sensor: i32,
    pub id_kind: i32,
    pub name: String,
    pub associated_relays: Vec<i32>,
    pub associated_yeelights: Vec<i32>,
}
pub struct SensorBoard {
    pub pio_a: Option<Sensor>,
    pub pio_b: Option<Sensor>,
    pub ow_family: u8,
    pub ow_address: u64,
    pub last_value: Option<u8>,
    pub file: Option<File>,
}

impl SensorBoard {
    fn open(&mut self) {
        let path = format!(
            "{}/{}/state",
            W1_ROOT_PATH,
            get_w1_device_name(self.ow_family, self.ow_address)
        );
        let data_path = Path::new(&path);
        info!(
            "{}: opening sensor file: {}",
            get_w1_device_name(self.ow_family, self.ow_address),
            data_path.display()
        );
        self.file = File::open(data_path).ok();
    }

    fn read_state(&mut self) -> Option<u8> {
        if self.file.is_none() {
            self.open();
        }

        match &mut self.file {
            Some(file) => {
                let mut new_value = [0u8; 1];
                file.seek(SeekFrom::Start(0)).expect("file seek error");
                file.read_exact(&mut new_value).expect("error reading");
                debug!(
                    "{}: read byte: {:#04x}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    new_value[0]
                );
                match self.last_value {
                    Some(val) => {
                        //we have last value to compare with
                        if new_value[0] != val {
                            debug!(
                                "{}: change detected, old: {:#04x} new: {:#04x}",
                                get_w1_device_name(self.ow_family, self.ow_address),
                                val,
                                new_value[0]
                            );
                            return Some(new_value[0]);
                        }
                    }
                    None => {
                        //sensor read for the very first time
                        self.last_value = Some(new_value[0]);
                    }
                }
            }
            None => (),
        }

        return None;
    }
}

pub struct Relay {
    pub id_relay: i32,
    pub name: String,
    pub last_toggled: Option<Instant>,
    pub stop_at: Option<Instant>,
    pub override_to: Option<Instant>,
}
pub struct RelayBoard {
    pub relay: [Option<Relay>; 8],
    pub ow_family: u8,
    pub ow_address: u64,
    pub new_value: Option<u8>,
    pub last_value: Option<u8>,
    pub file: Option<File>,
}

impl RelayBoard {
    fn open(&mut self) {
        let path = format!(
            "{}/{}/output",
            W1_ROOT_PATH,
            get_w1_device_name(self.ow_family, self.ow_address)
        );
        let data_path = Path::new(&path);
        info!(
            "{}: opening relay file: {}",
            get_w1_device_name(self.ow_family, self.ow_address),
            data_path.display()
        );
        self.file = File::open(data_path).ok();
    }

    fn save_state(&mut self) {
        if self.file.is_none() {
            self.open();
        }

        match &mut self.file {
            Some(file) => {
                //todo
            }
            None => (),
        }
    }
}

pub struct Yeelight {
    pub id_yeelight: i32,
    pub name: String,
    pub ip_address: String,
    pub last_toggled: Option<Instant>,
    pub stop_at: Option<Instant>,
    pub override_to: Option<Instant>,
}

pub struct Devices {
    pub kinds: HashMap<i32, String>,
    pub sensor_boards: Vec<SensorBoard>,
    pub relay_boards: Vec<RelayBoard>,
    pub yeelight: Vec<Yeelight>,
}

impl Devices {
    pub fn add_sensor(
        &mut self,
        id_sensor: i32,
        id_kind: i32,
        name: String,
        family_code: Option<i16>,
        address: u64,
        bit: u8,
        associated_relays: Vec<i32>,
        associated_yeelights: Vec<i32>,
    ) {
        //find or create a sensor board
        let sens_board = match self
            .sensor_boards
            .iter_mut()
            .find(|b| b.ow_address == address)
        {
            Some(b) => b,
            None => {
                let mut sens_board = SensorBoard {
                    pio_a: None,
                    pio_b: None,
                    ow_family: match family_code {
                        Some(family) => family as u8,
                        None => FAMILY_CODE_DS2413,
                    },
                    ow_address: address,
                    last_value: None,
                    file: None,
                };
                sens_board.open();
                self.sensor_boards.push(sens_board);
                self.sensor_boards.last_mut().unwrap()
            }
        };

        //create and attach a sensor
        let sensor = Sensor {
            id_sensor,
            id_kind,
            name,
            associated_relays,
            associated_yeelights,
        };
        match bit {
            0 => {
                sens_board.pio_a = Some(sensor);
            }
            2 => {
                sens_board.pio_b = Some(sensor);
            }
            _ => {}
        }
    }

    pub fn add_relay(
        &mut self,
        id_relay: i32,
        name: String,
        family_code: Option<i16>,
        address: u64,
        bit: u8,
    ) {
        //find or create a relay board
        let relay_board = match self
            .relay_boards
            .iter_mut()
            .find(|b| b.ow_address == address)
        {
            Some(b) => b,
            None => {
                let mut relay_board = RelayBoard {
                    relay: Default::default(),
                    ow_family: match family_code {
                        Some(family) => family as u8,
                        None => FAMILY_CODE_DS2408,
                    },
                    ow_address: address,
                    new_value: None,
                    last_value: None,
                    file: None,
                };

                //we probably can read the current state of relays but due to safety reasons
                //assume that all relays are turned off by default
                relay_board.last_value = Some(0xff);

                relay_board.open();
                self.relay_boards.push(relay_board);
                self.relay_boards.last_mut().unwrap()
            }
        };

        //create and attach a relay
        let relay = Relay {
            id_relay,
            name,
            last_toggled: None,
            stop_at: None,
            override_to: None,
        };
        relay_board.relay[bit as usize] = Some(relay);
    }

    pub fn add_yeelight(&mut self, id_yeelight: i32, name: String, ip_address: String) {
        //create and add a yeelight
        let light = Yeelight {
            id_yeelight,
            name,
            ip_address,
            last_toggled: None,
            stop_at: None,
            override_to: None,
        };
        self.yeelight.push(light);
    }
}

pub struct OneWire {
    pub name: String,
    pub transmitter: Sender<DbTask>,
    pub devices: Arc<RwLock<Devices>>,
}

impl OneWire {
    pub fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            debug!("doing stuff");
            {
                let mut dev = self.devices.write().unwrap();

                //fixme: do we really need to clone this HashMap to use it below?
                let kinds_cloned = dev.kinds.clone();

                for sb in &mut dev.sensor_boards {
                    match sb.read_state() {
                        //we have new state to process
                        Some(new_value) => {
                            match sb.last_value {
                                Some(last_value) => {
                                    //checking for change on bit 0 (PIOA)
                                    if sb.pio_a.is_some() && (new_value & 0x01 != last_value & 0x01)
                                    {
                                        info!(
                                            "{}: [{} PIOA {}]: {:#04x}",
                                            kinds_cloned
                                                .get(&sb.pio_a.as_ref().unwrap().id_kind)
                                                .unwrap(),
                                            get_w1_device_name(sb.ow_family, sb.ow_address),
                                            sb.pio_a.as_ref().unwrap().name,
                                            new_value
                                        );
                                        //todo: trigger change
                                    }
                                    //checking for change on bit 3 (PIOB)
                                    if sb.pio_b.is_some() && (new_value & 0x04 != last_value & 0x04)
                                    {
                                        info!(
                                            "{}: [{} PIOB {}]: {:#04x}",
                                            kinds_cloned
                                                .get(&sb.pio_b.as_ref().unwrap().id_kind)
                                                .unwrap(),
                                            get_w1_device_name(sb.ow_family, sb.ow_address),
                                            sb.pio_b.as_ref().unwrap().name,
                                            new_value
                                        );
                                        //todo: trigger change
                                    }
                                }
                                _ => {}
                            }
                            //processed -> save new value as the previous one:
                            sb.last_value = Some(new_value);
                        }
                        None => (),
                    }
                    thread::sleep(Duration::from_micros(500));
                }
            }
            let task = DbTask {
                command: CommandCode::ReloadDevices,
                value: None,
            };
            self.transmitter.send(task).unwrap();
            let task = DbTask {
                command: CommandCode::IncrementSensorCounter,
                value: Some(2),
            };
            self.transmitter.send(task).unwrap();
            let task = DbTask {
                command: CommandCode::IncrementRelayCounter,
                value: Some(1),
            };
            self.transmitter.send(task).unwrap();
        }
        info!("{}: Stopping thread", self.name);
    }
}
