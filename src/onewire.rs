use crate::database::{CommandCode, DbTask};
use crate::ethlcd::{BeepMethod, EthLcd};
use crate::lcdproc::{LcdTask, LcdTaskCommand};
use crate::rfid::RfidTag;
use humantime::format_duration;
use ini::Ini;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use simplelog::*;
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::BufReader;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::ops::Add;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

//family codes for devices
pub const FAMILY_CODE_DS2413: u8 = 0x3a;
pub const FAMILY_CODE_DS2408: u8 = 0x29;
pub const FAMILY_CODE_DS18S20: u8 = 0x10;
pub const FAMILY_CODE_DS18B20: u8 = 0x28;
pub const FAMILY_CODE_DS2438: u8 = 0x26;

pub const DS2408_INITIAL_STATE: u8 = 0xff;

//timing constants
pub const DEFAULT_PIR_HOLD_SECS: f32 = 120.0; //2min for PIR sensors
pub const DEFAULT_SWITCH_HOLD_SECS: f32 = 3600.0; //1hour for wall-switches
pub const DEFAULT_PIR_PROLONG_SECS: f32 = 900.0; //15min prolonging in override_mode
pub const MIN_TOGGLE_DELAY_SECS: f32 = 1.0; //1sec flip-flop protection: minimum delay between toggles
pub const ENTRY_LIGHT_PROLONG_SECS: f32 = 600.0; //10min prolonging for entry lights

pub static W1_ROOT_PATH: &str = "/sys/bus/w1/devices";

//yeelight consts
pub const YEELIGHT_TCP_PORT: u16 = 55443;
static YEELIGHT_METHOD_SET_POWER: &str = "set_power"; //method value name for powering on/off
static YEELIGHT_EFFECT: &str = "smooth"; //default effect for turning on/off
pub const YEELIGHT_DURATION_MS: u32 = 500; //duration of above effect

pub const DAYLIGHT_SUN_DEGREE: f64 = 3.0; //sun elevation for day/night switching
pub const SUN_POS_CHECK_INTERVAL_SECS: f32 = 60.0; //secs between calculating sun position

#[derive(Debug, PartialEq)]
pub enum ProlongKind {
    PIR,
    Remote,
    Switch,
    AutoOff,
    DayNight,
}
#[derive(Clone, Debug)]
pub enum TaskCommand {
    TurnOnProlong,
    TurnOnProlongNight,
    TurnOff,
}
#[derive(Clone)]
pub struct OneWireTask {
    pub command: TaskCommand,
    pub id_relay: Option<i32>,
    pub tag_group: Option<String>,
    pub id_yeelight: Option<i32>,
    pub duration: Option<Duration>,
}

pub fn get_w1_device_name(family_code: u8, address: u64) -> String {
    format!("{:02x}-{:012x}", family_code, address)
}

