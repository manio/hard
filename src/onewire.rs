use crate::database::{CommandCode, DbTask};
use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::ops::Add;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

//family codes for devices
pub const FAMILY_CODE_DS2413: u8 = 0x3a;
pub const FAMILY_CODE_DS2408: u8 = 0x29;

pub const DS2408_INITIAL_STATE: u8 = 0xff;

//timing constants
pub const DEFAULT_PIR_HOLD_SECS: f32 = 120.0; //2min for PIR sensors
pub const DEFAULT_SWITCH_HOLD_SECS: f32 = 3600.0; //1hour for wall-switches
pub const DEFAULT_PIR_PROLONG_SECS: f32 = 900.0; //15min prolonging in override_mode
pub const MIN_TOGGLE_DELAY_SECS: f32 = 1.0; //1sec flip-flop protection: minimum delay between toggles

static W1_ROOT_PATH: &str = "/sys/bus/w1/devices";

//yeelight consts
pub const YEELIGHT_TCP_PORT: u16 = 55443;
static YEELIGHT_METHOD_SET_POWER: &str = "set_power"; //method value name for powering on/off
static YEELIGHT_EFFECT: &str = "smooth"; //default effect for turning on/off
pub const YEELIGHT_DURATION_MS: u32 = 500; //duration of above effect

fn get_w1_device_name(family_code: u8, address: u64) -> String {
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
                    }
                    Err(e) => {
                        error!(
                            "{}: error reading: {:?}",
                            get_w1_device_name(self.ow_family, self.ow_address),
                            e,
                        );
                    }
                }
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
    pub tags: Vec<String>,
    pub pir_exclude: bool,
    pub pir_hold_secs: f32,
    pub switch_hold_secs: f32,
    pub pir_all_day: bool,
    pub override_mode: bool,
    pub last_toggled: Option<Instant>,
    pub stop_after: Option<Duration>,
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
                        "{}: saving output byte: {:#04x}",
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
}

pub struct Yeelight {
    pub id_yeelight: i32,
    pub name: String,
    pub tags: Vec<String>,
    pub ip_address: String,
    pub pir_exclude: bool,
    pub pir_hold_secs: f32,
    pub switch_hold_secs: f32,
    pub pir_all_day: bool,
    pub override_mode: bool,
    pub last_toggled: Option<Instant>,
    pub stop_after: Option<Duration>,
    pub powered_on: bool,
}

#[derive(Serialize)]
struct YeelightCommand {
    id: u32,
    method: String,
    #[serde(serialize_with = "Yeelight::params_serialize")]
    params: Vec<String>,
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
        let cmd = YeelightCommand {
            id: 1,
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
        debug!("Yeelight: {}: connecting...", yeelight_name);
        match TcpStream::connect(format!("{}:{}", ip_addr, YEELIGHT_TCP_PORT)) {
            Err(e) => {
                error!("Yeelight: {}: connection error: {:?}", yeelight_name, e);
            }
            Ok(mut stream) => {
                debug!("Yeelight: {}: connected, sending command", yeelight_name);
                json_cmd.push_str("\r\n"); //specs requirement
                match stream.write_all(json_cmd.as_bytes()) {
                    Err(e) => {
                        error!(
                            "Yeelight: {}: cannot write to socket: {:?}",
                            yeelight_name, e
                        );
                    }
                    Ok(_) => (),
                }
            }
        }
    }

    fn turn_on_off(&mut self, turn_on: bool) {
        let yeelight_name = self.name.clone();
        let ip_address = self.ip_address.clone();
        thread::spawn(move || Yeelight::yeelight_tcp_command(yeelight_name, ip_address, turn_on));

        self.powered_on = turn_on;
        self.last_toggled = Some(Instant::now());
    }
}

pub struct SensorDevices {
    pub kinds: HashMap<i32, String>,
    pub sensor_boards: Vec<SensorBoard>,
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

        //create and attach a relay
        let relay = Relay {
            id_relay,
            name,
            tags,
            pir_exclude,
            pir_hold_secs: pir_hold_secs.unwrap_or(DEFAULT_PIR_HOLD_SECS),
            switch_hold_secs: switch_hold_secs.unwrap_or(DEFAULT_SWITCH_HOLD_SECS),
            pir_all_day,
            override_mode: initial_state,
            last_toggled: None,
            stop_after: None,
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
        let light = Yeelight {
            id_yeelight,
            name,
            tags,
            ip_address,
            pir_exclude,
            pir_hold_secs: pir_hold_secs.unwrap_or(DEFAULT_PIR_HOLD_SECS),
            switch_hold_secs: switch_hold_secs.unwrap_or(DEFAULT_SWITCH_HOLD_SECS),
            pir_all_day,
            override_mode: false,
            last_toggled: None,
            stop_after: None,
            powered_on: false,
        };
        self.yeelight.push(light);
    }
}

