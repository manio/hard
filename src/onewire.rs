use crate::database::{CommandCode, DbTask};
use crate::ethlcd::{BeepMethod, EthLcd};
use crate::lcdproc::{LcdTask, LcdTaskCommand};
use crate::rfid::RfidTag;
use flume::Receiver;
use flume::Sender;
use humantime::format_duration;
use ini::Ini;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use simplelog::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::BufReader;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::ops::Add;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

// Same generic error type as used in database.rs's `Result<T>` alias --
// needed so that OneWire::worker()'s future has the same Output type as the
// other workers spawned into the same tokio JoinSet in main.rs. Named
// differently (not `Result`) so it doesn't shadow the two-parameter
// std::result::Result used elsewhere in this file (e.g. serde's serializers).
type WorkerError = Box<dyn std::error::Error + Send + Sync>;

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
pub enum Operation {
    On,
    Off,
    Toggle,
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

//plain data rows loaded from the database, carried over a channel to the
//onewire coordinator task. Using plain structs here (instead of sharing
//SensorDevices/RelayDevices/Relays behind a lock) means the coordinator is
//the sole owner of its runtime state; the database task only ever *sends*
//a fresh snapshot, it never touches the live state directly.
#[derive(Clone)]
pub struct SensorRow {
    pub id_sensor: i32,
    pub id_kind: i32,
    pub name: String,
    pub family_code: Option<i16>,
    pub address: u64,
    pub bit: u8,
    pub associated_relays: Vec<i32>,
    pub associated_yeelights: Vec<i32>,
    pub tags: Vec<String>,
}

#[derive(Clone)]
pub struct RelayRow {
    pub id_relay: i32,
    pub name: String,
    pub family_code: Option<i16>,
    pub address: u64,
    pub bit: u8,
    pub pir_exclude: bool,
    pub pir_hold_secs: Option<f32>,
    pub switch_hold_secs: Option<f32>,
    pub initial_state: bool,
    pub pir_all_day: bool,
    pub tags: Vec<String>,
}

#[derive(Clone)]
pub struct YeelightRow {
    pub id_yeelight: i32,
    pub name: String,
    pub ip_address: String,
    pub pir_exclude: bool,
    pub pir_hold_secs: Option<f32>,
    pub switch_hold_secs: Option<f32>,
    pub pir_all_day: bool,
    pub tags: Vec<String>,
}

pub struct DeviceReloadData {
    pub kinds: HashMap<i32, String>,
    pub sensors: Vec<SensorRow>,
    pub relays: Vec<RelayRow>,
    pub yeelights: Vec<YeelightRow>,
    pub rfid_tags: Vec<RfidTag>,
}

//a single detected bus-level change, sent by the dedicated sensor-polling
//thread to the async coordinator. Carries only the raw address/value info --
//no sensor metadata (kinds/tags/associated_relays/...) crosses this channel,
//since that data lives solely in the coordinator's SensorDevices and is
//looked up there by (ow_family, ow_address) when the change is applied.
pub struct SensorBoardChange {
    pub ow_family: u8,
    pub ow_address: u64,
    pub old_value: Option<u8>,
    pub new_value: u8,
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
            || ((kind == ProlongKind::Remote
                || (kind == ProlongKind::AutoOff && !self.override_mode))
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
            ProlongKind::Switch => format!("🔲 Switch toggle {}", {
                if currently_off {
                    "💡"
                } else {
                    "◼️"
                }
            }),
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
                let mut duration;
                if (kind == ProlongKind::Remote && !on)
                    || kind == ProlongKind::AutoOff
                    || kind == ProlongKind::DayNight
                {
                    duration = "".to_string();
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
                    //mark that we was in override
                    if self.override_mode {
                        duration.push_str(" 🔓");
                    }
                    self.override_mode = false;
                } else {
                    duration = format!(", duration: <yellow>{}</>", format_duration(d));
                    if kind == ProlongKind::Switch {
                        self.override_mode = true;
                        duration.push_str(" 🔒");
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
            let mut duration = format!(", duration added: <yellow>{}</>", format_duration(d));
            if self.override_mode {
                if self.switch_hold_secs > d.as_secs_f32()
                    && toggled_elapsed
                        > Duration::from_secs_f32(self.switch_hold_secs - d.as_secs_f32())
                {
                    self.stop_after = Some(toggled_elapsed.add(d));
                } else {
                    duration = "".into();
                }
                //mark that we are in override mode
                duration.push_str(" 🔒");
            } else {
                self.stop_after = Some(toggled_elapsed.add(d));
            }
            info!(
                "<d>- - -</> ♾️ {:?} prolonged{}: <b>{}</> <cyan>(</><magenta>{}</><cyan>)</>{}",
                kind,
                {
                    if !self.override_mode {
                        ""
                    } else if !currently_off {
                        " 💡"
                    } else {
                        " ◼️"
                    }
                },
                self.name,
                dest_name,
                duration,
            );
        }
        false
    }
}

trait OnOff {
    fn currently_off(&self, index: Option<usize>) -> bool;
    fn get_dest_name(&self, index: Option<usize>) -> String;
    fn set_new_value(
        &mut self,
        op: Operation,
        index: Option<usize>,
        //only needed by Yeelight, to report a db counter increment -- passing
        //the transmitter directly means this trait no longer needs to know
        //about the whole OneWire struct
        db_transmitter: Option<&Sender<DbTask>>,
        dev: &mut Device,
    );
    fn sensor_trigger(
        &mut self,
        device: &mut Device,
        index: Option<usize>,
        state_machine: &mut StateMachine,
        db_transmitter: Option<&Sender<DbTask>>,
        associated_devices: &Vec<i32>,
        kind_code: &str,
        on: bool,
        night: bool,
    ) {
        let currently_off = self.currently_off(index);
        let dest_name = self.get_dest_name(index);
        if associated_devices.contains(&device.id) {
            //check hook function result and stop processing when needed
            let stop_processing =
                !state_machine.device_hook(&kind_code, on, &device.tags, night, device.id);
            if stop_processing {
                debug!("{}: {}: stopped processing", dest_name, device.name);
                return;
            }

            match kind_code.as_ref() {
                "PIR_Trigger" => {
                    if device.turn_on_prolong(
                        ProlongKind::PIR,
                        night,
                        dest_name,
                        on,
                        currently_off,
                        None,
                    ) {
                        self.set_new_value(Operation::On, index, db_transmitter, device);
                    }
                }
                "Switch" => {
                    if device.turn_on_prolong(
                        ProlongKind::Switch,
                        night,
                        dest_name,
                        on,
                        currently_off,
                        None,
                    ) {
                        self.set_new_value(Operation::Toggle, index, db_transmitter, device);
                    }
                }
                _ => (),
            }
        }
    }
}

pub struct RelayBoard {
    pub relay: [Option<i32>; 8],
    pub ow_family: u8,
    pub ow_address: u64,
    pub new_value: Option<u8>,
    pub last_value: Option<u8>,
    //async I/O: writing an output byte to the w1 sysfs file no longer
    //blocks the shared tokio runtime while it's in flight -- tokio::fs
    //offloads it to the blocking pool internally, same effect as
    //sensor_poller_thread's dedicated thread but without us having to
    //hand-roll it here. SensorBoard (below) intentionally keeps std::fs,
    //since its reads already happen on a dedicated blocking thread (see
    //sensor_poller_thread) where there is no async runtime to hand this to.
    pub file: Option<tokio::fs::File>,
}

impl RelayBoard {
    async fn open(&mut self) {
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
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(data_path)
            .await;
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

    async fn save_state(&mut self) {
        if self.file.is_none() {
            self.open().await;
        }

        match &mut self.file {
            Some(file) => match self.new_value {
                Some(val) => {
                    info!(
                        "{}: 💾 saving output byte: {:#04x}",
                        get_w1_device_name(self.ow_family, self.ow_address),
                        val
                    );
                    match file.seek(SeekFrom::Start(0)).await {
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
                    match file.write_all(&new_value).await {
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

impl OnOff for RelayBoard {
    fn currently_off(&self, index: Option<usize>) -> bool {
        //check if bit is set (relay is off)
        self.get_actual_state() & (1 << index.unwrap() as u8) != 0
    }

    fn get_dest_name(&self, index: Option<usize>) -> String {
        format!(
            "relay:{}|bit:{}",
            get_w1_device_name(self.ow_family, self.ow_address),
            index.unwrap()
        )
    }

    fn set_new_value(
        &mut self,
        op: Operation,
        index: Option<usize>,
        _db_transmitter: Option<&Sender<DbTask>>,
        _dev: &mut Device,
    ) {
        let mut new_state: u8 = self.get_actual_state();
        match op {
            Operation::On => new_state = new_state & !(1 << index.unwrap() as u8),
            Operation::Toggle => {
                //switching is toggling current state to the opposite:
                new_state = new_state ^ (1 << index.unwrap() as u8);
            }
            _ => (),
        }

        self.new_value = Some(new_state);
    }
}

pub struct Yeelight {
    pub id: i32,
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

    fn tasmota_command(yeelight_name: String, ip_addr: String, turn_on: bool) -> bool {
        let cmd = if turn_on { "Power On" } else { "Power off" };
        let url =
            reqwest::Url::parse_with_params(&format!("http://{}/cm", ip_addr), &[("cmnd", cmd)])
                .unwrap();
        debug!("URL = {:?}", url.as_str());

        for _ in 1..=3 {
            debug!(
                "Tasmota: {}: sending <blue>{}</> command...",
                yeelight_name, cmd
            );
            match reqwest::blocking::get(url.clone()) {
                Ok(resp) => {
                    if resp.status() == reqwest::StatusCode::OK {
                        return true;
                    } else {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                Err(e) => {
                    error!("Tasmota: {}: {}", yeelight_name, e);
                }
            }
        }
        false
    }

    fn turn_on_off(&mut self, turn_on: bool, dev: &Device) {
        let yeelight_name = dev.name.clone();
        let ip_address = self.ip_address.clone();
        if yeelight_name.starts_with("Nous") {
            thread::spawn(move || Yeelight::tasmota_command(yeelight_name, ip_address, turn_on));
        } else {
            thread::spawn(move || {
                Yeelight::yeelight_tcp_command(yeelight_name, ip_address, turn_on)
            });
        }

        self.powered_on = turn_on;
    }
}

impl OnOff for Yeelight {
    fn currently_off(&self, _index: Option<usize>) -> bool {
        !self.powered_on
    }

    fn get_dest_name(&self, _index: Option<usize>) -> String {
        format!("yeelight:{}", self.ip_address)
    }

    fn set_new_value(
        &mut self,
        op: Operation,
        _index: Option<usize>,
        db_transmitter: Option<&Sender<DbTask>>,
        dev: &mut Device,
    ) {
        let new_state = match op {
            Operation::On => true,
            Operation::Off => false,
            Operation::Toggle => !self.powered_on,
        };
        self.turn_on_off(new_state, dev);
        dev.last_toggled = Some(Instant::now());
        increment_yeelight_counter(db_transmitter.unwrap(), self.id);
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

pub struct Relays {
    pub relay: Vec<Device>,
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
        relays: &mut Vec<Device>,
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

                //file is opened lazily by save_state() on first write (it's
                //now async tokio::fs I/O, which add_relay() -- a sync
                //function called from apply_reload() -- can't .await here)
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

        let old_relay = relays.iter().find(|r| r.id == id_relay);

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
        relay_board.relay[bit as usize] = Some(id_relay);
        relays.retain(|r| r.id != id_relay);
        relays.push(relay);
    }

    pub fn add_yeelight(
        &mut self,
        relays: &mut Vec<Device>,
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
            id: id_yeelight,
            ip_address,
            powered_on: false,
        };
        self.yeelight.push(light);
        relays.retain(|r| r.id != id_yeelight);
        relays.push(dev);
    }

    pub fn relay_sensor_trigger(
        &mut self,
        relays: &mut Vec<Device>,
        state_machine: &mut StateMachine,
        associated_relays: &Vec<i32>,
        kind_code: &str,
        on: bool,
        night: bool,
    ) {
        for rb in &mut self.relay_boards {
            for i in 0..=7 {
                match rb.relay[i] {
                    Some(id) => {
                        let r = relays.iter_mut().find(|r| r.id == id);
                        match r {
                            Some(relay) => {
                                rb.sensor_trigger(
                                    relay,
                                    Some(i),
                                    state_machine,
                                    None,
                                    associated_relays,
                                    kind_code,
                                    on,
                                    night,
                                );
                            }
                            None => (),
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn yeelight_sensor_trigger(
        &mut self,
        relays: &mut Vec<Device>,
        state_machine: &mut StateMachine,
        db_transmitter: &Sender<DbTask>,
        associated_yeelights: &Vec<i32>,
        kind_code: &str,
        on: bool,
        night: bool,
    ) {
        for yeelight in &mut self.yeelight {
            let d = relays.iter_mut().find(|y| y.id == yeelight.id);
            match d {
                Some(dev) => {
                    yeelight.sensor_trigger(
                        dev,
                        None,
                        state_machine,
                        Some(db_transmitter),
                        associated_yeelights,
                        kind_code,
                        on,
                        night,
                    );
                }
                _ => (),
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
    pub rfid_tags: Vec<RfidTag>,
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
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
                            info!("{}: bedroom mode disabled 🛏️", self.name);
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
                                    "{}: 🛢️ cesspool level: {} {}%",
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
        //rfid_tags is now owned directly (updated on reload from the database
        //task), only rfid_pending_tags (written by the rfid thread) still
        //needs a lock since it has a different, independent owner
        let mut rfid_pending_tags = self.rfid_pending_tags.write().unwrap();
        if !rfid_pending_tags.is_empty() {
            //todo
            for id in rfid_pending_tags.iter() {
                debug!("{}: rfid_pending_tags: {:?}", self.name, id);
                for rfid_tag in self.rfid_tags.iter().find(|&x| x.id_tag as u32 == *id) {
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
                                                    "{}: ⏹️ enabling wicket gate mode for {:?}",
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

//what kind of Device a DeviceStatus entry describes -- relays and yeelights
//share the same Device struct (override_mode/last_toggled/stop_after), but
//"is it currently on" is read from a different place for each
#[derive(Clone, Copy, PartialEq)]
pub enum DeviceStatusKind {
    Relay,
    Yeelight,
}

//one entry per relay/yeelight currently away from its default (off,
//non-override) state. Built by collect_device_status() and handed back over
//a StatusQuery's oneshot channel -- this is the only way the webserver task
//gets to see coordinator state, since sensor_devices/relay_devices/relays
//are owned solely by the OneWire coordinator loop and are never behind a
//lock (see the OneWire struct's own doc comment below).
#[derive(Clone)]
pub struct DeviceStatus {
    pub id: i32,
    pub name: String,
    pub kind: DeviceStatusKind,
    pub is_on: bool,
    pub override_mode: bool,
    pub remaining: Option<Duration>, //time left until auto turn-off, if known
}

//sent by the webserver task to ask the coordinator for a snapshot of
//non-default device state; the coordinator answers once, over reply_tx
pub struct StatusQuery {
    pub reply_tx: tokio::sync::oneshot::Sender<Vec<DeviceStatus>>,
}

//Builds the "what's currently non-default" snapshot. A relay/yeelight is
//included if it's on, or in override_mode (a switch/remote toggle holding it
//away from automatic control) -- either condition means "not just sitting
//in its normal automatic state" and worth showing on the status page.
//remaining is computed from last_toggled (a monotonic Instant) and
//stop_after (a relative Duration), never converted to wall-clock time here
//-- the caller adds it to SystemTime::now() if it wants an ETA to display,
//since Instant itself carries no wall-clock meaning.
fn collect_device_status(relay_devices: &RelayDevices, relays: &Relays) -> Vec<DeviceStatus> {
    let mut result = Vec::new();

    let remaining_for = |dev: &Device| -> Option<Duration> {
        let stop_after = dev.stop_after?;
        let elapsed = dev.last_toggled?.elapsed();
        stop_after.checked_sub(elapsed)
    };

    for rb in &relay_devices.relay_boards {
        let actual_state = rb.get_actual_state();
        for (bit, id) in rb.relay.iter().enumerate() {
            let id = match id {
                Some(id) => *id,
                None => continue,
            };
            //active-low: bit clear means the relay is ON (see e.g.
            //check_day_night()/process_pending_tasks() elsewhere in this
            //file, which flip bits the same way)
            let is_on = actual_state & (1 << bit as u8) == 0;
            if let Some(dev) = relays.relay.iter().find(|r| r.id == id) {
                if is_on || dev.override_mode {
                    result.push(DeviceStatus {
                        id,
                        name: dev.name.clone(),
                        kind: DeviceStatusKind::Relay,
                        is_on,
                        override_mode: dev.override_mode,
                        remaining: remaining_for(dev),
                    });
                }
            }
        }
    }

    for yeelight in &relay_devices.yeelight {
        if let Some(dev) = relays.relay.iter().find(|r| r.id == yeelight.id) {
            if yeelight.powered_on || dev.override_mode {
                result.push(DeviceStatus {
                    id: yeelight.id,
                    name: dev.name.clone(),
                    kind: DeviceStatusKind::Yeelight,
                    is_on: yeelight.powered_on,
                    override_mode: dev.override_mode,
                    remaining: remaining_for(dev),
                });
            }
        }
    }

    result
}

pub struct OneWire {
    pub name: String,
    pub transmitter: Sender<DbTask>,
    pub ow_receiver: Receiver<OneWireTask>,
    pub lcd_transmitter: Sender<LcdTask>,
    pub reload_receiver: Receiver<DeviceReloadData>,
    pub status_receiver: Receiver<StatusQuery>,
    //owned directly: this coordinator task is the sole owner/mutator of all
    //three, so no lock is needed at all (the database task only ever *sends*
    //a fresh snapshot over reload_receiver, it never touches these directly)
    pub sensor_devices: SensorDevices,
    pub relay_devices: RelayDevices,
    pub relays: Relays,
}

fn increment_relay_counter(transmitter: &Sender<DbTask>, id_relay: i32) {
    let task = DbTask {
        command: CommandCode::IncrementRelayCounter,
        value: Some(id_relay),
    };
    let _ = transmitter.send(task);
}

fn increment_yeelight_counter(transmitter: &Sender<DbTask>, id_yeelight: i32) {
    let task = DbTask {
        command: CommandCode::IncrementYeelightCounter,
        value: Some(id_yeelight),
    };
    let _ = transmitter.send(task);
}

fn load_geolocation_config(lat: &mut f64, lon: &mut f64) {
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

//apply a fresh device list loaded from the database. This fully replaces
//kinds/sensor_boards and yeelights, while add_relay()/add_yeelight()
//internally preserve override_mode/last_toggled/stop_after for relays that
//still exist after the reload (unchanged logic, see RelayDevices::add_relay).
//Note: relay_boards themselves are intentionally NOT cleared here, matching
//the original database.rs behavior (a board no longer present in the DB is
//simply left in place / overwritten, never pruned).
//
//Returns the fresh set of (ow_family, ow_address) pairs so the caller can
//route them to the per-bus polling threads via route_boards_to_pollers().
//sensor_devices here keeps holding the *metadata* only
//(kinds/pio_a/pio_b/tags/associated_relays/...); the polling threads keep
//their own, separate lists for the actual file I/O.
fn apply_reload(
    sensor_devices: &mut SensorDevices,
    relay_devices: &mut RelayDevices,
    relays: &mut Relays,
    state_machine: &mut StateMachine,
    data: DeviceReloadData,
    name: &str,
) -> Vec<(u8, u64)> {
    info!("{}: applying reloaded device configuration", name);

    sensor_devices.kinds = data.kinds;
    sensor_devices.sensor_boards.clear();
    for row in data.sensors {
        sensor_devices.add_sensor(
            row.id_sensor,
            row.id_kind,
            row.name,
            row.family_code,
            row.address,
            row.bit,
            row.associated_relays,
            row.associated_yeelights,
            row.tags,
        );
    }

    let poller_boards: Vec<(u8, u64)> = sensor_devices
        .sensor_boards
        .iter()
        .map(|b| (b.ow_family, b.ow_address))
        .collect();

    for row in data.relays {
        relay_devices.add_relay(
            &mut relays.relay,
            row.id_relay,
            row.name,
            row.family_code,
            row.address,
            row.bit,
            row.pir_exclude,
            row.pir_hold_secs,
            row.switch_hold_secs,
            row.initial_state,
            row.pir_all_day,
            row.tags,
        );
    }

    relay_devices.yeelight.clear();
    for row in data.yeelights {
        relay_devices.add_yeelight(
            &mut relays.relay,
            row.id_yeelight,
            row.name,
            row.ip_address,
            row.pir_exclude,
            row.pir_hold_secs,
            row.switch_hold_secs,
            row.pir_all_day,
            row.tags,
        );
    }

    state_machine.rfid_tags = data.rfid_tags;

    //handed back to the caller (an async fn) rather than sent to the poller
    //right here, since routing it now needs to discover the w1 bus layout
    //first, and that's blocking I/O -- see route_boards_to_pollers()
    poller_boards
}

//Parses a w1 device name ("family-address" hex, e.g. "3a-0000000e2c0b") as
//produced by the kernel and used as sysfs entry/symlink names -- the inverse
//of get_w1_device_name() above.
fn parse_w1_device_name(name: &str) -> Option<(u8, u64)> {
    let (family_str, address_str) = name.split_once('-')?;
    let family = u8::from_str_radix(family_str, 16).ok()?;
    let address = u64::from_str_radix(address_str, 16).ok()?;
    Some((family, address))
}

//Reads /sys/bus/w1/devices/w1_bus_masterN/w1_master_slaves for every master
//currently present, building a (family, address) -> bus number map. This is
//a fast, purely in-kernel-memory sysfs read (the driver's already-known
//slave list, not a live bus scan), but it's still blocking I/O, so callers
//run it via spawn_blocking rather than directly on the async runtime.
//
//The w1 core already serializes all traffic per physical bus behind its own
//mutex (mutex_lock(&sl->master->bus_mutex) in w1_reset_select_slave(), see
//e.g. w1_ds2413.c) -- grouping our own reads by the same bus means two
//different buses' reads no longer wait on each other in one flat sequential
//loop; they only wait on each other's actual hardware transactions if they
//happen to share the same underlying I2C adapter (see route_boards_to_pollers()).
fn discover_w1_bus_map() -> HashMap<(u8, u64), u32> {
    let mut map = HashMap::new();

    let root = match std::fs::read_dir(W1_ROOT_PATH) {
        Ok(entries) => entries,
        Err(e) => {
            error!("failed to read {}: {:?}", W1_ROOT_PATH, e);
            return map;
        }
    };

    for entry in root.flatten() {
        let file_name = entry.file_name();
        let entry_name = file_name.to_string_lossy();
        let bus_num: u32 = match entry_name.strip_prefix("w1_bus_master") {
            Some(suffix) => match suffix.parse() {
                Ok(n) => n,
                Err(_) => continue,
            },
            None => continue,
        };

        let slaves_path = format!("{}/{}/w1_master_slaves", W1_ROOT_PATH, entry_name);
        let contents = match std::fs::read_to_string(&slaves_path) {
            Ok(c) => c,
            //e.g. permission error, or the master vanished between listing
            //the directory and reading this file -- just skip it
            Err(_) => continue,
        };

        for line in contents.lines() {
            let line = line.trim();
            //empty when no slaves are attached ("not found." on some kernels)
            if line.is_empty() || line == "not found." {
                continue;
            }
            if let Some(addr) = parse_w1_device_name(line) {
                map.insert(addr, bus_num);
            }
        }
    }

    map
}

//Splits a flat address list across one dedicated polling thread per
//physical w1 bus (spawning new ones on demand, and parking -- not killing --
//threads for buses that temporarily have no configured devices). Addresses
//that can't currently be placed on a known bus (e.g. briefly not listed in
//w1_master_slaves) still get polled, just grouped under a synthetic
//"unknown" bus id, so they're never silently dropped.
async fn route_boards_to_pollers(
    addresses: Vec<(u8, u64)>,
    bus_pollers: &mut HashMap<u32, Sender<Vec<(u8, u64)>>>,
    change_tx: &Sender<SensorBoardChange>,
    cancel_flag: &Arc<AtomicBool>,
    name: &str,
) {
    const UNKNOWN_BUS: u32 = u32::MAX;

    let bus_map = tokio::task::spawn_blocking(discover_w1_bus_map)
        .await
        .expect("w1 bus discovery task panicked");

    let mut grouped: HashMap<u32, Vec<(u8, u64)>> = HashMap::new();
    for addr in addresses {
        let bus = bus_map.get(&addr).copied().unwrap_or(UNKNOWN_BUS);
        grouped.entry(bus).or_default().push(addr);
    }

    //every bus we already have a thread for, plus every bus that just
    //showed up in this reload -- a bus no longer present gets an empty
    //list below so its thread parks instead of polling stale addresses
    let all_buses: HashSet<u32> = bus_pollers
        .keys()
        .copied()
        .chain(grouped.keys().copied())
        .collect();

    for bus in all_buses {
        let boards = grouped.remove(&bus).unwrap_or_default();

        let sender = bus_pollers.entry(bus).or_insert_with(|| {
            let (boards_tx, boards_rx) = flume::unbounded();
            let poller_cancel_flag = cancel_flag.clone();
            let poller_change_tx = change_tx.clone();
            let poller_name = format!("{}-bus{}", name, bus);
            tokio::task::spawn_blocking(move || {
                sensor_poller_thread(boards_rx, poller_change_tx, poller_cancel_flag, poller_name);
            });
            boards_tx
        });

        if let Err(e) = sender.send(boards) {
            error!(
                "{}: failed to send updated board list to bus {} poller thread: {:?}",
                name, bus, e
            );
        }
    }
}

//Runs on a dedicated blocking-pool thread for the whole lifetime of the
//onewire task (one instance per physical w1 bus, spawned on demand by
//route_boards_to_pollers() the first time that bus is seen -- not
//per-iteration). Owns its own private list of SensorBoard entries -- used
//here purely for their open()/read_state() I/O, never for the
//pio_a/pio_b/kinds/tags metadata, which stays solely in the coordinator's
//SensorDevices. This means no lock is needed anywhere: the per-bus lists
//and the coordinator's metadata list are independent, and are kept in sync
//only by the address lists the coordinator pushes over boards_rx every time
//a device reload happens.
//
//Detected changes are pushed to the coordinator over change_tx (shared by
//every bus's thread -- flume senders are freely cloneable) as soon as
//they're found; the coordinator applies them asynchronously and drives its
//own pacing (see apply_sensor_change() and the main loop in worker()).
fn rebuild_poller_boards(addrs: Vec<(u8, u64)>) -> Vec<SensorBoard> {
    addrs
        .into_iter()
        .map(|(ow_family, ow_address)| SensorBoard {
            pio_a: None,
            pio_b: None,
            ow_family,
            ow_address,
            last_value: None,
            file: None,
        })
        .collect()
}

fn sensor_poller_thread(
    boards_rx: Receiver<Vec<(u8, u64)>>,
    change_tx: Sender<SensorBoardChange>,
    cancel_flag: Arc<AtomicBool>,
    name: String,
) {
    info!("{}: sensor poller thread starting", name);
    let mut boards: Vec<SensorBoard> = Vec::new();

    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        //apply the latest board list sent by the coordinator, if any (a
        //device reload rebuilds this list from scratch, same as the
        //coordinator's own metadata copy -- last_value/file are reset,
        //matching the pre-existing reload behavior)
        while let Ok(addrs) = boards_rx.try_recv() {
            boards = rebuild_poller_boards(addrs);
        }

        if boards.is_empty() {
            //nothing to poll yet (e.g. before the first device reload
            //arrives) -- wait a bit for one to show up, but don't just
            //discard it if it does: recv_timeout() actually consumes the
            //message from the channel, so if we throw the Ok(..) away here
            //instead of applying it, the board list is lost for good and
            //this thread never polls anything.
            if let Ok(addrs) = boards_rx.recv_timeout(Duration::from_millis(200)) {
                boards = rebuild_poller_boards(addrs);
            }
            continue;
        }

        for sb in &mut boards {
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            if let Some(new_value) = sb.read_state() {
                if sb.last_value != Some(new_value) {
                    let change = SensorBoardChange {
                        ow_family: sb.ow_family,
                        ow_address: sb.ow_address,
                        old_value: sb.last_value,
                        new_value,
                    };
                    sb.last_value = Some(new_value);
                    if change_tx.send(change).is_err() {
                        //coordinator gone -- nothing more to do
                        info!("{}: sensor poller thread stopping (coordinator gone)", name);
                        return;
                    }
                }
            }
            // Throttle consecutive sysfs/w1 reads. The 1-Wire protocol timing
            // itself is handled by the kernel w1 master driver.
            thread::sleep(Duration::from_micros(50));
        }
    }
    info!("{}: sensor poller thread stopped", name);
}

//Apply one change reported by the sensor-polling thread. sensor_devices is
//only ever read here (metadata lookup by address) -- ownership of the
//SensorBoard entries used for actual I/O lives solely in the poller thread,
//so nothing here needs a lock.
async fn apply_sensor_change(
    sensor_devices: &SensorDevices,
    relay_devices: &mut RelayDevices,
    relays: &mut Relays,
    state_machine: &mut StateMachine,
    transmitter: &Sender<DbTask>,
    pending_tasks: &mut Vec<OneWireTask>,
    night: bool,
    name: &str,
    change: SensorBoardChange,
) {
    let SensorBoardChange {
        ow_family,
        ow_address,
        old_value: last_value,
        new_value,
    } = change;

    let idx = match sensor_devices
        .sensor_boards
        .iter()
        .position(|b| b.ow_family == ow_family && b.ow_address == ow_address)
    {
        Some(idx) => idx,
        None => {
            //metadata for this board isn't known (yet/anymore) -- e.g. a
            //reload racing with an in-flight reading -- nothing to
            //associate this change with, so just drop it
            return;
        }
    };

    let kinds_cloned = &sensor_devices.kinds;
    let bits = [0u8, 2u8];

    match last_value {
        Some(last_value) => {
            debug!(
                "{}: change detected, old: {:#04x} new: {:#04x}",
                get_w1_device_name(ow_family, ow_address),
                last_value,
                new_value
            );

            for bit in &bits {
                if new_value & (1 << bit) != last_value & (1 << bit) {
                    let (pio_name, sensor_clone) = {
                        let sb = &sensor_devices.sensor_boards[idx];
                        let sensor_opt = if *bit == 0 { &sb.pio_a } else { &sb.pio_b };
                        let pio_name = if *bit == 0 { "PIOA" } else { "PIOB" };
                        (
                            pio_name,
                            sensor_opt.as_ref().map(|s| {
                                (
                                    s.id_sensor,
                                    s.id_kind,
                                    s.name.clone(),
                                    s.tags.clone(),
                                    s.associated_relays.clone(),
                                    s.associated_yeelights.clone(),
                                )
                            }),
                        )
                    };

                    if let Some((
                        sensor_id,
                        id_kind,
                        sensor_name,
                        sensor_tags,
                        associated_relays,
                        associated_yeelights,
                    )) = sensor_clone
                    {
                        let task = DbTask {
                            command: CommandCode::IncrementSensorCounter,
                            value: Some(sensor_id),
                        };
                        let _ = transmitter.send(task);

                        let kind_code = kinds_cloned.get(&id_kind).unwrap().clone();
                        let on: bool = new_value & (1 << bit) != 0;

                        let stop_processing = !state_machine.sensor_hook(
                            &kind_code,
                            &sensor_name,
                            on,
                            &sensor_tags,
                            night,
                            false,
                            pending_tasks,
                            sensor_id,
                        );
                        info!(
                                    "<green>{}</>: <b>{}</> <cyan>(</><magenta>sensor:{}|{}</><cyan>)</>, value: {:#04x}, {}</>{}",
                                    kind_code,
                                    sensor_name,
                                    get_w1_device_name(ow_family, ow_address),
                                    pio_name,
                                    new_value,
                                    { if on { "<bold><green>active" } else { "<bright-black>inactive" } },
                                    { if stop_processing { ", <yellow>stopped processing</>" } else { "" } },
                                );
                        if stop_processing {
                            continue;
                        }

                        if !associated_relays.is_empty() {
                            relay_devices.relay_sensor_trigger(
                                &mut relays.relay,
                                state_machine,
                                &associated_relays,
                                &kind_code,
                                on,
                                night,
                            );
                        }

                        if !associated_yeelights.is_empty() {
                            relay_devices.yeelight_sensor_trigger(
                                &mut relays.relay,
                                state_machine,
                                transmitter,
                                &associated_yeelights,
                                &kind_code,
                                on,
                                night,
                            );
                        }
                    }
                }
            }

            //flush any relay boards that got a pending write from the triggers above
            for rb in &mut relay_devices.relay_boards {
                if let Some(new_value) = rb.new_value {
                    let old_value = rb.last_value.unwrap_or(DS2408_INITIAL_STATE);
                    if new_value != old_value {
                        for i in 0..=7 {
                            if new_value & (1 << i as u8) != old_value & (1 << i as u8) {
                                if let Some(id) = rb.relay[i] {
                                    if let Some(relay) =
                                        relays.relay.iter_mut().find(|r| r.id == id)
                                    {
                                        relay.last_toggled = Some(Instant::now());
                                        increment_relay_counter(transmitter, id);
                                    }
                                }
                            }
                        }
                        rb.save_state().await;
                    }
                }
            }
        }
        None => {
            //sensor read for the very first time
            debug!(
                "{}: setting initial sensorboard value {:#04x}",
                get_w1_device_name(ow_family, ow_address),
                new_value
            );
            for bit in &bits {
                let (pio_name, sensor_clone) = {
                    let sb = &sensor_devices.sensor_boards[idx];
                    let sensor_opt = if *bit == 0 { &sb.pio_a } else { &sb.pio_b };
                    let pio_name = if *bit == 0 { "PIOA" } else { "PIOB" };
                    (
                        pio_name,
                        sensor_opt
                            .as_ref()
                            .map(|s| (s.id_sensor, s.id_kind, s.name.clone(), s.tags.clone())),
                    )
                };

                if let Some((sensor_id, id_kind, sensor_name, sensor_tags)) = sensor_clone {
                    let kind_code = kinds_cloned.get(&id_kind).unwrap().clone();
                    let on: bool = new_value & (1 << bit) != 0;

                    let _ = !state_machine.sensor_hook(
                        &kind_code,
                        &sensor_name,
                        on,
                        &sensor_tags,
                        night,
                        true,
                        pending_tasks,
                        sensor_id,
                    );
                    debug!(
                        "initial state: {}: [{} {} {}]: {:#04x} on: {}",
                        kind_code,
                        get_w1_device_name(ow_family, ow_address),
                        pio_name,
                        sensor_name,
                        new_value,
                        on
                    );
                }
            }
        }
    }
}

async fn check_day_night(
    relay_devices: &mut RelayDevices,
    relays: &mut Relays,
    state_machine: &mut StateMachine,
    transmitter: &Sender<DbTask>,
    lat: f64,
    lon: f64,
    night: &mut bool,
    name: &str,
) {
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

    if *night == new_night {
        return;
    }
    *night = new_night;
    if *night {
        info!("{}: Enabling night mode 🌙", name);
    } else {
        info!("{}: Disabling night mode 🌞", name);
    }

    for rb in &mut relay_devices.relay_boards {
        let mut new_state: u8 = rb.get_actual_state();
        for i in 0..=7 {
            if let Some(id) = rb.relay[i] {
                if let Some(relay) = relays.relay.iter_mut().find(|r| r.id == id) {
                    let relay_marked = relay.tags.iter().any(|tag| tag == "all_night");
                    if relay_marked {
                        if relay.turn_on_prolong(
                            ProlongKind::DayNight,
                            *night,
                            rb.get_dest_name(Some(i)),
                            *night,
                            false,
                            None,
                        ) {
                            if *night {
                                new_state = new_state & !(1 << i as u8);
                            } else {
                                new_state = new_state | (1 << i as u8);
                            }
                            rb.new_value = Some(new_state);
                            increment_relay_counter(transmitter, relay.id);
                        }
                    }
                }
            }
        }
        //save output state when needed
        rb.save_state().await;
    }

    for yeelight in &mut relay_devices.yeelight {
        if let Some(dev) = relays.relay.iter_mut().find(|y| y.id == yeelight.id) {
            let relay_marked = dev.tags.iter().any(|tag| tag == "all_night");
            if relay_marked {
                if dev.turn_on_prolong(
                    ProlongKind::DayNight,
                    *night,
                    yeelight.get_dest_name(None),
                    *night,
                    !yeelight.powered_on,
                    None,
                ) {
                    yeelight.turn_on_off(*night, &dev);
                    dev.last_toggled = Some(Instant::now());
                    increment_yeelight_counter(transmitter, yeelight.id);
                }
            }
        }
    }
}

//groups pending tasks once (by relay/yeelight id and by tag group) instead of
//cloning + filtering the whole task list again for every single device
async fn process_pending_tasks(
    relay_devices: &mut RelayDevices,
    relays: &mut Relays,
    transmitter: &Sender<DbTask>,
    pending_tasks: &mut Vec<OneWireTask>,
    night: bool,
) {
    if pending_tasks.is_empty() {
        return;
    }

    let mut by_id: HashMap<i32, Vec<OneWireTask>> = HashMap::new();
    let mut by_tag: HashMap<String, Vec<OneWireTask>> = HashMap::new();
    for t in pending_tasks.drain(..) {
        if let Some(id) = t.id_relay.or(t.id_yeelight) {
            by_id.entry(id).or_default().push(t);
        } else if let Some(tag) = t.tag_group.clone() {
            by_tag.entry(tag).or_default().push(t);
        }
    }

    let tasks_for = |id: i32, tags: &Vec<String>| -> Vec<&OneWireTask> {
        let mut result: Vec<&OneWireTask> = by_id
            .get(&id)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        for tag in tags {
            if let Some(v) = by_tag.get(tag) {
                result.extend(v.iter());
            }
        }
        result
    };

    //Yeelights
    for yeelight in &mut relay_devices.yeelight {
        if let Some(dev) = relays.relay.iter_mut().find(|y| y.id == yeelight.id) {
            for t in tasks_for(dev.id, &dev.tags) {
                debug!(
                    "Processing OneWireTask: command={:?}, matched id_yeelight={}, duration={:?}",
                    t.command, dev.id, t.duration
                );
                match t.command {
                    TaskCommand::TurnOnProlong => {
                        if dev.turn_on_prolong(
                            ProlongKind::Remote,
                            night,
                            yeelight.get_dest_name(None),
                            true,
                            !yeelight.powered_on,
                            t.duration,
                        ) {
                            yeelight.turn_on_off(true, &dev);
                            dev.last_toggled = Some(Instant::now());
                            increment_yeelight_counter(transmitter, dev.id);
                        }
                    }
                    TaskCommand::TurnOff => {
                        if dev.turn_on_prolong(
                            ProlongKind::Remote,
                            night,
                            yeelight.get_dest_name(None),
                            false,
                            !yeelight.powered_on,
                            t.duration,
                        ) {
                            yeelight.turn_on_off(false, &dev);
                            dev.last_toggled = Some(Instant::now());
                            increment_yeelight_counter(transmitter, dev.id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    //Relays
    for rb in &mut relay_devices.relay_boards {
        let mut new_state: u8 = rb.get_actual_state();
        for i in 0..=7 {
            if let Some(id) = rb.relay[i] {
                if let Some(relay) = relays.relay.iter_mut().find(|r| r.id == id) {
                    for t in tasks_for(relay.id, &relay.tags) {
                        debug!(
                            "Processing OneWireTask: command={:?}, matched id_relay={}, duration={:?}",
                            t.command, relay.id, t.duration
                        );
                        let currently_off = new_state & (1 << i as u8) != 0;
                        match t.command {
                            TaskCommand::TurnOnProlong => {
                                if relay.turn_on_prolong(
                                    ProlongKind::Remote,
                                    night,
                                    rb.get_dest_name(Some(i)),
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
                                    rb.get_dest_name(Some(i)),
                                    false,
                                    currently_off,
                                    t.duration,
                                ) {
                                    new_state = new_state | (1 << i as u8);
                                    rb.new_value = Some(new_state);
                                    increment_relay_counter(transmitter, relay.id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        //save output state when needed
        rb.save_state().await;
    }
}

async fn check_auto_off(
    relay_devices: &mut RelayDevices,
    relays: &mut Relays,
    transmitter: &Sender<DbTask>,
    night: bool,
) {
    //auto turn-off of relays
    for rb in &mut relay_devices.relay_boards {
        let mut new_state: u8 = rb.get_actual_state();
        for i in 0..=7 {
            if let Some(id) = rb.relay[i] {
                if let Some(relay) = relays.relay.iter_mut().find(|r| r.id == id) {
                    if let (Some(toggled), Some(stop_after)) =
                        (relay.last_toggled, relay.stop_after)
                    {
                        if toggled.elapsed() > stop_after {
                            let currently_off = new_state & (1 << i as u8) != 0;
                            if relay.turn_on_prolong(
                                ProlongKind::AutoOff,
                                night,
                                rb.get_dest_name(Some(i)),
                                false,
                                currently_off,
                                None,
                            ) {
                                new_state = new_state | (1 << i as u8);
                                rb.new_value = Some(new_state);
                                increment_relay_counter(transmitter, relay.id);
                            }
                        }
                    }
                }
            }
        }
        //save output state when needed
        rb.save_state().await;
    }

    //auto turn-off of yeelights
    for yeelight in &mut relay_devices.yeelight {
        if let Some(dev) = relays.relay.iter_mut().find(|y| y.id == yeelight.id) {
            if let (Some(toggled), Some(stop_after)) = (dev.last_toggled, dev.stop_after) {
                if toggled.elapsed() > stop_after {
                    if dev.turn_on_prolong(
                        ProlongKind::AutoOff,
                        night,
                        yeelight.get_dest_name(None),
                        false,
                        !yeelight.powered_on,
                        None,
                    ) {
                        yeelight.turn_on_off(false, &dev);
                        dev.last_toggled = Some(Instant::now());
                        increment_yeelight_counter(transmitter, yeelight.id);
                    }
                }
            }
        }
    }
}

impl OneWire {
    //Runs as a normal async task (spawned into the same tokio JoinSet as the
    //other workers, no dedicated std::thread anymore). The actual blocking
    //sysfs I/O lives entirely in a single long-lived sensor_poller_thread,
    //spawned once below via spawn_blocking for the whole lifetime of this
    //task (not per-iteration): it owns its own private SensorBoard list and
    //streams detected changes back over a channel. This coordinator loop
    //only ever reads sensor_devices for metadata and never touches a sysfs
    //file itself, so it can never be stalled by slow w1 bus I/O, and nothing
    //here needs a lock since sensor_devices/relay_devices/relays are owned
    //solely by this loop.
    pub async fn worker(
        mut self,
        worker_cancel_flag: Arc<AtomicBool>,
        ethlcd: Option<EthLcd>,
        rfid_pending_tags: Arc<RwLock<Vec<u32>>>,
    ) -> std::result::Result<(), WorkerError> {
        info!("{}: Starting task", self.name);

        //one dedicated polling thread per physical w1 bus, spawned on
        //demand by route_boards_to_pollers() the first time a reload shows
        //a device on that bus; all of them report detected changes onto
        //this single shared channel
        let mut bus_pollers: HashMap<u32, Sender<Vec<(u8, u64)>>> = HashMap::new();
        let (sensor_change_tx, sensor_change_rx): (
            Sender<SensorBoardChange>,
            Receiver<SensorBoardChange>,
        ) = flume::unbounded();

        //if there's already device data loaded (shouldn't normally happen
        //this early, but keeps the pollers in sync in all cases), route it
        if !self.sensor_devices.sensor_boards.is_empty() {
            let boards: Vec<(u8, u64)> = self
                .sensor_devices
                .sensor_boards
                .iter()
                .map(|b| (b.ow_family, b.ow_address))
                .collect();
            route_boards_to_pollers(
                boards,
                &mut bus_pollers,
                &sensor_change_tx,
                &worker_cancel_flag,
                &self.name,
            )
            .await;
        }

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
            rfid_tags: vec![],
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
        load_geolocation_config(&mut lat, &mut lon);
        if lat != 0.0 && lon != 0.0 {
            night_check = Some(Instant::now());
            info!(
                "{}: 🌎 calculating sun position for lat: {}, long: {}",
                self.name, lat, lon
            );
        }

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            //apply any device reload(s) sent by the database task (this also
            //routes the fresh board address list to the per-bus poller
            //threads, spawning new ones on demand)
            while let Ok(data) = self.reload_receiver.try_recv() {
                let poller_boards = apply_reload(
                    &mut self.sensor_devices,
                    &mut self.relay_devices,
                    &mut self.relays,
                    &mut state_machine,
                    data,
                    &self.name,
                );
                route_boards_to_pollers(
                    poller_boards,
                    &mut bus_pollers,
                    &sensor_change_tx,
                    &worker_cancel_flag,
                    &self.name,
                )
                .await;
            }
            if state_machine.cesspool_level.level.len() < self.sensor_devices.max_cesspool_level {
                state_machine
                    .cesspool_level
                    .level
                    .resize(self.sensor_devices.max_cesspool_level, None);
            }

            //answer any pending status-page queries from the webserver task
            //with a fresh snapshot; if the requester (an HTTP handler) timed
            //out and dropped its receiver already, the send below just fails
            //silently -- nothing to clean up on our side
            while let Ok(query) = self.status_receiver.try_recv() {
                let status = collect_device_status(&self.relay_devices, &self.relays);
                let _ = query.reply_tx.send(status);
            }

            //drain all currently queued external tasks (garage beep / night-prolong
            //conversion handled right here, same as before)
            while let Ok(mut t) = self.ow_receiver.try_recv() {
                debug!(
                    "Received OneWireTask: id_relay: {:?}, tag_group: {:?}, duration: {:?}",
                    t.id_relay, t.tag_group, t.duration
                );
                match t.command {
                    TaskCommand::TurnOnProlongNight => {
                        if night {
                            t.command = TaskCommand::TurnOnProlong;
                            pending_tasks.push(t);
                        }
                    }
                    _ => {
                        pending_tasks.push(t);
                    }
                }
            }

            //wait for the next sensor change (this also paces the loop,
            //replacing the old blocking-read-driven pacing); a 200ms cap
            //keeps the periodic checks below (day/night, auto-off, rfid)
            //ticking even when nothing changed on the bus
            match tokio::time::timeout(Duration::from_millis(200), sensor_change_rx.recv_async())
                .await
            {
                Ok(Ok(change)) => {
                    apply_sensor_change(
                        &self.sensor_devices,
                        &mut self.relay_devices,
                        &mut self.relays,
                        &mut state_machine,
                        &self.transmitter,
                        &mut pending_tasks,
                        night,
                        &self.name,
                        change,
                    )
                    .await;
                    //drain anything else that piled up in the meantime so a
                    //burst of changes is applied together, same as a single
                    //poll pass used to do
                    while let Ok(change) = sensor_change_rx.try_recv() {
                        apply_sensor_change(
                            &self.sensor_devices,
                            &mut self.relay_devices,
                            &mut self.relays,
                            &mut state_machine,
                            &self.transmitter,
                            &mut pending_tasks,
                            night,
                            &self.name,
                            change,
                        )
                        .await;
                    }
                }
                Ok(Err(_)) => {
                    //poller thread is gone (panicked?) -- log once in a
                    //while so this doesn't spam, but keep the rest of the
                    //coordinator (relays/tasks/rfid/etc.) running
                    error!(
                        "{}: sensor poller channel disconnected, no more sensor readings will arrive",
                        self.name
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(_) => {
                    //no sensor change within the timeout -- fall through to
                    //the periodic checks below
                }
            }

            //checking day/night
            if night_check.is_some()
                && night_check.unwrap().elapsed()
                    > Duration::from_secs_f32(SUN_POS_CHECK_INTERVAL_SECS)
            {
                night_check = Some(Instant::now());
                check_day_night(
                    &mut self.relay_devices,
                    &mut self.relays,
                    &mut state_machine,
                    &self.transmitter,
                    lat,
                    lon,
                    &mut night,
                    &self.name,
                )
                .await;
            }

            //process rfid pending tags, if any
            state_machine.process_rfid_tags(&mut pending_tasks, night);

            //checking for pending tasks
            process_pending_tasks(
                &mut self.relay_devices,
                &mut self.relays,
                &self.transmitter,
                &mut pending_tasks,
                night,
            )
            .await;

            //checking for auto turn-off of necessary relays/yeelights
            check_auto_off(
                &mut self.relay_devices,
                &mut self.relays,
                &self.transmitter,
                night,
            )
            .await;
        }
        info!("{}: task stopped", self.name);
        Ok(())
    }
}