pub struct Sensor {
    pub id_sensor: i32,
    pub id_kind: i32,
    pub name: String,
    pub tags: Vec<String>,
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
                let result = file.read_exact(&mut new_value);
                match result {
                    Ok(_) => {
                        debug!(
                            "{}: read byte: {:#04x}",
                            get_w1_device_name(self.ow_family, self.ow_address),
                            new_value[0]
                        );
                        //in this application only the following values are valid
                        if new_value[0] == 0x5a
                            || new_value[0] == 0x4b
                            || new_value[0] == 0x1e
                            || new_value[0] == 0x0f
                        {
                            return Some(new_value[0]);
                        } else {
                            error!(
                                "{}: reading state file gives invalid byte value: {:#04x}, ignoring",
                                get_w1_device_name(self.ow_family, self.ow_address),
                                new_value[0]
                            );
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
}

pub struct Device {
    pub id: i32,
    pub name: String,
    pub tags: Vec<String>,
    pub pir_exclude: bool,
    pub pir_hold_secs: f32,
    pub switch_hold_secs: f32,
    pub pir_all_day: bool,
    pub override_mode: bool,
    pub last_toggled: Option<Instant>,
    pub stop_after: Option<Duration>,
}

impl Device {
    fn turn_on_prolong(
        &mut self,
        kind: ProlongKind,
        night: bool,
        dest_name: String,
        on: bool,
        currently_off: bool,
        duration: Option<Duration>,
    ) -> bool {
        if (kind == ProlongKind::PIR
            && !(self.override_mode && on
                || (!self.pir_exclude && on && (night || self.pir_all_day))))
            || ((kind == ProlongKind::Remote || kind == ProlongKind::AutoOff)
                && !on
                && currently_off)
        {
            return false;
        }
        let d = match duration {
            Some(d) => {
                //if we have a duration pass it directly
                d
            }
            None => {
                //otherwise take a switch_hold_secs or pir_hold_secs
                let mut prolong_secs = match kind {
                    ProlongKind::Switch => self.switch_hold_secs,
                    _ => self.pir_hold_secs,
                };
                if kind != ProlongKind::Switch {
                    if !self.override_mode && currently_off {
                        if kind == ProlongKind::Remote
                            && self.switch_hold_secs != DEFAULT_SWITCH_HOLD_SECS
                        {
                            prolong_secs = self.switch_hold_secs
                        }
                    } else if self.override_mode {
                        if DEFAULT_PIR_PROLONG_SECS > prolong_secs {
                            prolong_secs = DEFAULT_PIR_PROLONG_SECS;
                        };
                    }
                }
                Duration::from_secs_f32(prolong_secs)
            }
        };

        //visual
        let mode = match kind {
            ProlongKind::Switch => "🔲 Switch toggle".to_string(),
            ProlongKind::Remote => format!("🧩 Remote turn-{}", {
                if on {
                    "on"
                } else {
                    "off"
                }
            }),
            ProlongKind::PIR => "💡 PIR turn-on".to_string(),
            ProlongKind::AutoOff => "⌛ Auto turn-off".to_string(),
            ProlongKind::DayNight => format!("🌄 Day/night auto turn-{}", {
                if on {
                    "on"
                } else {
                    "off"
                }
            }),
        };

        //checking if device is currently OFF
        if kind == ProlongKind::Switch
            || ((kind == ProlongKind::Remote || kind == ProlongKind::AutoOff) && !on)
            || (!self.override_mode && currently_off)
            || kind == ProlongKind::DayNight
        {
            //flip-flop protection for too fast state changes
            let mut flipflop_block = false;
            match self.last_toggled {
                Some(toggled) => {
                    if toggled.elapsed() < Duration::from_secs_f32(MIN_TOGGLE_DELAY_SECS) {
                        flipflop_block = true;
                    }
                }
                _ => {}
            }

            if flipflop_block {
                warn!(
                        "<d>- - -</> 🚫 flip-flop protection: <b>{}</> <cyan>(</><magenta>{}</><cyan>)</>, {} request ignored",
                        self.name,
                        dest_name,
                        mode,
                    );
            } else {
                let duration;
                if (kind == ProlongKind::Remote && !on)
                    || kind == ProlongKind::AutoOff
                    || kind == ProlongKind::DayNight
                {
                    duration = "".into();
                    self.stop_after = None;
                    if kind == ProlongKind::AutoOff && currently_off && self.override_mode {
                        info!(
                        "<d>- - -</> 🔓 End of override mode: <b>{}</> <cyan>(</><magenta>{}</><cyan>)</>{}",
                        self.name, dest_name, duration,
                    );
                        self.last_toggled = None;
                        self.override_mode = false;
                        return false;
                    }
                    self.override_mode = false;
                } else {
                    duration = format!(", duration: <yellow>{}</>", format_duration(d));
                    if kind == ProlongKind::Switch {
                        self.override_mode = true;
                    }
                    self.stop_after = Some(d);
                }
                info!(
                    "<d>- - -</> {}: <b>{}</> <cyan>(</><magenta>{}</><cyan>)</>{}",
                    mode, self.name, dest_name, duration,
                );
                self.last_toggled = Some(Instant::now());
                return true;
            }
        } else {
            let toggled_elapsed = self.last_toggled.unwrap_or(Instant::now()).elapsed();
            if self.override_mode {
                if self.switch_hold_secs > d.as_secs_f32()
                    && toggled_elapsed
                        > Duration::from_secs_f32(self.switch_hold_secs - d.as_secs_f32())
                {
                    self.stop_after = Some(toggled_elapsed.add(d));
                }
            } else {
                self.stop_after = Some(toggled_elapsed.add(d));
            }
            info!(
                "<d>- - -</> ♾️ {:?} prolonged: <b>{}</> <cyan>(</><magenta>{}</><cyan>)</>, duration added: <yellow>{}</>",
                kind,
                self.name,
                dest_name,
                format_duration(d),
            );
        }
        false
    }
}

pub struct RelayBoard {
    pub relay: [Option<Device>; 8],
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
        let file = OpenOptions::new().write(true).open(data_path);
        match file {
            Ok(file) => {
                self.file = Some(file);
            }
            Err(e) => {
                error!(
                    "{}: error opening file {:?}: {:?}",
                    get_w1_device_name(self.ow_family, self.ow_address),
                    data_path.display(),
                    e,
                );
            }
        }
    }

    fn save_state(&mut self) {
        if self.file.is_none() {
            self.open();
        }

        match &mut self.file {
            Some(file) => match self.new_value {
                Some(val) => {
                    info!(
                        "{}: 💾 saving output byte: {:#04x}",
                        get_w1_device_name(self.ow_family, self.ow_address),
                        val
                    );
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
                    let new_value = [val; 1];
                    match file.write_all(&new_value) {
                        Ok(_) => {
                            self.last_value = Some(val);
                            self.new_value = None;
                        }
                        Err(e) => {
                            error!(
                                "{}: error writing output byte: {:?}",
                                get_w1_device_name(self.ow_family, self.ow_address),
                                e,
                            );
                        }
                    }
                }
                _ => {}
            },
            None => (),
        }
    }

    fn get_actual_state(&self) -> u8 {
        //we will be computing new output byte for a relay board
        //so first of all get the base/previous value
        self.new_value
            .unwrap_or(self.last_value.unwrap_or(DS2408_INITIAL_STATE))
    }
}

pub struct Yeelight {
    pub dev: Device,
    pub ip_address: String,
    pub powered_on: bool,
}

#[derive(Serialize)]
struct YeelightCommand {
    id: u32,
    method: String,
    #[serde(serialize_with = "Yeelight::params_serialize")]
    params: Vec<String>,
}
#[derive(Deserialize)]
struct YeelightResult {
    id: u32,
    result: Vec<String>,
}

impl Yeelight {
    fn params_serialize<S>(params: &Vec<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(params.len()))?;
        for (pos, elem) in params.iter().enumerate() {
            if pos == 2 {
                //converting last parameter (duration of effect) to integer
                let duration: u32 = elem.parse().unwrap_or_default();
                seq.serialize_element(&duration)?;
            } else {
                //leaving as String
                seq.serialize_element(&elem)?;
            }
        }
        seq.end()
    }

    fn yeelight_tcp_command(yeelight_name: String, ip_addr: String, turn_on: bool) {
        let on_off = if turn_on { "on" } else { "off" };
        let id = 1;
        let cmd = YeelightCommand {
            id: id,
            method: YEELIGHT_METHOD_SET_POWER.to_owned(),
            params: vec![
                on_off.to_owned(),
                YEELIGHT_EFFECT.to_owned(),
                YEELIGHT_DURATION_MS.to_string(),
            ],
        };

        // serialize command to a JSON string
        let mut json_cmd = serde_json::to_string(&cmd).unwrap();
        debug!(
            "Yeelight: {}: generated JSON command={:?}",
            yeelight_name, json_cmd
        );

        for _ in 1..=3 {
            debug!("Yeelight: {}: connecting...", yeelight_name);
            match TcpStream::connect(format!("{}:{}", ip_addr, YEELIGHT_TCP_PORT)) {
                Err(e) => {
                    error!("Yeelight: {}: connection error: {:?}", yeelight_name, e);
                }
                Ok(mut stream) => {
                    debug!("Yeelight: {}: connected, sending command", yeelight_name);
                    json_cmd.push_str("\r\n"); //specs requirement
                    match stream.write_all(json_cmd.as_bytes()) {
                        Ok(_) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs_f32(1.5)));
                            let mut reader = BufReader::new(stream.try_clone().unwrap());

                            //read a line with json result from yeelight
                            let mut raw_result = String::new();
                            let _ = reader.read_line(&mut raw_result);

                            //try to parse json
                            match serde_json::from_str::<YeelightResult>(&raw_result) {
                                Ok(json_res) => {
                                    //check for correct command result
                                    if json_res.id == id && json_res.result == vec!["ok"] {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "Yeelight: {}: error parsing result JSON: {:?}\nraw input data: {:?}",
                                        yeelight_name, e, raw_result
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(
                                "Yeelight: {}: cannot write to socket: {:?}",
                                yeelight_name, e
                            );
                        }
                    }
                }
            }
        }
    }

    fn turn_on_off(&mut self, turn_on: bool) {
        let yeelight_name = self.dev.name.clone();
        let ip_address = self.ip_address.clone();
        thread::spawn(move || Yeelight::yeelight_tcp_command(yeelight_name, ip_address, turn_on));

        self.powered_on = turn_on;
        self.dev.last_toggled = Some(Instant::now());
    }
}

pub struct SensorDevices {
    pub kinds: HashMap<i32, String>,
    pub sensor_boards: Vec<SensorBoard>,
    pub max_cesspool_level: usize,
}

pub struct RelayDevices {
    pub relay_boards: Vec<RelayBoard>,
    pub yeelight: Vec<Yeelight>,
}

impl SensorDevices {
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
        tags: Vec<String>,
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