pub struct StateMachine {
    pub name: String,
    pub bedroom_mode: bool,
}

impl StateMachine {
    /* all below hook functions are returning bool, which means:
    true - continue processing
    false - stop processing the event (don't turn the relays, etc) */

    fn sensor_hook(
        &mut self,
        sensor_kind_code: &str,
        sensor_on: bool,
        sensor_tags: &Vec<String>,
        night: bool,
    ) -> bool {
        if sensor_kind_code == "PIR_Trigger" && sensor_on && night {
            for tag in sensor_tags {
                match tag.as_ref() {
                    "bedroom_enable" => {
                        return if !self.bedroom_mode {
                            info!("{}: bedroom mode enabled", self.name);
                            self.bedroom_mode = true;
                            true //allow single turn-on
                        } else {
                            false
                        };
                    }
                    "bedroom_disable" => {
                        if self.bedroom_mode {
                            info!("{}: bedroom mode disabled", self.name);
                            self.bedroom_mode = false;
                        }
                    }
                    _ => {}
                }
            }
        }
        true
    }

    fn relay_hook(
        &mut self,
        sensor_kind_code: &str,
        sensor_on: bool,
        relay_tags: &Vec<String>,
        night: bool,
        flipflop_block: bool,
    ) -> bool {
        if sensor_kind_code == "PIR_Trigger" && sensor_on && night {
            for tag in relay_tags {
                match tag.as_ref() {
                    "night_exclude" => {
                        return false;
                    }
                    _ => {}
                }
            }
        }
        true
    }

    fn yeelight_hook(
        &mut self,
        sensor_kind_code: &str,
        sensor_on: bool,
        yeelight_tags: &Vec<String>,
        night: bool,
        flipflop_block: bool,
    ) -> bool {
        true
    }
}

pub struct OneWire {
    pub name: String,
    pub transmitter: Sender<DbTask>,
    pub sensor_devices: Arc<RwLock<SensorDevices>>,
    pub relay_devices: Arc<RwLock<RelayDevices>>,
}

impl OneWire {
    fn increment_relay_counter(&self, id_relay: i32) {
        let task = DbTask {
            command: CommandCode::IncrementRelayCounter,
            value: Some(id_relay),
        };
        self.transmitter.send(task).unwrap();
    }

