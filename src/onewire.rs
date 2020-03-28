use crate::database::{CommandCode, DbTask};
use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

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
    pub ow_address: u64,
    pub last_value: Option<u8>,
    pub file: Option<File>,
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
    pub ow_address: u64,
    pub last_value: Option<u8>,
    pub file: Option<File>,
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
                self.sensor_boards.push(SensorBoard {
                    pio_a: None,
                    pio_b: None,
                    ow_address: address,
                    last_value: None,
                    file: None,
                });
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

    pub fn add_relay(&mut self, id_relay: i32, name: String, address: u64, bit: u8) {
        //find or create a relay board
        let relay_board = match self
            .relay_boards
            .iter_mut()
            .find(|b| b.ow_address == address)
        {
            Some(b) => b,
            None => {
                self.relay_boards.push(RelayBoard {
                    relay: Default::default(),
                    ow_address: address,
                    last_value: None,
                    file: None,
                });
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

            thread::sleep(Duration::from_secs(10));
            //thread::sleep(Duration::from_micros(500));
        }
        info!("{}: Stopping thread", self.name);
    }
}