        //find a max index for cesspool level
        for tag in tags
            .iter()
            .filter(|&s| s.starts_with("cesspool"))
            .into_iter()
        {
            let v: Vec<&str> = tag.split(":").collect();
            match v.get(1) {
                Some(&index_string) => match index_string.parse::<usize>() {
                    Ok(index) => {
                        if self.max_cesspool_level < index {
                            self.max_cesspool_level = index
                        }
                    }
                    Err(_) => (),
                },
                None => (),
            }
        }

        //create and attach a sensor
        let sensor = Sensor {
            id_sensor,
            id_kind,
            name,
            tags,
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
}

impl RelayDevices {
    pub fn add_relay(
        &mut self,
        id_relay: i32,
        name: String,
        family_code: Option<i16>,
        address: u64,
        bit: u8,
        pir_exclude: bool,
        pir_hold_secs: Option<f32>,
        switch_hold_secs: Option<f32>,
        initial_state: bool,
        pir_all_day: bool,
        tags: Vec<String>,
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
                relay_board.last_value = Some(DS2408_INITIAL_STATE);

                relay_board.open();
                self.relay_boards.push(relay_board);
                self.relay_boards.last_mut().unwrap()
            }
        };

        //if the initial_state is true, then we are turning on this relay
        if initial_state {
            let mut new_state = relay_board.last_value.unwrap_or(DS2408_INITIAL_STATE);
            new_state = new_state & !(1 << bit as u8);
            warn!(
                "{}: Initial state is active for: {}: bit={} new state: {:#04x}",
                get_w1_device_name(relay_board.ow_family, relay_board.ow_address),
                name.clone(),
                bit,
                new_state,
            );
            relay_board.new_value = Some(new_state);
        }

        let old_relay = &relay_board.relay[bit as usize];