    fn increment_yeelight_counter(&self, id_yeelight: i32) {
        let task = DbTask {
            command: CommandCode::IncrementYeelightCounter,
            value: Some(id_yeelight),
        };
        self.transmitter.send(task).unwrap();
    }

    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
        let mut state_machine = StateMachine {
            name: "statemachine".to_owned(),
            bedroom_mode: false,
        };

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            debug!("doing stuff");
            {
                let mut night = false;
                let mut sensor_dev = self.sensor_devices.write().unwrap();
                let mut relay_dev = self.relay_devices.write().unwrap();

                //fixme: do we really need to clone this HashMap to use it below?
                let kinds_cloned = sensor_dev.kinds.clone();

                for sb in &mut sensor_dev.sensor_boards {
                    match sb.read_state() {
                        //we have new state to process
                        Some(new_value) => {
                            match sb.last_value {
                                Some(last_value) => {
                                    let bits = vec![0, 2];
                                    let names = &["PIOA", "PIOB"];

                                    for bit in bits {
                                        //check for bit change
                                        if new_value & (1 << bit) != last_value & (1 << bit) {
                                            let mut pio_name: &str = &"".to_string();
                                            let mut sensor: &Option<Sensor> = &None;
                                            if bit == 0 {
                                                sensor = &sb.pio_a;
                                                pio_name = names[0];
                                            } else if bit == 2 {
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
                                                    self.transmitter.send(task).unwrap();

                                                    let kind_code =
                                                        kinds_cloned.get(&sensor.id_kind).unwrap();
                                                    let on: bool = new_value & (1 << bit) != 0;

                                                    //check hook function result and stop processing when needed
                                                    let stop_processing = !state_machine
                                                        .sensor_hook(
                                                            &kind_code,
                                                            on,
                                                            &sensor.tags,
                                                            night,
                                                        );
                                                    info!(
                                                        "{}: [{} {} {}]: {:#04x} on: {}, stop_processing: {}",
                                                        kind_code,
                                                        get_w1_device_name(
                                                            sb.ow_family,
                                                            sb.ow_address
                                                        ),
                                                        pio_name,
                                                        sensor.name,
                                                        new_value,
                                                        on,
                                                        stop_processing
                                                    );
                                                    if stop_processing {
                                                        continue;
                                                    }

                                                    //trigger actions for relays
                                                    let associated_relays =
                                                        &sensor.associated_relays;
                                                    if !associated_relays.is_empty() {
                                                        for rb in &mut relay_dev.relay_boards {
                                                            for i in 0..=7 {
                                                                match &mut rb.relay[i] {
                                                                    Some(relay) => {
                                                                        if associated_relays
                                                                            .contains(
                                                                                &relay.id_relay,
                                                                            )
                                                                        {
                                                                            //flip-flop protection for too fast state changes
                                                                            let mut flipflop_block =
                                                                                false;
                                                                            match relay.last_toggled {
                                                                                Some(toggled) => {
                                                                                    if toggled.elapsed() < Duration::from_secs_f32(MIN_TOGGLE_DELAY_SECS) {
                                                                                        flipflop_block = true;
                                                                                    }
                                                                                }
                                                                                _ => {}
                                                                            }

                                                                            //check hook function result and stop processing when needed
                                                                            let stop_processing =
                                                                                !state_machine
                                                                                    .relay_hook(
                                                                                    &kind_code,
                                                                                    on,
                                                                                    &relay.tags,
                                                                                    night,
                                                                                    flipflop_block,
                                                                                );
                                                                            if stop_processing {
                                                                                debug!(
                                                                                    "{}: {}: stopped processing",
                                                                                    get_w1_device_name(
                                                                                        rb.ow_family,
                                                                                        rb.ow_address
                                                                                    ),
                                                                                    relay.name,
                                                                                );
                                                                                continue;
                                                                            }

                                                                            //we will be computing new output byte for a relay board
                                                                            //so first of all get the base/previous value
                                                                            let mut new_state: u8 = match rb.new_value {
                                                                                Some(val) => val,
                                                                                None => rb.last_value.unwrap_or(DS2408_INITIAL_STATE)
                                                                            };

                                                                            match kind_code.as_ref()
                                                                            {
                                                                                "PIR_Trigger" => {
                                                                                    if !relay
                                                                                        .pir_exclude
                                                                                        && on && (night || relay.pir_all_day)
                                                                                    {
                                                                                        //checking if bit is set (relay is off)
                                                                                        if !relay.override_mode && new_state & (1 << i as u8) != 0 {
                                                                                            if flipflop_block {
                                                                                                warn!(
                                                                                                    "{}: {}: flip-flop protection: PIR turn-on request ignored",
                                                                                                    get_w1_device_name(
                                                                                                        rb.ow_family,
                                                                                                        rb.ow_address
                                                                                                    ),
                                                                                                    relay.name,
                                                                                                );
                                                                                            } else {
                                                                                                new_state = new_state & !(1 << i as u8);
                                                                                                info!(
                                                                                                    "{}: Turning ON: {}: bit={} new state: {:#04x}",
                                                                                                    get_w1_device_name(
                                                                                                        rb.ow_family,
                                                                                                        rb.ow_address
                                                                                                    ),
                                                                                                    relay.name,
                                                                                                    i,
                                                                                                    new_state,
                                                                                                );
                                                                                                relay.stop_after = Some(Duration::from_secs_f32(relay.pir_hold_secs));
                                                                                                rb.new_value = Some(new_state);
                                                                                            }
                                                                                        } else {
                                                                                            info!(
                                                                                                "{}: Prolonging: {}: bit={}",
                                                                                                get_w1_device_name(
                                                                                                    rb.ow_family,
                                                                                                    rb.ow_address
                                                                                                ),
                                                                                                relay.name,
                                                                                                i,
                                                                                            );

                                                                                            let toggled_elapsed = relay.last_toggled.unwrap_or(Instant::now()).elapsed();
                                                                                            if relay.override_mode {
                                                                                                if toggled_elapsed > Duration::from_secs_f32(relay.switch_hold_secs - DEFAULT_PIR_PROLONG_SECS) {
                                                                                                    relay.stop_after = Some(toggled_elapsed.add(Duration::from_secs_f32(DEFAULT_PIR_PROLONG_SECS)));
                                                                                                }
                                                                                            } else {
                                                                                                relay.stop_after = Some(toggled_elapsed.add(Duration::from_secs_f32(relay.pir_hold_secs)));
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                                "Switch" => {
                                                                                    if flipflop_block {
                                                                                        warn!(
                                                                                            "{}: {}: flip-flop protection: Switch toggle request ignored",
                                                                                            get_w1_device_name(
                                                                                                rb.ow_family,
                                                                                                rb.ow_address
                                                                                            ),
                                                                                            relay.name,
                                                                                        );
                                                                                    } else {
                                                                                        //switching is toggling current state to the opposite:
                                                                                        new_state = new_state ^ (1 << i as u8);
                                                                                        info!(
                                                                                            "{}: Switch toggle: {}: bit={} new state: {:#04x}",
                                                                                            get_w1_device_name(
                                                                                                rb.ow_family,
                                                                                                rb.ow_address
                                                                                            ),
                                                                                            relay.name,
                                                                                            i,
                                                                                            new_state,
                                                                                        );
                                                                                        relay.override_mode = true;
                                                                                        relay.stop_after = Some(Duration::from_secs_f32(relay.switch_hold_secs));
                                                                                        rb.new_value = Some(new_state);
                                                                                    }
                                                                                }
                                                                                _ => {
                                                                                    error!(
                                                                                        "{}: {}/{}: unhandled kind: {:?}",
                                                                                        get_w1_device_name(
                                                                                            sb.ow_family,
                                                                                            sb.ow_address
                                                                                        ),
                                                                                        pio_name,
                                                                                        sensor.name,
                                                                                        kind_code,
                                                                                    );
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                    _ => {}
                                                                }
                                                            }
                                                        }
                                                    }

                                                    //trigger actions for yeelights
                                                    let associated_yeelights =
                                                        &sensor.associated_yeelights;
                                                    if !associated_yeelights.is_empty() {
                                                        for yeelight in &mut relay_dev.yeelight {
                                                            if associated_yeelights
                                                                .contains(&yeelight.id_yeelight)
                                                            {
                                                                //flip-flop protection for too fast state changes
                                                                let mut flipflop_block = false;
                                                                match yeelight.last_toggled {
                                                                    Some(toggled) => {
                                                                        if toggled.elapsed() < Duration::from_secs_f32(MIN_TOGGLE_DELAY_SECS) {
                                                                            flipflop_block = true;
                                                                        }
                                                                    }
                                                                    _ => {}
                                                                }

                                                                //check hook function result and stop processing when needed
                                                                let stop_processing =
                                                                    !state_machine.yeelight_hook(
                                                                        &kind_code,
                                                                        on,
                                                                        &yeelight.tags,
                                                                        night,
                                                                        flipflop_block,
                                                                    );
                                                                if stop_processing {
                                                                    debug!(
                                                                        "Yeelight: {}: stopped processing",
                                                                        yeelight.name,
                                                                    );
                                                                    continue;
                                                                }

                                                                match kind_code.as_ref() {
                                                                    "PIR_Trigger" => {
                                                                        if !yeelight.pir_exclude
                                                                            && on
                                                                            && (night
                                                                                || yeelight
                                                                                    .pir_all_day)
                                                                        {
                                                                            //checking if yeelight is off
                                                                            if !yeelight
                                                                                .override_mode
                                                                                && !yeelight
                                                                                    .powered_on
                                                                            {
                                                                                if flipflop_block {
                                                                                    warn!(
                                                                                        "Yeelight: {}: flip-flop protection: PIR turn-on request ignored",
                                                                                        yeelight.name,
                                                                                    );
                                                                                } else {
                                                                                    info!(
                                                                                        "Yeelight: Turning ON: {}",
                                                                                        yeelight.name,
                                                                                    );
                                                                                    yeelight.stop_after = Some(Duration::from_secs_f32(yeelight.pir_hold_secs));
                                                                                    yeelight.turn_on_off(true);
                                                                                    self.increment_yeelight_counter(yeelight.id_yeelight);
                                                                                }
                                                                            } else {
                                                                                info!(
                                                                                    "Yeelight: Prolonging: {}",
                                                                                    yeelight.name,
                                                                                );

                                                                                let toggled_elapsed = yeelight.last_toggled.unwrap_or(Instant::now()).elapsed();
                                                                                if yeelight
                                                                                    .override_mode
                                                                                {
                                                                                    if toggled_elapsed > Duration::from_secs_f32(yeelight.switch_hold_secs - DEFAULT_PIR_PROLONG_SECS) {
                                                                                        yeelight.stop_after = Some(toggled_elapsed.add(Duration::from_secs_f32(DEFAULT_PIR_PROLONG_SECS)));
                                                                                    }
                                                                                } else {
                                                                                    yeelight.stop_after = Some(toggled_elapsed.add(Duration::from_secs_f32(yeelight.pir_hold_secs)));
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                    "Switch" => {
                                                                        if flipflop_block {
                                                                            warn!(
                                                                                "Yeelight: {}: flip-flop protection: Switch toggle request ignored",
                                                                                yeelight.name,
                                                                            );
                                                                        } else {
                                                                            //switching is toggling current state to the opposite:
                                                                            info!(
                                                                                "Yeelight: Switch toggle: {}",
                                                                                yeelight.name,
                                                                            );
                                                                            yeelight
                                                                                .override_mode =
                                                                                true;
                                                                            yeelight.stop_after = Some(Duration::from_secs_f32(yeelight.switch_hold_secs));
                                                                            yeelight.turn_on_off(
                                                                                !yeelight
                                                                                    .powered_on,
                                                                            );
                                                                            self.increment_yeelight_counter(yeelight.id_yeelight);
                                                                        }
                                                                    }
                                                                    _ => {
                                                                        error!(
                                                                            "Yeelight: {}/{}: unhandled kind: {:?}",
                                                                            pio_name,
                                                                            sensor.name,
                                                                            kind_code,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        }
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
                                                let old_value =
                                                    rb.last_value.unwrap_or(DS2408_INITIAL_STATE);
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
                                                                        relay.id_relay,
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
                                _ => {}
                            }
                            //processed -> save new value as the previous one:
                            sb.last_value = Some(new_value);
                        }
                        None => (),
                    }
                    thread::sleep(Duration::from_micros(500));
                }

                //checking for auto turn-off of necessary relays
                for rb in &mut relay_dev.relay_boards {
                    //we will be eventually computing new output byte for a relay board
                    //so first of all get the base/previous value
                    let mut new_state: u8 = match rb.new_value {
                        Some(val) => val,
                        None => rb.last_value.unwrap_or(DS2408_INITIAL_STATE),
                    };

                    //iteration on all relays and check elapsed time
                    for i in 0..=7 {
                        match &mut rb.relay[i] {
                            Some(relay) => {
                                match relay.last_toggled {
                                    Some(toggled) => {
                                        match relay.stop_after {
                                            Some(stop_after) => {
                                                if toggled.elapsed()
                                                    > Duration::from_secs_f32(MIN_TOGGLE_DELAY_SECS)
                                                    && toggled.elapsed() > stop_after
                                                {
                                                    let on: bool = new_state & (1 << i as u8) == 0;
                                                    if on {
                                                        //set a bit -> turn off relay
                                                        new_state = new_state | (1 << i as u8);
                                                        info!(
                                                            "{}: Auto turn-off: {}: bit={} new state: {:#04x}",
                                                            get_w1_device_name(
                                                                rb.ow_family,
                                                                rb.ow_address
                                                            ),
                                                            relay.name,
                                                            i,
                                                            new_state,
                                                        );
                                                        relay.last_toggled = Some(Instant::now());
                                                        rb.new_value = Some(new_state);
                                                        self.increment_relay_counter(
                                                            relay.id_relay,
                                                        );
                                                    } else {
                                                        if relay.override_mode {
                                                            info!(
                                                                "{}: End of override mode: {}: bit={}",
                                                                get_w1_device_name(
                                                                    rb.ow_family,
                                                                    rb.ow_address
                                                                ),
                                                                relay.name,
                                                                i,
                                                            );
                                                        }
                                                        relay.last_toggled = None;
                                                    }
                                                    relay.stop_after = None;
                                                    relay.override_mode = false;
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
                    match yeelight.last_toggled {
                        Some(toggled) => match yeelight.stop_after {
                            Some(stop_after) => {
                                if toggled.elapsed()
                                    > Duration::from_secs_f32(MIN_TOGGLE_DELAY_SECS)
                                    && toggled.elapsed() > stop_after
                                {
                                    if yeelight.powered_on {
                                        info!("Yeelight: Auto turn-off: {}", yeelight.name,);
                                        yeelight.turn_on_off(false);
                                        self.increment_yeelight_counter(yeelight.id_yeelight);
                                    } else {
                                        if yeelight.override_mode {
                                            info!(
                                                "Yeelight: End of override mode: {}",
                                                yeelight.name,
                                            );
                                        }
                                        yeelight.last_toggled = None;
                                    }
                                    yeelight.stop_after = None;
                                    yeelight.override_mode = false;
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
        info!("{}: Stopping thread", self.name);
    }
}