        //create and attach a relay
        let relay = Device {
            id: id_relay,
            name: name.clone(),
            tags,
            pir_exclude,
            pir_hold_secs: pir_hold_secs.unwrap_or(DEFAULT_PIR_HOLD_SECS),
            switch_hold_secs: switch_hold_secs.unwrap_or(DEFAULT_SWITCH_HOLD_SECS),
            pir_all_day,
            override_mode: {
                if let Some(old_relay) = old_relay {
                    if old_relay.id == id_relay {
                        if old_relay.override_mode {
                            info!(
                                "{}: {}: 📌 override_mode preserved",
                                name,
                                get_w1_device_name(relay_board.ow_family, relay_board.ow_address),
                            );
                        };
                        old_relay.override_mode
                    } else {
                        initial_state
                    }
                } else {
                    initial_state
                }
            },
            last_toggled: {
                if let Some(old_relay) = old_relay {
                    if old_relay.id == id_relay {
                        if old_relay.last_toggled.is_some() {
                            info!(
                                "{}: {}: 📌 last_toggled preserved ({})",
                                get_w1_device_name(relay_board.ow_family, relay_board.ow_address),
                                name,
                                format_duration(old_relay.last_toggled.unwrap().elapsed()),
                            );
                        };
                        old_relay.last_toggled
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
            stop_after: {
                if let Some(old_relay) = old_relay {
                    if old_relay.id == id_relay {
                        if old_relay.stop_after.is_some() {
                            info!(
                                "{}: {}: 📌 stop_after preserved ({})",
                                get_w1_device_name(relay_board.ow_family, relay_board.ow_address),
                                name,
                                format_duration(old_relay.stop_after.unwrap()),
                            );
                        };
                        old_relay.stop_after
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
        };
        relay_board.relay[bit as usize] = Some(relay);
    }

    pub fn add_yeelight(
        &mut self,
        id_yeelight: i32,
        name: String,
        ip_address: String,
        pir_exclude: bool,
        pir_hold_secs: Option<f32>,
        switch_hold_secs: Option<f32>,
        pir_all_day: bool,
        tags: Vec<String>,
    ) {
        //create and add a yeelight
        let dev = Device {
            id: id_yeelight,
            name,
            tags,
            pir_exclude,
            pir_hold_secs: pir_hold_secs.unwrap_or(DEFAULT_PIR_HOLD_SECS),
            switch_hold_secs: switch_hold_secs.unwrap_or(DEFAULT_SWITCH_HOLD_SECS),
            pir_all_day,
            override_mode: false,
            last_toggled: None,
            stop_after: None,
        };
        let light = Yeelight {
            dev,
            ip_address,
            powered_on: false,
        };
        self.yeelight.push(light);
    }

    pub fn relay_sensor_trigger(
        &mut self,
        state_machine: &mut StateMachine,
        associated_relays: &Vec<i32>,
        kind_code: &str,
        on: bool,
        night: bool,
    ) {
        for rb in &mut self.relay_boards {
            for i in 0..=7 {
                match &mut rb.relay[i] {
                    Some(relay) => {
                        if associated_relays.contains(&relay.id) {
                            //check hook function result and stop processing when needed
                            let stop_processing = !state_machine.device_hook(
                                &kind_code,
                                on,
                                &relay.tags,
                                night,
                                relay.id,
                            );
                            if stop_processing {
                                debug!(
                                    "{}: {}: stopped processing",
                                    get_w1_device_name(rb.ow_family, rb.ow_address),
                                    relay.name,
                                );
                                continue;
                            }

                            let mut new_state: u8 = rb
                                .new_value
                                .unwrap_or(rb.last_value.unwrap_or(DS2408_INITIAL_STATE));

                            match kind_code.as_ref() {
                                "PIR_Trigger" => {
                                    //check if bit is set (relay is off)
                                    let currently_off = new_state & (1 << i as u8) != 0;
                                    if relay.turn_on_prolong(
                                        ProlongKind::PIR,
                                        night,
                                        format!(
                                            "relay:{}|bit:{}",
                                            get_w1_device_name(rb.ow_family, rb.ow_address),
                                            i
                                        ),
                                        on,
                                        currently_off,
                                        None,
                                    ) {
                                        new_state = new_state & !(1 << i as u8);
                                        rb.new_value = Some(new_state);
                                    }
                                }
                                "Switch" => {
                                    if relay.turn_on_prolong(
                                        ProlongKind::Switch,
                                        night,
                                        format!(
                                            "relay:{}|bit:{}",
                                            get_w1_device_name(rb.ow_family, rb.ow_address),
                                            i
                                        ),
                                        on,
                                        false,
                                        None,
                                    ) {
                                        //switching is toggling current state to the opposite:
                                        new_state = new_state ^ (1 << i as u8);
                                        rb.new_value = Some(new_state);
                                    }
                                }
                                _ => (),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn yeelight_sensor_trigger(
        &mut self,
        state_machine: &mut StateMachine,
        onewire: &OneWire,
        associated_yeelights: &Vec<i32>,
        kind_code: &str,
        on: bool,
        night: bool,
    ) {
        for yeelight in &mut self.yeelight {
            if associated_yeelights.contains(&yeelight.dev.id) {
                //check hook function result and stop processing when needed
                let stop_processing = !state_machine.device_hook(
                    &kind_code,
                    on,
                    &yeelight.dev.tags,
                    night,
                    yeelight.dev.id,
                );
                if stop_processing {
                    debug!("Yeelight: {}: stopped processing", yeelight.dev.name,);
                    continue;
                }

                match kind_code.as_ref() {
                    "PIR_Trigger" => {
                        if yeelight.dev.turn_on_prolong(
                            ProlongKind::PIR,
                            night,
                            format!("yeelight:{}", yeelight.ip_address),
                            on,
                            !yeelight.powered_on,
                            None,
                        ) {
                            yeelight.turn_on_off(true);
                            onewire.increment_yeelight_counter(yeelight.dev.id);
                        }
                    }
                    "Switch" => {
                        if yeelight.dev.turn_on_prolong(
                            ProlongKind::Switch,
                            night,
                            format!("yeelight:{}", yeelight.ip_address),
                            on,
                            false,
                            None,
                        ) {
                            //switching is toggling current state to the opposite:
                            yeelight.turn_on_off(!yeelight.powered_on);
                            onewire.increment_yeelight_counter(yeelight.dev.id);
                        }
                    }
                    _ => (),
                }
            }
        }
    }
}

pub struct CesspoolLevel {
    pub level: Vec<Option<bool>>,
}

impl CesspoolLevel {
    fn got_all_sensors(&mut self) -> bool {
        self.level.iter().filter(|l| l.is_none()).count() == 0
    }
    fn get_level_lcd(&self) -> u8 {
        self.level.iter().flatten().filter(|&x| *x == true).count() as u8
    }
    fn get_level_percentage(&self) -> u8 {
        (((self.level.iter().flatten().filter(|&x| *x == true).count() as f32)
            / self.level.len() as f32)
            * 100f32) as u8
    }
}

impl fmt::Display for CesspoolLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for elem in &self.level {
            match elem {
                Some(val) => {
                    if *val {
                        write!(f, "🔴🔴🔴🔴")?;
                    } else {
                        write!(f, "⚫⚫⚫⚫")?;
                    }
                }
                _ => (),
            }
        }

        Ok(())
    }
}

pub struct StateMachine {
    pub name: String,
    pub alarm_armed: bool,
    pub bedroom_mode: bool,
    pub wicket_gate_started: Option<Instant>,
    pub wicket_gate_delay: Option<Duration>,
    pub wicket_gate_relays: Vec<i32>,
    pub ethlcd: Option<EthLcd>,
    pub rfid_tags: Arc<RwLock<Vec<RfidTag>>>,
    pub rfid_pending_tags: Arc<RwLock<Vec<u32>>>,
    pub cesspool_level: CesspoolLevel,
    pub lcd_transmitter: Sender<LcdTask>,
    pub db_transmitter: Sender<DbTask>,
}

impl StateMachine {
    pub fn run_shell_command(cmd: String) {
        info!("StateMachine: about to call external command: {}", cmd);
        //we have a command and args in one string, split it by first space
        let mut args: Vec<&str> = cmd.splitn(2, " ").collect();
        let output = Command::new(args.remove(0))
            .args(args)
            .output()
            .expect("Error calling script");
        info!(
            "StateMachine: script call result:\nstdout: {:?}\nstderr: {:?}",
            String::from_utf8(output.stdout),
            String::from_utf8(output.stderr)
        );
    }

    /* all below hook functions are returning bool, which means:
    true - continue processing
    false - stop processing the event (don't turn the relays, etc) */

    fn sensor_hook(
        &mut self,
        sensor_kind_code: &str,
        sensor_name: &str,
        sensor_on: bool,
        sensor_tags: &Vec<String>,
        night: bool,
        initial_read: bool,
        pending_tasks: &mut Vec<OneWireTask>,
        id_sensor: i32,
    ) -> bool {
        //bedroom mode handling during the night
        if !initial_read && sensor_kind_code == "PIR_Trigger" && sensor_on && night {
            for tag in sensor_tags {
                match tag.as_ref() {
                    "bedroom_enable" => {
                        return if !self.bedroom_mode {
                            info!("{}: bedroom mode enabled 🛌💤", self.name);
                            self.bedroom_mode = true;
                            true //allow single turn-on
                        } else {
                            false
                        };
                    }
                    "bedroom_disable" => {
                        if self.bedroom_mode {
                            info!("{}: bedroom mode disabled 🛏", self.name);
                            self.bedroom_mode = false;
                        }
                    }
                    _ => {}
                }
            }
        }

        //wicket gate mode opening
        //doing it in separate block as this tag has to be processed with highest priority
        if !initial_read {
            for tag in sensor_tags.iter().find(|&x| x.starts_with("wicket_gate")) {
                //shadow the outer variable
                let mut sensor_on = sensor_on;
                //check for inverted sensor logic
                if tag.contains("invert_state") {
                    sensor_on = !sensor_on;
                }
                if sensor_on {
                    match self.wicket_gate_started {
                        Some(started) => {
                            match self.wicket_gate_delay {
                                Some(delay) => {
                                    self.wicket_gate_started = None; //processed => clear
                                    if started.elapsed() < delay {
                                        info!("{}: opening wicket gate", self.name);
                                        for id_relay in &self.wicket_gate_relays {
                                            let new_task = OneWireTask {
                                                command: TaskCommand::TurnOnProlong,
                                                id_relay: Some(*id_relay),
                                                tag_group: None,
                                                id_yeelight: None,
                                                duration: None,
                                            };
                                            pending_tasks.push(new_task);
                                        }

                                        //confirmation beep
                                        match self.ethlcd.as_mut() {
                                            Some(ethlcd) => {
                                                ethlcd.async_beep(BeepMethod::Confirmation)
                                            }
                                            _ => {}
                                        }

                                        if night {
                                            info!("{}: turning on entry lights...", self.name);
                                            let new_task = OneWireTask {
                                                command: TaskCommand::TurnOnProlongNight,
                                                id_relay: None,
                                                tag_group: Some("entry_light".to_owned()),
                                                id_yeelight: None,
                                                duration: Some(Duration::from_secs_f32(
                                                    ENTRY_LIGHT_PROLONG_SECS,
                                                )),
                                            };
                                            pending_tasks.push(new_task);
                                        }

                                        return false; //stop further processing this sensor
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        //processing other tags
        for tag in sensor_tags {
            //shadow the outer variable
            let mut sensor_on = sensor_on;
            //check for inverted sensor logic
            if tag.contains("invert_state") {
                sensor_on = !sensor_on;
            }

            //if the sensor is tagged with 'monitor_in_influxdb' we are saving
            //all changes to influx for such sensor
            if tag.starts_with("monitor_in_influxdb") {
                let cmd = match sensor_on {
                    true => CommandCode::UpdateSensorStateOn,
                    false => CommandCode::UpdateSensorStateOff,
                };
                let task = DbTask {
                    command: cmd,
                    value: Some(id_sensor),
                };
                let _ = self.db_transmitter.send(task);
            }

            // by default we trigger on sensor_on but if the tag contains
            // the 'all_changes' modifier, then trigger on all changes
            if !initial_read && !(sensor_on || tag.contains("all_changes")) {
                continue;
            }

            if !initial_read {
                //run a shell script for sensors tagged with "cmd:"
                if tag.starts_with("cmd") {
                    let on_off = if sensor_on { "on" } else { "off" };

                    let v: Vec<&str> = tag.split(":").collect();
                    match v.get(1) {
                        Some(&command) => {
                            let mut cmd = command.to_string().clone();
                            cmd = str::replace(&cmd, "%name%", sensor_name);
                            cmd = str::replace(&cmd, "%colon%", ":");
                            cmd = str::replace(&cmd, "%state%", on_off);
                            thread::spawn(move || StateMachine::run_shell_command(cmd));
                        }
                        _ => (),
                    };
                }
                //doorbell => make a beep using ethlcd device
                else if self.ethlcd.is_some() && tag.starts_with("doorbell") {
                    self.ethlcd
                        .as_mut()
                        .unwrap()
                        .async_beep(BeepMethod::DoorBell);
                }
            }

            //cesspool level sensor
            if tag.starts_with("cesspool") {
                let v: Vec<&str> = tag.split(":").collect();
                match v.get(1) {
                    Some(&index_string) => match index_string.parse::<usize>() {
                        Ok(index) => {
                            self.cesspool_level.level[index - 1] = Some(sensor_on);
                            if self.cesspool_level.got_all_sensors() {
                                info!(
                                    "{}: 🛢 cesspool level: {} {}%",
                                    self.name,
                                    self.cesspool_level,
                                    self.cesspool_level.get_level_percentage()
                                );

                                //inform lcdproc thread about initial/new level
                                let task = LcdTask {
                                    command: LcdTaskCommand::SetCesspoolLevel,
                                    int_arg: self.cesspool_level.get_level_lcd(),
                                    string_arg: None,
                                };
                                let _ = self.lcd_transmitter.send(task);

                                //save cesspool level to influxdb
                                let task = DbTask {
                                    command: CommandCode::UpdateCesspoolLevel,
                                    value: Some(self.cesspool_level.get_level_percentage() as i32),
                                };
                                let _ = self.db_transmitter.send(task);
                            }
                        }
                        Err(_) => (),
                    },
                    _ => (),
                };
            }
        }

        true
    }

    fn device_hook(
        &mut self,
        sensor_kind_code: &str,
        sensor_on: bool,
        tags: &Vec<String>,
        night: bool,
        id: i32,
    ) -> bool {
        if sensor_kind_code == "PIR_Trigger" && sensor_on && night {
            for tag in tags {
                match tag.as_ref() {
                    "night_exclude" => {
                        return false;
                    }
                    _ => {}
                }
            }
        }

        for tag in tags {
            //if the relay is tagged with 'monitor_in_influxdb' we are saving
            //all changes to influx for such relay
            if tag.starts_with("monitor_in_influxdb") {
                let cmd = match sensor_on {
                    true => CommandCode::UpdateRelayStateOn,
                    false => CommandCode::UpdateRelayStateOff,
                };
                let task = DbTask {
                    command: cmd,
                    value: Some(id),
                };
                let _ = self.db_transmitter.send(task);
            }
        }

        true
    }

    fn process_rfid_tags(&mut self, pending_tasks: &mut Vec<OneWireTask>, night: bool) {
        let rfid_tags = self.rfid_tags.read().unwrap();
        let mut rfid_pending_tags = self.rfid_pending_tags.write().unwrap();
        if !rfid_pending_tags.is_empty() {
            //todo
            for id in rfid_pending_tags.iter() {
                debug!("{}: rfid_pending_tags: {:?}", self.name, id);
                for rfid_tag in rfid_tags.iter().find(|&x| x.id_tag as u32 == *id) {
                    info!("{}: 🆔 matched rfid_tag: {:?}", self.name, rfid_tag.name);

                    if !rfid_tag.tags.is_empty() {
                        //handle tags
                        for tag in &rfid_tag.tags {
                            //handle wicket_gate mode
                            if tag.starts_with("wicket_gate") {
                                let v: Vec<&str> = tag.split(":").collect();
                                match v.get(1) {
                                    Some(&delay_str) => {
                                        match delay_str.parse::<f32>() {
                                            Ok(val) => {
                                                let delay = Duration::from_secs_f32(val);
                                                self.wicket_gate_started = Some(Instant::now());
                                                self.wicket_gate_delay = Some(delay);
                                                self.wicket_gate_relays =
                                                    rfid_tag.associated_relays.clone();
                                                info!(
                                                    "{}: ⏹ enabling wicket gate mode for {:?}",
                                                    self.name, delay
                                                );

                                                //confirmation beep
                                                match self.ethlcd.as_mut() {
                                                    Some(ethlcd) => {
                                                        ethlcd.async_beep(BeepMethod::Confirmation)
                                                    }
                                                    _ => {}
                                                }

                                                if night {
                                                    info!(
                                                        "{}: 🏡 turning on entry lights...",
                                                        self.name
                                                    );
                                                    let new_task = OneWireTask {
                                                        command: TaskCommand::TurnOnProlongNight,
                                                        id_relay: None,
                                                        tag_group: Some("entry_light".to_owned()),
                                                        id_yeelight: None,
                                                        duration: Some(Duration::from_secs_f32(
                                                            ENTRY_LIGHT_PROLONG_SECS,
                                                        )),
                                                    };
                                                    pending_tasks.push(new_task);
                                                }
                                            }
                                            Err(e) => {
                                                error!("{}: delay parse error: {:?}", self.name, e);
                                            }
                                        }
                                    }
                                    None => {
                                        error!(
                                            "{}: wicket gate mode: missing delay parameter",
                                            self.name
                                        );
                                    }
                                };
                            }
                        }
                    } else {
                        //turn on associated relay
                        for id_relay in &rfid_tag.associated_relays {
                            info!("{}: 🔗 associated relay: {:?}", self.name, id_relay);
                            let new_task = OneWireTask {
                                command: TaskCommand::TurnOnProlong,
                                id_relay: Some(*id_relay),
                                tag_group: None,
                                id_yeelight: None,
                                duration: None,
                            };
                            pending_tasks.push(new_task);
                        }
                    }
                }
            }
            rfid_pending_tags.clear();
        }
    }
}

pub struct OneWire {
    pub name: String,
    pub transmitter: Sender<DbTask>,
    pub ow_receiver: Receiver<OneWireTask>,
    pub lcd_transmitter: Sender<LcdTask>,
    pub sensor_devices: Arc<RwLock<SensorDevices>>,
    pub relay_devices: Arc<RwLock<RelayDevices>>,
}

impl OneWire {
    fn increment_relay_counter(&self, id_relay: i32) {
        let task = DbTask {
            command: CommandCode::IncrementRelayCounter,
            value: Some(id_relay),
        };
        let _ = self.transmitter.send(task);
    }

    fn increment_yeelight_counter(&self, id_yeelight: i32) {
        let task = DbTask {
            command: CommandCode::IncrementYeelightCounter,
            value: Some(id_yeelight),
        };
        let _ = self.transmitter.send(task);
    }

    fn load_geolocation_config(&self, lat: &mut f64, lon: &mut f64) {
        let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
        let section = conf
            .section(Some("general".to_owned()))
            .expect("Cannot find general section in config");
        *lat = section
            .get("lat")
            .unwrap_or(&"0.0".to_owned())
            .parse()
            .unwrap_or_default();
        *lon = section
            .get("lon")
            .unwrap_or(&"0.0".to_owned())
            .parse()
            .unwrap_or_default();
    }

    pub fn worker(
        &self,
        worker_cancel_flag: Arc<AtomicBool>,
        ethlcd: Option<EthLcd>,
        rfid_tags: Arc<RwLock<Vec<RfidTag>>>,
        rfid_pending_tags: Arc<RwLock<Vec<u32>>>,
    ) {
        info!("{}: Starting thread", self.name);

        //show ethlcd config if set
        match &ethlcd {
            Some(device) => {
                info!(
                    "{}: ethlcd beep device host defined as: {:?}",
                    self.name, device.host
                );
            }
            None => {}
        }

        let mut state_machine = StateMachine {
            name: "statemachine".to_owned(),
            alarm_armed: false,
            bedroom_mode: false,
            wicket_gate_started: None,
            wicket_gate_delay: None,
            wicket_gate_relays: vec![],
            ethlcd,
            rfid_tags,
            rfid_pending_tags,
            cesspool_level: CesspoolLevel { level: vec![] },
            lcd_transmitter: self.lcd_transmitter.clone(),
            db_transmitter: self.transmitter.clone(),
        };

        let mut pending_tasks = vec![];

        //geo location for sun calculation
        let mut lat: f64 = 0.0;
        let mut lon: f64 = 0.0;
        let mut night_check = None;
        let mut night = false;
        self.load_geolocation_config(&mut lat, &mut lon);
        if lat != 0.0 && lon != 0.0 {
            night_check = Some(Instant::now());
            info!(
                "{}: 🌎 calculating sun position for lat: {}, long: {}",
                self.name, lat, lon
            );
        }

        let bits = vec![0, 2];
        let names = &["PIOA", "PIOB"];

        loop {
            let loop_start = Instant::now();
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            //checking for external relay tasks
            //fixme: read all tasks, not a single one at a call
            match self.ow_receiver.try_recv() {
                Ok(mut t) => {
                    debug!(
                        "Received OneWireTask: id_relay: {:?}, tag_group: {:?}, duration: {:?}",
                        t.id_relay, t.tag_group, t.duration
                    );
                    match t.command {
                        TaskCommand::TurnOnProlongNight => {
                            if night {
                                //change to normal prolong command
                                t.command = TaskCommand::TurnOnProlong;
                                pending_tasks.push(t);
                            }
                        }
                        _ => {
                            pending_tasks.push(t);
                        }
                    }
                }
                _ => (),
            }

            debug!("doing stuff");
            {
                let mut sensor_dev = self.sensor_devices.write().unwrap();
                let mut relay_dev = self.relay_devices.write().unwrap();

                //set a cesspool level size
                if state_machine.cesspool_level.level.len() < sensor_dev.max_cesspool_level {
                    state_machine
                        .cesspool_level
                        .level
                        .resize(sensor_dev.max_cesspool_level, None);
                }

                //fixme: do we really need to clone this HashMap to use it below?
                let kinds_cloned = sensor_dev.kinds.clone();

                for sb in &mut sensor_dev.sensor_boards {
                    match sb.read_state() {
                        //we have a read value to process
                        Some(new_value) => {
                            match sb.last_value {
                                Some(last_value) => {
                                    //we have last value to compare with
                                    if last_value != new_value {
                                        debug!(
                                            "{}: change detected, old: {:#04x} new: {:#04x}",
                                            get_w1_device_name(sb.ow_family, sb.ow_address),
                                            last_value,
                                            new_value
                                        );

                                        for bit in &bits {
                                            //check for bit change
                                            if new_value & (1 << bit) != last_value & (1 << bit) {
                                                let mut pio_name: &str = &"".to_string();
                                                let mut sensor: &Option<Sensor> = &None;
                                                if *bit == 0 {
                                                    sensor = &sb.pio_a;
                                                    pio_name = names[0];
                                                } else if *bit == 2 {
                                                    sensor = &sb.pio_b;
                                                    pio_name = names[1];
                                                }

                                                //check if we have attached sensor
                                                match sensor {
                                                    Some(sensor) => {
                                                        //db update task for sensor
                                                        let task = DbTask {
                                                            command:
                                                                CommandCode::IncrementSensorCounter,
                                                            value: Some(sensor.id_sensor),
                                                        };
                                                        let _ = self.transmitter.send(task);

                                                        let kind_code = kinds_cloned
                                                            .get(&sensor.id_kind)
                                                            .unwrap();
                                                        let on: bool = new_value & (1 << bit) != 0;

                                                        //check hook function result and stop processing when needed
                                                        let stop_processing = !state_machine
                                                            .sensor_hook(
                                                                &kind_code,
                                                                &sensor.name,
                                                                on,
                                                                &sensor.tags,
                                                                night,
                                                                false,
                                                                &mut pending_tasks,
                                                                sensor.id_sensor,
                                                            );
                                                        info!(
                                                            "<green>{}</>: <b>{}</> <cyan>(</><magenta>sensor:{}|{}</><cyan>)</>, value: {:#04x}, {}</>{}",
                                                            kind_code,
                                                            sensor.name,
                                                            get_w1_device_name(
                                                                sb.ow_family,
                                                                sb.ow_address
                                                            ),
                                                            pio_name,
                                                            new_value,
                                                            {if on {"<bold><green>active"} else {"<black>inactive"}},
                                                            {if stop_processing {", <yellow>stopped processing</>"} else {""}},
                                                        );
                                                        if stop_processing {
                                                            continue;
                                                        }

                                                        //trigger actions for relays
                                                        let associated_relays =
                                                            &sensor.associated_relays;
                                                        if !associated_relays.is_empty() {
                                                            relay_dev.relay_sensor_trigger(
                                                                &mut state_machine,
                                                                associated_relays,
                                                                kind_code,
                                                                on,
                                                                night,
                                                            );
                                                        }

                                                        //trigger actions for yeelights
                                                        let associated_yeelights =
                                                            &sensor.associated_yeelights;
                                                        if !associated_yeelights.is_empty() {
                                                            relay_dev.yeelight_sensor_trigger(
                                                                &mut state_machine,
                                                                self,
                                                                associated_yeelights,
                                                                kind_code,
                                                                on,
                                                                night,
                                                            );
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }

                                        //iteration over all boards that has changed state and needs a save_state()
                                        for rb in &mut relay_dev.relay_boards {
                                            match rb.new_value {
                                                Some(new_value) => {
                                                    let old_value = rb
                                                        .last_value
                                                        .unwrap_or(DS2408_INITIAL_STATE);
                                                    if new_value != old_value {
                                                        //checking all changed bits (relays) and set last_toggled Instant
                                                        for i in 0..=7 {
                                                            if new_value & (1 << i as u8)
                                                                != old_value & (1 << i as u8)
                                                            {
                                                                match &mut rb.relay[i] {
                                                                    Some(relay) => {
                                                                        relay.last_toggled =
                                                                            Some(Instant::now());
                                                                        self.increment_relay_counter(
                                                                            relay.id,
                                                                        );
                                                                    }
                                                                    _ => {}
                                                                }
                                                            }
                                                        }
                                                        rb.save_state();
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                None => {
                                    //sensor read for the very first time
                                    debug!(
                                        "{}: setting initial sensorboard value {:#04x}",
                                        get_w1_device_name(sb.ow_family, sb.ow_address),
                                        new_value
                                    );

                                    for bit in &bits {
                                        let mut pio_name: &str = &"".to_string();
                                        let mut sensor: &Option<Sensor> = &None;
                                        if *bit == 0 {
                                            sensor = &sb.pio_a;
                                            pio_name = names[0];
                                        } else if *bit == 2 {
                                            sensor = &sb.pio_b;
                                            pio_name = names[1];
                                        }

                                        //check if we have attached sensor
                                        match sensor {
                                            Some(sensor) => {
                                                let kind_code =
                                                    kinds_cloned.get(&sensor.id_kind).unwrap();
                                                let on: bool = new_value & (1 << bit) != 0;

                                                let _ = !state_machine.sensor_hook(
                                                    &kind_code,
                                                    &sensor.name,
                                                    on,
                                                    &sensor.tags,
                                                    night,
                                                    true,
                                                    &mut pending_tasks,
                                                    sensor.id_sensor,
                                                );
                                                debug!(
                                                    "initial state: {}: [{} {} {}]: {:#04x} on: {}",
                                                    kind_code,
                                                    get_w1_device_name(sb.ow_family, sb.ow_address),
                                                    pio_name,
                                                    sensor.name,
                                                    new_value,
                                                    on
                                                );
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            //processed -> save new value as the previous one:
                            sb.last_value = Some(new_value);
                        }
                        None => (),
                    }
                    thread::sleep(Duration::from_micros(500));
                }

                //checking day/night
                if night_check.is_some()
                    && night_check.unwrap().elapsed()
                        > Duration::from_secs_f32(SUN_POS_CHECK_INTERVAL_SECS)
                {
                    night_check = Some(Instant::now());
                    let start = SystemTime::now();
                    let since_the_epoch = start
                        .duration_since(UNIX_EPOCH)
                        .expect("Time went backwards");
                    let unixtime = since_the_epoch.as_millis();
                    let pos = sun::pos(unixtime as i64, lat, lon);
                    let az = pos.azimuth.to_degrees();
                    let alt = pos.altitude.to_degrees();
                    debug!("the position of the sun is az: {} / alt: {}", az, alt);
                    let new_night = alt < DAYLIGHT_SUN_DEGREE;

                    if night != new_night {
                        night = new_night;
                        if night {
                            info!("{}: Enabling night mode 🌙", self.name);
                        } else {
                            info!("{}: Disabling night mode 🌞", self.name);
                        }

                        for rb in &mut relay_dev.relay_boards {
                            let mut new_state: u8 = rb.get_actual_state();

                            //iteration on all relays and check 'all night' tag
                            for i in 0..=7 {
                                match &mut rb.relay[i] {
                                    Some(relay) => {
                                        let mut relay_marked: bool = false;
                                        for tag in &relay.tags {
                                            match tag.as_ref() {
                                                "all_night" => {
                                                    relay_marked = true;
                                                }
                                                _ => {}
                                            }
                                        }
                                        if relay_marked {
                                            if relay.turn_on_prolong(
                                                ProlongKind::DayNight,
                                                night,
                                                format!(
                                                    "relay:{}|bit:{}",
                                                    get_w1_device_name(rb.ow_family, rb.ow_address),
                                                    i
                                                ),
                                                night,
                                                false,
                                                None,
                                            ) {
                                                if night {
                                                    //turn ON relay
                                                    new_state = new_state & !(1 << i as u8);
                                                } else {
                                                    //turn OFF relay
                                                    new_state = new_state | (1 << i as u8);
                                                }
                                                rb.new_value = Some(new_state);
                                                self.increment_relay_counter(relay.id);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }

                            //save output state when needed
                            rb.save_state();
                        }
                    }
                }

                //process rfid pending tags, if any
                state_machine.process_rfid_tags(&mut pending_tasks, night);

                //checking for pending tasks
                if !pending_tasks.is_empty() {
                    //Yeelights
                    for yeelight in &mut relay_dev.yeelight {
                        let relay_tasks: Vec<OneWireTask> = pending_tasks
                            .clone()
                            .into_iter()
                            .filter(|t| match t.id_yeelight {
                                Some(id) => yeelight.dev.id == id,
                                None => match &t.tag_group {
                                    Some(tag_name) => yeelight.dev.tags.contains(tag_name),
                                    None => false,
                                },
                            })
                            .collect();
                        for t in &relay_tasks {
                            debug!("Processing OneWireTask: command={:?}, matched id_yeelight={}, duration={:?}", t.command, yeelight.dev.id, t.duration);

                            match t.command {
                                TaskCommand::TurnOnProlong => {
                                    //turn on or prolong
                                    if yeelight.dev.turn_on_prolong(
                                        ProlongKind::Remote,
                                        night,
                                        format!("yeelight:{}", yeelight.ip_address),
                                        true,
                                        !yeelight.powered_on,
                                        t.duration,
                                    ) {
                                        yeelight.turn_on_off(true);
                                        self.increment_yeelight_counter(yeelight.dev.id);
                                    }
                                }
                                TaskCommand::TurnOff => {
                                    if yeelight.dev.turn_on_prolong(
                                        ProlongKind::Remote,
                                        night,
                                        format!("yeelight:{}", yeelight.ip_address),
                                        false,
                                        !yeelight.powered_on,
                                        t.duration,
                                    ) {
                                        yeelight.turn_on_off(false);
                                        self.increment_yeelight_counter(yeelight.dev.id);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    //Relays
                    for rb in &mut relay_dev.relay_boards {
                        let mut new_state: u8 = rb.get_actual_state();

                        //iterate all relays in the board
                        for i in 0..=7 {
                            match &mut rb.relay[i] {
                                Some(relay) => {
                                    let relay_tasks: Vec<OneWireTask> = pending_tasks
                                        .clone()
                                        .into_iter()
                                        .filter(|t| match t.id_relay {
                                            Some(id) => relay.id == id,
                                            None => match &t.tag_group {
                                                Some(tag_name) => relay.tags.contains(tag_name),
                                                None => false,
                                            },
                                        })
                                        .collect();
                                    for t in &relay_tasks {
                                        debug!(
                                            "Processing OneWireTask: command={:?}, matched id_relay={}, duration={:?}",
                                            t.command, relay.id, t.duration
                                        );

                                        //check if bit is set (relay is off)
                                        let currently_off = new_state & (1 << i as u8) != 0;
                                        match t.command {
                                            TaskCommand::TurnOnProlong => {
                                                //turn on or prolong
                                                if relay.turn_on_prolong(
                                                    ProlongKind::Remote,
                                                    night,
                                                    format!(
                                                        "relay:{}|bit:{}",
                                                        get_w1_device_name(
                                                            rb.ow_family,
                                                            rb.ow_address
                                                        ),
                                                        i
                                                    ),
                                                    true,
                                                    currently_off,
                                                    t.duration,
                                                ) {
                                                    new_state = new_state & !(1 << i as u8);
                                                    rb.new_value = Some(new_state);
                                                }
                                            }
                                            TaskCommand::TurnOff => {
                                                if relay.turn_on_prolong(
                                                    ProlongKind::Remote,
                                                    night,
                                                    format!(
                                                        "relay:{}|bit:{}",
                                                        get_w1_device_name(
                                                            rb.ow_family,
                                                            rb.ow_address
                                                        ),
                                                        i
                                                    ),
                                                    false,
                                                    currently_off,
                                                    t.duration,
                                                ) {
                                                    //set a bit -> turn off relay
                                                    new_state = new_state | (1 << i as u8);
                                                    rb.new_value = Some(new_state);
                                                    self.increment_relay_counter(relay.id);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }

                        //save output state when needed
                        rb.save_state();
                    }
                    pending_tasks.clear();
                }

                //checking for auto turn-off of necessary relays
                for rb in &mut relay_dev.relay_boards {
                    let mut new_state: u8 = rb.get_actual_state();

                    //iteration on all relays and check elapsed time
                    for i in 0..=7 {
                        match &mut rb.relay[i] {
                            Some(relay) => {
                                match relay.last_toggled {
                                    Some(toggled) => {
                                        match relay.stop_after {
                                            Some(stop_after) => {
                                                if toggled.elapsed() > stop_after {
                                                    let currently_off =
                                                        new_state & (1 << i as u8) != 0;
                                                    if relay.turn_on_prolong(
                                                        ProlongKind::AutoOff,
                                                        night,
                                                        format!(
                                                            "relay:{}|bit:{}",
                                                            get_w1_device_name(
                                                                rb.ow_family,
                                                                rb.ow_address
                                                            ),
                                                            i
                                                        ),
                                                        false,
                                                        currently_off,
                                                        None,
                                                    ) {
                                                        //set a bit -> turn off relay
                                                        new_state = new_state | (1 << i as u8);
                                                        rb.new_value = Some(new_state);
                                                        self.increment_relay_counter(relay.id);
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }

                    //save output state when needed
                    rb.save_state();
                }

                //checking for auto turn-off of necessary yeelights
                for yeelight in &mut relay_dev.yeelight {
                    match yeelight.dev.last_toggled {
                        Some(toggled) => match yeelight.dev.stop_after {
                            Some(stop_after) => {
                                if toggled.elapsed() > stop_after {
                                    if yeelight.dev.turn_on_prolong(
                                        ProlongKind::AutoOff,
                                        night,
                                        format!("yeelight:{}", yeelight.ip_address),
                                        false,
                                        !yeelight.powered_on,
                                        None,
                                    ) {
                                        yeelight.turn_on_off(false);
                                        self.increment_yeelight_counter(yeelight.dev.id);
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }

            debug!(
                "Loop iteration total time: {} ms",
                loop_start.elapsed().as_millis()
            );
        }
        info!("{}: thread stopped", self.name);
    }
}
