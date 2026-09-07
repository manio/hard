// deye.rs
//
// Deye hybrid three-phase inverter (SUN-x-SG04LP3 / SG01HP3 family) Modbus RTU/TCP client.
//
// Modeled after sun2000.rs: same overall shape (Parameter table, block-grouped reads,
// a worker() polling loop), adapted to the Deye register map.
//
// Register map ported from https://github.com/Lewa-Reka/esphome-deye-inverter
// (packages/deye_hybrid_3p/*.yaml), which the project itself derived from the
// Deye/Sofar MODBUS RTU protocol documents. Cross-checked against
// https://github.com/davidrapan/ha-solarman (deye_p3.yaml) and
// https://github.com/kbialek/deye-inverter-mqtt where register names overlapped.
//
// All WRITES (settings/time/parameters) are gated behind `DeyeConfig::enable_write`,
// which defaults to `false`. With writes disabled this module is read-only,
// same as sun2000.rs.
//
// A few registers in the source project are ambiguous or possibly conflicting
// (noted inline as `// aka: ...` comments on the parameter table below) - worth
// double-checking against the MODBUS RTU V105 doc before relying on them for writes.

use crate::database::DeyeDailyYield;
use flume::Sender;
use influxdb::{Client, InfluxDbWriteable, Timestamp, Type};
use io::ErrorKind;
use simplelog::*;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::timeout;
use tokio_modbus::client::Context;
use tokio_modbus::prelude::*;

pub const DEYE_POLL_INTERVAL_SECS: f32 = 5.0;
pub const DEYE_STATS_DUMP_INTERVAL_SECS: f32 = 3600.0;
pub const DEYE_ATTEMPTS_PER_PARAM: u8 = 7;
pub const DEYE_MAX_REGS_PER_BLOCK: u16 = 64;
/// Names of the daily energy counters pushed to Postgres each stats-dump
/// interval via a dedicated Sender<DeyeDailyYield> channel (see
/// DeyeConfig::deye_yield_transmitter) into the deye_daily_energy
/// table/function - independent from sun2000's DbTask/CommandCode.
pub const DEYE_YIELD_PV: &str = "Daily PV Production";
pub const DEYE_YIELD_PV_TOTAL: &str = "Total PV Production";
pub const DEYE_YIELD_BATTERY_CHARGE: &str = "Daily Battery Charge";
pub const DEYE_YIELD_BATTERY_DISCHARGE: &str = "Daily Battery Discharge";
pub const DEYE_YIELD_GRID_BOUGHT: &str = "Daily Energy Bought";
pub const DEYE_YIELD_GRID_SOLD: &str = "Daily Energy Sold";
pub const DEYE_YIELD_LOAD: &str = "Daily Load Consumption";
pub const DEYE_YIELD_GENERATOR: &str = "Daily Generator Production";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Functional grouping, mirrors the esphome project's package split
/// (device / pv / battery / grid / load / inverter / generator / ups / tou / work_mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Device,
    Pv,
    Battery,
    Grid,
    Load,
    Inverter,
    Generator,
    Ups,
    Tou,
    WorkMode,
}

/// How to decode the raw u16 register(s) at `address`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RegKind {
    U16,
    I16,
    /// 32-bit value spread across 2 registers, low word first (Deye's "U_DWORD_R").
    U32Swapped,
    /// ASCII text spanning `len` registers, 2 chars/register, big-endian bytes.
    Text(u16),
    /// Boolean derived from `raw != 0`.
    Bool,
    /// Boolean derived from `(raw & mask) != 0`.
    Bitflag(u16),
}

#[derive(Clone)]
pub enum ParamValue {
    Text(Option<String>),
    U16(Option<u16>),
    I16(Option<i16>),
    U32(Option<u32>),
    Bool(Option<bool>),
}

impl fmt::Display for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParamValue::Text(v) => write!(f, "{}", v.clone().unwrap_or_default()),
            ParamValue::U16(v) => write!(f, "{}", v.map(|x| x.to_string()).unwrap_or_default()),
            ParamValue::I16(v) => write!(f, "{}", v.map(|x| x.to_string()).unwrap_or_default()),
            ParamValue::U32(v) => write!(f, "{}", v.map(|x| x.to_string()).unwrap_or_default()),
            ParamValue::Bool(v) => write!(f, "{}", v.map(|x| x.to_string()).unwrap_or_default()),
        }
    }
}

/// A single Modbus-backed parameter: where it lives, how to decode it, and
/// (for `writable` ones) the allowed range for a future write.
#[derive(Clone)]
pub struct Parameter {
    pub name: &'static str,
    pub category: Category,
    pub address: u16,
    pub len: u16,
    pub kind: RegKind,
    /// final_value = raw / gain (gain=1.0 => no scaling). Matches esphome's
    /// `multiply: X` filters, stored here as 1/X to keep sun2000-style semantics.
    pub gain: f32,
    pub unit: Option<&'static str>,
    /// true only for registers exposed as `number`/`select`/`switch` in the
    /// source esphome project, i.e. ones the inverter actually accepts writes to.
    pub writable: bool,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub value: ParamValue,
}

impl Parameter {
    pub const fn new(
        name: &'static str,
        category: Category,
        address: u16,
        len: u16,
        kind: RegKind,
        gain: f32,
        unit: Option<&'static str>,
        writable: bool,
        min: Option<f32>,
        max: Option<f32>,
    ) -> Self {
        let value = match kind {
            RegKind::Text(_) => ParamValue::Text(None),
            RegKind::I16 => ParamValue::I16(None),
            RegKind::U32Swapped => ParamValue::U32(None),
            RegKind::Bool | RegKind::Bitflag(_) => ParamValue::Bool(None),
            RegKind::U16 => ParamValue::U16(None),
        };
        Self {
            name,
            category,
            address,
            len,
            kind,
            gain,
            unit,
            writable,
            min,
            max,
            value,
        }
    }

    pub fn get_text_value(&self) -> String {
        match &self.value {
            ParamValue::Text(v) => v.clone().unwrap_or_default(),
            ParamValue::U16(v) => match v {
                Some(x) if self.gain != 1.0 => format!("{:.3}", *x as f32 / self.gain),
                Some(x) => x.to_string(),
                None => String::new(),
            },
            ParamValue::I16(v) => match v {
                Some(x) if self.gain != 1.0 => format!("{:.3}", *x as f32 / self.gain),
                Some(x) => x.to_string(),
                None => String::new(),
            },
            ParamValue::U32(v) => match v {
                Some(x) if self.gain != 1.0 => format!("{:.3}", *x as f32 / self.gain),
                Some(x) => x.to_string(),
                None => String::new(),
            },
            ParamValue::Bool(v) => v.map(|b| b.to_string()).unwrap_or_default(),
        }
    }

    /// Same mapping sun2000.rs uses: scaled values become Float, unscaled
    /// integers keep their native (Un)SignedInteger influxdb type.
    pub fn get_influx_value(&self) -> Option<influxdb::Type> {
        match &self.value {
            ParamValue::Text(v) => v.clone().map(Type::Text),
            ParamValue::U16(v) => v.map(|x| {
                if self.gain != 1.0 {
                    Type::Float(x as f64 / self.gain as f64)
                } else {
                    Type::UnsignedInteger(x as u64)
                }
            }),
            ParamValue::I16(v) => v.map(|x| {
                if self.gain != 1.0 {
                    Type::Float(x as f64 / self.gain as f64)
                } else {
                    Type::SignedInteger(x as i64)
                }
            }),
            ParamValue::U32(v) => v.map(|x| {
                if self.gain != 1.0 {
                    Type::Float(x as f64 / self.gain as f64)
                } else {
                    Type::UnsignedInteger(x as u64)
                }
            }),
            ParamValue::Bool(v) => v.map(|b| Type::Boolean(b)),
        }
    }
}

#[derive(Debug)]
pub struct DeyeConfig {
    pub name: String,
    pub host_port: String,
    pub dongle_connection: bool,
    /// Master switch for ALL write operations (settings/time/parameters).
    /// Defaults to `false`: with this off, `set_parameter()`/`write_time()`
    /// always return an error and no write ever reaches the inverter.
    pub enable_write: bool,
    /// InfluxDB base URL (e.g. "http://localhost:8086"). When set, every
    /// parameter is written to the "deye" influx database each poll cycle,
    /// same as sun2000.rs does for its own "sun2000" database.
    pub influxdb_url: Option<String>,
    /// Dedicated channel for daily energy counters (see database.rs'
    /// DeyeDailyYield / deye_daily_energy table) - deliberately NOT
    /// DbTask/db_transmitter, so nothing else in the codebase (sun2000.rs,
    /// onewire.rs, ...) needs to change.
    pub deye_yield_transmitter: Option<Sender<DeyeDailyYield>>,
}

impl Default for DeyeConfig {
    fn default() -> Self {
        Self {
            name: "deye".to_string(),
            host_port: "127.0.0.1:502".to_string(),
            dongle_connection: true,
            enable_write: false,
            influxdb_url: None,
            deye_yield_transmitter: None,
        }
    }
}

#[derive(Debug)]
pub struct Deye {
    pub config: DeyeConfig,
    pub poll_ok: u64,
    pub poll_errors: u64,
}

impl Deye {
    pub fn new(config: DeyeConfig) -> Self {
        Self {
            config,
            poll_ok: 0,
            poll_errors: 0,
        }
    }

    #[rustfmt::skip]
    pub fn param_table() -> Vec<Parameter> {
        vec![
        // ---- Device ----
        Parameter::new("Device Type", Category::Device, 0, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Modbus Address", Category::Device, 1, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Protocol Version", Category::Device, 2, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Serial Number", Category::Device, 3, 5, RegKind::Text(5), 1.0f32, None, false, None, None),
        Parameter::new("Device MCU Board Version", Category::Device, 10, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Control Board Firmware Raw 11", Category::Device, 11, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Arc Board Firmware Version", Category::Device, 12, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Slave MCU Version", Category::Device, 13, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Control Board Firmware Raw 14", Category::Device, 14, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Control Board Firmware Raw 15", Category::Device, 15, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Communication Board Firmware Raw 16", Category::Device, 16, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Communication Board Firmware Raw 17", Category::Device, 17, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Communication Board Firmware Raw 18", Category::Device, 18, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Device Rated Power", Category::Device, 20, 2, RegKind::U32Swapped, 10.0f32, Some("W"), false, None, None),
        Parameter::new("Device MPPTs / Phases (packed)", Category::Device, 22, 1, RegKind::U16, 1.0f32, None, false, None, None), // bits 8-11: MPPT count, bits 0-3: phase count
        Parameter::new("Device Self-check", Category::Device, 36, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Self Check Time", Category::Device, 61, 1, RegKind::U16, 1.0f32, Some("s"), true, Some(0f32), Some(1000f32)),
        Parameter::new("Inverter Date Time Raw 1", Category::Device, 62, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Inverter Date Time Raw 2", Category::Device, 63, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Inverter Date Time Raw 3", Category::Device, 64, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Power Switch", Category::Device, 80, 1, RegKind::Bool, 1.0f32, None, true, None, None),
        Parameter::new("Running Status", Category::Device, 500, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Comm Board Failure Status", Category::Device, 548, 1, RegKind::U16, 1.0f32, None, false, None, None), // bit0: flash chip error, bit1: RTC/time error, bit2: EEPROM error
        Parameter::new("Turn Off On Status", Category::Device, 551, 1, RegKind::Bitflag(1), 1.0f32, None, false, None, None),
        Parameter::new("Relay Status (raw)", Category::Device, 552, 1, RegKind::U16, 1.0f32, None, false, None, None), // bit0: inverter relay, bit1: load relay (reserved), bit2: grid relay, bit3: generator relay
        Parameter::new("Device Alarm 1", Category::Device, 553, 1, RegKind::U16, 1.0f32, None, false, None, None), // alarm bitfield word 1
        Parameter::new("Device Alarm 2", Category::Device, 554, 1, RegKind::U16, 1.0f32, None, false, None, None), // alarm bitfield word 2
        Parameter::new("Device Fault 1", Category::Device, 555, 1, RegKind::U16, 1.0f32, None, false, None, None), // fault bitfield word
        Parameter::new("Device Fault 2", Category::Device, 556, 1, RegKind::U16, 1.0f32, None, false, None, None), // fault bitfield word
        Parameter::new("Device Fault 3", Category::Device, 557, 1, RegKind::U16, 1.0f32, None, false, None, None), // fault bitfield word
        Parameter::new("Device Fault 4", Category::Device, 558, 1, RegKind::U16, 1.0f32, None, false, None, None), // fault bitfield word
        // ---- Pv ----
        Parameter::new("MPPT Multi-point Scanning", Category::Pv, 341, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Daily PV Production", Category::Pv, 529, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total PV Production", Category::Pv, 534, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("PV1 Power", Category::Pv, 672, 1, RegKind::U16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("PV2 Power", Category::Pv, 673, 1, RegKind::U16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("PV3 Power", Category::Pv, 674, 1, RegKind::U16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("PV4 Power", Category::Pv, 675, 1, RegKind::U16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("PV1 Voltage", Category::Pv, 676, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("PV1 Current", Category::Pv, 677, 1, RegKind::U16, 10.0f32, Some("A"), false, None, None),
        Parameter::new("PV2 Voltage", Category::Pv, 678, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("PV2 Current", Category::Pv, 679, 1, RegKind::U16, 10.0f32, Some("A"), false, None, None),
        Parameter::new("PV3 Voltage", Category::Pv, 680, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("PV3 Current", Category::Pv, 681, 1, RegKind::U16, 10.0f32, Some("A"), false, None, None),
        Parameter::new("PV4 Voltage", Category::Pv, 682, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("PV4 Current", Category::Pv, 683, 1, RegKind::U16, 10.0f32, Some("A"), false, None, None),
        // ---- Battery ----
        Parameter::new("Battery Control Mode", Category::Battery, 98, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Battery Equalization Voltage", Category::Battery, 99, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Absorption Voltage", Category::Battery, 100, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Float Voltage", Category::Battery, 101, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Capacity", Category::Battery, 102, 1, RegKind::U16, 1.0f32, Some("Ah"), true, Some(0f32), Some(2000f32)),
        Parameter::new("Battery Empty Voltage", Category::Battery, 103, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Equalization Cycle", Category::Battery, 105, 1, RegKind::U16, 1.0f32, Some("d"), true, Some(0f32), Some(90f32)),
        Parameter::new("Battery Equalization Time", Category::Battery, 106, 1, RegKind::U16, 1.0f32, Some("h"), true, Some(0f32), Some(10f32)),
        Parameter::new("Battery Temperature Compensation", Category::Battery, 107, 1, RegKind::I16, 1.0f32, Some("mV/°C"), true, Some(0f32), Some(50f32)),
        Parameter::new("Maximum Battery Charge Current", Category::Battery, 108, 1, RegKind::U16, 1.0f32, Some("A"), true, None, None),
        Parameter::new("Maximum Battery Discharge Current", Category::Battery, 109, 1, RegKind::U16, 1.0f32, Some("A"), true, None, None),
        Parameter::new("Battery Operation Mode", Category::Battery, 111, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Battery Wake Up", Category::Battery, 112, 1, RegKind::U16, 1.0f32, None, true, None, None), // aka: Battery 2 Wake Up
        Parameter::new("Battery Resistance", Category::Battery, 113, 1, RegKind::U16, 1.0f32, Some("mΩ"), true, Some(0f32), Some(6000f32)),
        Parameter::new("Battery Charging Efficiency", Category::Battery, 114, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Battery Shutdown SOC", Category::Battery, 115, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Battery Restart SOC", Category::Battery, 116, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Battery Low SOC", Category::Battery, 117, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Battery Shutdown Voltage", Category::Battery, 118, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Restart Voltage", Category::Battery, 119, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Low Voltage", Category::Battery, 120, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(61f32)),
        Parameter::new("Battery Generator Charging Start Voltage", Category::Battery, 123, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Battery Generator Charging Start SOC", Category::Battery, 124, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Battery Generator Charging Current", Category::Battery, 125, 1, RegKind::U16, 1.0f32, Some("A"), true, Some(0f32), Some(240f32)),
        Parameter::new("Battery Grid Charging Start Voltage", Category::Battery, 126, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Battery Grid Charging Start SOC", Category::Battery, 127, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Maximum Battery Grid Charge Current", Category::Battery, 128, 1, RegKind::U16, 1.0f32, Some("A"), true, None, None),
        Parameter::new("Battery Generator Charging", Category::Battery, 129, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Battery Grid Charging", Category::Battery, 130, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Force Grid Charge Start SOC", Category::Battery, 191, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(0f32)), // aka: Grid Peak Shaving Power
        Parameter::new("Battery BMS Charging Voltage", Category::Battery, 210, 1, RegKind::U16, 100.0f32, Some("V"), false, None, None),
        Parameter::new("Battery BMS Discharging Voltage", Category::Battery, 211, 1, RegKind::U16, 100.0f32, Some("V"), false, None, None),
        Parameter::new("Battery BMS Charging Current", Category::Battery, 212, 1, RegKind::U16, 1.0f32, Some("A"), false, None, None),
        Parameter::new("Battery BMS Discharging Current", Category::Battery, 213, 1, RegKind::U16, 1.0f32, Some("A"), false, None, None),
        Parameter::new("Battery BMS SOC", Category::Battery, 214, 1, RegKind::U16, 1.0f32, Some("%"), false, None, None),
        Parameter::new("Battery BMS Voltage", Category::Battery, 215, 1, RegKind::U16, 100.0f32, Some("V"), false, None, None),
        Parameter::new("Battery BMS Current", Category::Battery, 216, 1, RegKind::I16, 1.0f32, Some("A"), false, None, None),
        Parameter::new("Battery BMS Max Charging Current", Category::Battery, 218, 1, RegKind::U16, 1.0f32, Some("A"), false, None, None),
        Parameter::new("Battery BMS Max Discharging Current", Category::Battery, 219, 1, RegKind::U16, 1.0f32, Some("A"), false, None, None),
        Parameter::new("Battery Alarm", Category::Battery, 220, 1, RegKind::Bool, 1.0f32, None, false, None, None),
        Parameter::new("Battery Fault", Category::Battery, 221, 1, RegKind::Bool, 1.0f32, None, false, None, None),
        Parameter::new("Battery BMS Other Symbol", Category::Battery, 222, 1, RegKind::U16, 1.0f32, None, false, None, None),
        Parameter::new("Battery BMS Type", Category::Battery, 223, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Daily Battery Charge", Category::Battery, 514, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Daily Battery Discharge", Category::Battery, 515, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Battery Charge", Category::Battery, 516, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Battery Discharge", Category::Battery, 518, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Battery Temperature", Category::Battery, 586, 1, RegKind::I16, 10.0f32, Some("°C"), false, None, None),
        Parameter::new("Battery Voltage", Category::Battery, 587, 1, RegKind::U16, 100.0f32, Some("V"), false, None, None),
        Parameter::new("Battery", Category::Battery, 588, 1, RegKind::U16, 1.0f32, Some("%"), false, None, None),
        Parameter::new("Battery 2", Category::Battery, 589, 1, RegKind::U16, 1.0f32, Some("%"), false, None, None),
        Parameter::new("Battery Power", Category::Battery, 590, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Battery Current", Category::Battery, 591, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Battery Corrected Capacity", Category::Battery, 592, 1, RegKind::U16, 1.0f32, Some("Ah"), false, None, None),
        Parameter::new("Battery 2 Voltage", Category::Battery, 593, 1, RegKind::U16, 100.0f32, Some("V"), false, None, None),
        Parameter::new("Battery 2 Current", Category::Battery, 594, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Battery 2 Power", Category::Battery, 595, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Battery 2 Temperature", Category::Battery, 596, 1, RegKind::U16, 10.0f32, Some("°C"), false, None, None),
        // ---- Grid ----
        Parameter::new("Grid Voltage", Category::Grid, 138, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Charging Signal", Category::Grid, 140, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Force Off Grid", Category::Grid, 179, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Grid Frequency Setting", Category::Grid, 183, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Generator Grid Side", Category::Grid, 189, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("AC Couple", Category::Grid, 234, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Asymmetric Phase Feeding Raw", Category::Grid, 237, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Daily Energy Bought", Category::Grid, 520, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Daily Energy Sold", Category::Grid, 521, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Energy Bought", Category::Grid, 522, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Energy Sold", Category::Grid, 524, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("CT L1 Self Test", Category::Grid, 597, 1, RegKind::Bitflag(1), 1.0f32, None, false, None, None), // aka: CT L2 Self Test, CT L3 Self Test
        Parameter::new("Grid L1 Voltage", Category::Grid, 598, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Grid L2 Voltage", Category::Grid, 599, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Grid L3 Voltage", Category::Grid, 600, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Internal CT L1 Power", Category::Grid, 604, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Internal CT L2 Power", Category::Grid, 605, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Internal CT L3 Power", Category::Grid, 606, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Internal CT Power", Category::Grid, 607, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Grid Frequency", Category::Grid, 609, 1, RegKind::U16, 100.0f32, Some("Hz"), false, None, None),
        Parameter::new("Internal CT L1 Current", Category::Grid, 610, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Internal CT L2 Current", Category::Grid, 611, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Internal CT L3 Current", Category::Grid, 612, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("External CT L1 Current", Category::Grid, 613, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("External CT L2 Current", Category::Grid, 614, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("External CT L3 Current", Category::Grid, 615, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("External CT L1 Power", Category::Grid, 616, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("External CT L2 Power", Category::Grid, 617, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("External CT L3 Power", Category::Grid, 618, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("External CT Power", Category::Grid, 619, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Grid Power Factor", Category::Grid, 621, 1, RegKind::U16, 1000.0f32, Some("%"), false, None, None),
        Parameter::new("Grid L1 Power", Category::Grid, 622, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Grid L2 Power", Category::Grid, 623, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Grid L3 Power", Category::Grid, 624, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Grid Power", Category::Grid, 625, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        // ---- Load ----
        Parameter::new("Daily Load Consumption", Category::Load, 526, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Consumption", Category::Load, 527, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Load L1 Voltage", Category::Load, 644, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Load L2 Voltage", Category::Load, 645, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Load L3 Voltage", Category::Load, 646, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Load L1 Power", Category::Load, 650, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load L2 Power", Category::Load, 651, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load L3 Power", Category::Load, 652, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load Power", Category::Load, 653, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load Frequency", Category::Load, 655, 1, RegKind::U16, 100.0f32, Some("Hz"), false, None, None),
        // ---- Inverter ----
        Parameter::new("DC Temperature", Category::Inverter, 540, 1, RegKind::I16, 10.0f32, Some("°C"), false, None, None),
        Parameter::new("AC Temperature", Category::Inverter, 541, 1, RegKind::I16, 10.0f32, Some("°C"), false, None, None),
        Parameter::new("Output L1 Voltage", Category::Inverter, 627, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Output L2 Voltage", Category::Inverter, 628, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Output L3 Voltage", Category::Inverter, 629, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Output L1 Current", Category::Inverter, 630, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Output L2 Current", Category::Inverter, 631, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Output L3 Current", Category::Inverter, 632, 1, RegKind::I16, 100.0f32, Some("A"), false, None, None),
        Parameter::new("Output L1 Power", Category::Inverter, 633, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Output L2 Power", Category::Inverter, 634, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Output L3 Power", Category::Inverter, 635, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Output Power", Category::Inverter, 636, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Output Frequency", Category::Inverter, 638, 1, RegKind::U16, 100.0f32, Some("Hz"), false, None, None),
        // ---- Generator ----
        Parameter::new("Generator Operating Time", Category::Generator, 121, 1, RegKind::U16, 1.0f32, Some("h"), true, Some(0f32), Some(24f32)),
        Parameter::new("Generator Cooling Time", Category::Generator, 122, 1, RegKind::U16, 1.0f32, Some("h"), true, Some(0f32), Some(24f32)),
        Parameter::new("Generator", Category::Generator, 132, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Generator Port Use", Category::Generator, 133, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("SmartLoad Off Voltage", Category::Generator, 134, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(63f32)),
        Parameter::new("SmartLoad Off SOC", Category::Generator, 135, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("SmartLoad On Voltage", Category::Generator, 136, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(38f32), Some(63f32)),
        Parameter::new("SmartLoad On SOC", Category::Generator, 137, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("PV Minimum Power To Start Generator", Category::Generator, 139, 1, RegKind::U16, 1.0f32, Some("W"), true, Some(0f32), Some(8000f32)),
        Parameter::new("Gen Config Register Raw", Category::Generator, 178, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Generator Peak Shaving Power", Category::Generator, 190, 1, RegKind::U16, 1.0f32, Some("W"), true, Some(0f32), Some(0f32)),
        Parameter::new("Daily Generator Production", Category::Generator, 536, 1, RegKind::U16, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Total Generator Production", Category::Generator, 537, 2, RegKind::U32Swapped, 10.0f32, Some("kWh"), false, None, None),
        Parameter::new("Generator L1 Voltage", Category::Generator, 661, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Generator L2 Voltage", Category::Generator, 662, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Generator L3 Voltage", Category::Generator, 663, 1, RegKind::U16, 10.0f32, Some("V"), false, None, None),
        Parameter::new("Generator L1 Power", Category::Generator, 664, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Generator L2 Power", Category::Generator, 665, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Generator L3 Power", Category::Generator, 666, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Generator Power Total", Category::Generator, 667, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        // ---- Ups ----
        Parameter::new("Load UPS L1 Power", Category::Ups, 640, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load UPS L2 Power", Category::Ups, 641, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load UPS L3 Power", Category::Ups, 642, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        Parameter::new("Load UPS Power", Category::Ups, 643, 1, RegKind::I16, 1.0f32, Some("W"), false, None, None),
        // ---- Tou ----
        Parameter::new("Time of Use Enabled Raw", Category::Tou, 146, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 1 Start Raw", Category::Tou, 148, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 2 Start Raw", Category::Tou, 149, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 3 Start Raw", Category::Tou, 150, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 4 Start Raw", Category::Tou, 151, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 5 Start Raw", Category::Tou, 152, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 6 Start Raw", Category::Tou, 153, 1, RegKind::U16, 1.0f32, None, true, Some(0f32), Some(2359f32)),
        Parameter::new("Time of Use 1 Out Power", Category::Tou, 154, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 2 Out Power", Category::Tou, 155, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 3 Out Power", Category::Tou, 156, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 4 Out Power", Category::Tou, 157, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 5 Out Power", Category::Tou, 158, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 6 Out Power", Category::Tou, 159, 1, RegKind::U16, 1.0f32, Some("W"), true, None, None),
        Parameter::new("Time of Use 1 Voltage", Category::Tou, 160, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 2 Voltage", Category::Tou, 161, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 3 Voltage", Category::Tou, 162, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 4 Voltage", Category::Tou, 163, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 5 Voltage", Category::Tou, 164, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 6 Voltage", Category::Tou, 165, 1, RegKind::U16, 1.0f32, Some("V"), true, Some(0f32), Some(63f32)),
        Parameter::new("Time of Use 1 SoC", Category::Tou, 166, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 2 SoC", Category::Tou, 167, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 3 SoC", Category::Tou, 168, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 4 SoC", Category::Tou, 169, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 5 SoC", Category::Tou, 170, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 6 SoC", Category::Tou, 171, 1, RegKind::U16, 1.0f32, Some("%"), true, Some(0f32), Some(100f32)),
        Parameter::new("Time of Use 1 Raw", Category::Tou, 172, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 2 Raw", Category::Tou, 173, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 3 Raw", Category::Tou, 174, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 4 Raw", Category::Tou, 175, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 5 Raw", Category::Tou, 176, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Time of Use 6 Raw", Category::Tou, 177, 1, RegKind::U16, 1.0f32, None, true, None, None),
        // ---- WorkMode ----
        Parameter::new("Zero Export Power", Category::WorkMode, 104, 1, RegKind::U16, 1.0f32, Some("W"), true, Some(20f32), Some(500f32)),
        Parameter::new("Energy Management Priority", Category::WorkMode, 141, 1, RegKind::U16, 1.0f32, None, true, None, None), // aka: Load Priority
        Parameter::new("System Work Mode", Category::WorkMode, 142, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Max Sell Power", Category::WorkMode, 143, 1, RegKind::U16, 1.0f32, Some("W"), true, Some(0f32), Some(0f32)),
        Parameter::new("Solar Sell", Category::WorkMode, 145, 1, RegKind::U16, 1.0f32, None, true, None, None),
        Parameter::new("Max Solar Power", Category::WorkMode, 340, 1, RegKind::U16, 1.0f32, Some("W"), true, Some(0f32), Some(0f32)),        ]
    }

    /// Reads every parameter in `parameters`, grouped into contiguous Modbus
    /// register blocks (max `DEYE_MAX_REGS_PER_BLOCK` registers/request), same
    /// strategy as sun2000.rs::read_params.
    async fn save_to_influxdb(client: influxdb::Client, thread_name: &str, param: &Parameter) {
        let value = match param.get_influx_value() {
            Some(v) => v,
            None => return, // nothing read this cycle, skip
        };

        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();

        let mut query = Timestamp::Milliseconds(since_the_epoch).into_query(param.name);
        query = query.add_field("value", value);

        match client.query(&query).await {
            Ok(msg) => debug!("{}: influxdb write success: {:?}", thread_name, msg),
            Err(e) => error!("<i>{}</>: influxdb write error: <b>{:?}</>", thread_name, e),
        }
    }

    async fn save_ms_to_influxdb(
        client: influxdb::Client,
        thread_name: &str,
        ms: u64,
        param_count: usize,
    ) {
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();

        let mut query = Timestamp::Milliseconds(since_the_epoch).into_query("inverter_query_time");
        query = query.add_field("value", ms);
        query = query.add_field("param_count", param_count as u8);

        match client.query(&query).await {
            Ok(msg) => debug!("{}: influxdb write success: {:?}", thread_name, msg),
            Err(e) => error!("<i>{}</>: influxdb write error: <b>{:?}</>", thread_name, e),
        }
    }

    async fn read_params(
        &mut self,
        mut ctx: Context,
        parameters: &Vec<Parameter>,
    ) -> io::Result<(Context, Vec<Parameter>)> {
        // connect to influxdb (same "one client per read cycle" approach as sun2000.rs)
        let client = self
            .config
            .influxdb_url
            .as_ref()
            .map(|url| Client::new(url, "deye"));

        let mut params: Vec<Parameter> = vec![];
        let mut disconnected = false;
        let now = Instant::now();

        let mut params_wanted: Vec<_> = parameters.iter().collect();
        params_wanted.sort_by(|a, b| a.address.cmp(&b.address));

        let mut reg_block = vec![];
        let mut all_blocks = vec![];
        let mut start_addr: Option<u16> = None;
        let mut pe = params_wanted.into_iter().peekable();
        while pe.peek().is_some() {
            let p = pe.next().unwrap();
            match start_addr {
                None => start_addr = Some(p.address),
                Some(s) if p.address + p.len - s > DEYE_MAX_REGS_PER_BLOCK => {
                    start_addr = Some(p.address);
                    all_blocks.push(reg_block);
                    reg_block = vec![];
                }
                _ => {}
            }
            reg_block.push(p);
        }
        if !reg_block.is_empty() {
            all_blocks.push(reg_block);
        }

        for (i, reg_block) in all_blocks.iter().enumerate() {
            if disconnected {
                break;
            }

            let last = reg_block.last().unwrap();
            let start_addr = reg_block[0].address;
            let len = last.address + last.len - start_addr;

            let mut attempts = 0;
            while attempts < DEYE_ATTEMPTS_PER_PARAM {
                attempts += 1;
                debug!(
                    "-> obtaining register block #{} start={:#x}, len={}, attempt={}",
                    i, start_addr, len, attempts
                );
                let retval = ctx.read_holding_registers(start_addr, len);
                let start = Instant::now();
                let read_res;
                let read_time;
                match timeout(Duration::from_secs_f32(5.0), retval).await {
                    Ok(res) => {
                        read_res = res;
                        read_time = start.elapsed();
                    }
                    Err(e) => {
                        let msg = format!(
                            "<i>{}</i>: read timeout (attempt #{} of {}), register: <green><i>{:#x}+{}</>, error: <b>{}</>",
                            self.config.name, attempts, DEYE_ATTEMPTS_PER_PARAM, start_addr, len, e
                        );
                        if attempts == DEYE_ATTEMPTS_PER_PARAM {
                            error!("{}", msg);
                            break;
                        } else {
                            warn!("{}", msg);
                            continue;
                        }
                    }
                }
                match read_res {
                    Ok(data) => {
                        if read_time > Duration::from_secs_f32(3.5) {
                            warn!(
                                "<i>{}</i>: inverter has lagged during read, register: <green><i>{:#x}+{}</>, read time: <b>{:?}</>",
                                self.config.name, start_addr, len, read_time
                            );
                        }
                        for p in reg_block {
                            let offset = (p.address - start_addr) as usize;
                            let raw = &data[offset..offset + (p.len as usize)];
                            let mut param = (*p).clone();
                            param.value = match p.kind {
                                RegKind::Text(_) => {
                                    let bytes: Vec<u8> = raw.iter().fold(vec![], |mut acc, w| {
                                        acc.push((w >> 8) as u8);
                                        acc.push((w & 0xff) as u8);
                                        acc
                                    });
                                    let s = String::from_utf8_lossy(&bytes)
                                        .trim_matches(char::from(0))
                                        .to_string();
                                    ParamValue::Text(Some(s))
                                }
                                RegKind::U16 => ParamValue::U16(Some(raw[0])),
                                RegKind::I16 => ParamValue::I16(Some(raw[0] as i16)),
                                RegKind::U32Swapped => {
                                    let v = (raw[0] as u32) | ((raw[1] as u32) << 16);
                                    ParamValue::U32(Some(v))
                                }
                                RegKind::Bool => ParamValue::Bool(Some(raw[0] != 0)),
                                RegKind::Bitflag(mask) => {
                                    ParamValue::Bool(Some((raw[0] & mask) != 0))
                                }
                            };

                            params.push(param);
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = format!(
                            "<i>{}</i>: read error (attempt #{} of {}), register: <green><i>{:#x}+{}</>, error: <b>{}</>",
                            self.config.name, attempts, DEYE_ATTEMPTS_PER_PARAM, start_addr, len, e
                        );
                        match e.kind() {
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                                error!("{}", msg);
                                disconnected = true;
                                break;
                            }
                            _ => {
                                if attempts == DEYE_ATTEMPTS_PER_PARAM {
                                    error!("{}", msg);
                                    break;
                                } else {
                                    warn!("{}", msg);
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }

        // measurement of the actual polling cycle ends here.
        let elapsed = now.elapsed();
        debug!(
            "{}: read {} parameters [⏱️ {:?}]",
            self.config.name,
            params.len(),
            elapsed
        );

        // Write everything to influxdb AFTER the measurement, so the 223
        // sequential HTTP writes no longer inflate `inverter_query_time`.
        //
        // NOTE: this is still awaited inline (not tokio::spawn'ed) - the
        // influxdb crate's HTTP client (surf/hyper) depends on a tokio 0.2
        // timer context that a freshly spawned task on our tokio runtime
        // doesn't have ("there is no timer running" panic). So this still
        // blocks the poll loop for the duration of the writes; it just no
        // longer gets misreported as Modbus read time. If you want the
        // writes to be truly non-blocking, look at how DbTask/db_transmitter
        // (mentioned in the DeyeConfig doc comment) already ships data off
        // to influx elsewhere in the project - route through that channel
        // instead of calling client.query() directly here.
        if let Some(c) = client {
            for param in &params {
                Deye::save_to_influxdb(c.clone(), &self.config.name, param).await;
            }
            let ms = (elapsed.as_secs() * 1_000) + (elapsed.subsec_nanos() / 1_000_000) as u64;
            Deye::save_ms_to_influxdb(c, &self.config.name, ms, params.len()).await;
        }

        Ok((ctx, params))
    }

    /// Writes a single register-sized value to a writable parameter.
    ///
    /// Disabled by default: unless `self.config.enable_write` is `true`, this
    /// always returns an error and never touches the inverter. This covers
    /// ALL settings/time/parameter writes (battery/BMS setpoints, work mode,
    /// time-of-use schedules, self-check time, the power on/off switch, etc.)
    /// - there is no per-parameter override, it's one master switch.
    pub async fn set_parameter(
        &mut self,
        mut ctx: Context,
        param_name: &str,
        raw_value: u16,
    ) -> (Context, Result<()>) {
        if !self.config.enable_write {
            return (
                ctx,
                Err(format!(
                    "{}: refusing to write '{}': writes are disabled (DeyeConfig::enable_write = false)",
                    self.config.name, param_name
                )
                .into()),
            );
        }

        let table = Deye::param_table();
        let p = match table.iter().find(|p| p.name == param_name) {
            Some(p) => p,
            None => {
                return (
                    ctx,
                    Err(format!("{}: unknown parameter '{}'", self.config.name, param_name).into()),
                )
            }
        };

        if !p.writable {
            return (
                ctx,
                Err(format!(
                    "{}: parameter '{}' is read-only on this inverter (no number/select/switch mapping)",
                    self.config.name, param_name
                )
                .into()),
            );
        }

        if let (Some(min), Some(max)) = (p.min, p.max) {
            let scaled = raw_value as f32 / p.gain;
            if scaled < min || scaled > max {
                return (
                    ctx,
                    Err(format!(
                        "{}: value {} for '{}' out of range [{}, {}]",
                        self.config.name, scaled, param_name, min, max
                    )
                    .into()),
                );
            }
        }

        info!(
            "<i>{}</>: writing register {:#x} ({}) = {}",
            self.config.name, p.address, param_name, raw_value
        );
        let retval = ctx.write_single_register(p.address, raw_value);
        let result = match timeout(Duration::from_secs_f32(5.0), retval).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!(
                "{}: write error for '{}': {}",
                self.config.name, param_name, e
            )
            .into()),
            Err(e) => Err(format!(
                "{}: write timeout for '{}': {}",
                self.config.name, param_name, e
            )
            .into()),
        };
        (ctx, result)
    }

    /// Writes the inverter's date/time (registers 0x003E-0x0040), matching the
    /// esphome project's `time_sync.yaml`. Disabled by default, same as
    /// `set_parameter` - gated on `self.config.enable_write`.
    ///
    /// Always returns `ctx` back (even on error/when writes are disabled),
    /// so callers can keep using the same Modbus connection afterwards.
    pub async fn write_time(
        &mut self,
        mut ctx: Context,
        dt: chrono::NaiveDateTime,
    ) -> (Context, Result<()>) {
        /*if !self.config.enable_write {
            return (
                ctx,
                Err(format!(
                    "{}: refusing to write inverter time: writes are disabled (DeyeConfig::enable_write = false)",
                    self.config.name
                )
                .into()),
            );
        }*/
        use chrono::{Datelike, Timelike};
        let regs = [
            ((dt.year() as u16 % 100) << 8) | (dt.month() as u16),
            ((dt.day() as u16) << 8) | (dt.hour() as u16),
            ((dt.minute() as u16) << 8) | (dt.second() as u16),
        ];
        info!(
            "<i>{}</>: writing inverter date/time: {}",
            self.config.name, dt
        );
        let retval = ctx.write_multiple_registers(0x003E, &regs);
        let result = match timeout(Duration::from_secs_f32(5.0), retval).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{}: time write error: {}", self.config.name, e).into()),
            Err(e) => Err(format!("{}: time write timeout: {}", self.config.name, e).into()),
        };
        (ctx, result)
    }

    #[rustfmt::skip]
    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("<i>{}</>: Starting task", self.config.name);
        if self.config.enable_write {
            warn!("<i>{}</>: writes are ENABLED - settings/time/parameter writes will reach the inverter", self.config.name);
        } else {
            info!("<i>{}</>: writes are disabled (read-only mode)", self.config.name);
        }

        let mut poll_interval = Instant::now();
        let mut stats_interval = Instant::now();
        let parameters = Deye::param_table();
        // one-time clock sync for the lifetime of this worker() call - NOT
        // reset on reconnect, so it only fires once even if the connection
        // drops and comes back later.
        let mut time_synced = false;

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            let socket_addr = self.config.host_port.parse().unwrap();
            let slave = if self.config.dongle_connection { Slave(0x01) } else { Slave(0x00) };

            info!("<i>{}</>: connecting to <u>{}</>...", self.config.name, self.config.host_port);
            let retval = tcp::connect_slave(socket_addr, slave);
            let conn = match timeout(Duration::from_secs(5), retval).await {
                Ok(res) => res,
                Err(e) => {
                    error!("<i>{}</>: connect timeout: <b>{}</>", self.config.name, e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            match conn {
                Ok(mut ctx) => {
                    info!("<i>{}</>: connected successfully", self.config.name);

                    //one-time clock sync to GMT/UTC, only on the very first successful
                    //connect of this worker() run (not repeated on later reconnects).
                    //write_time() itself checks enable_write and no-ops (returns Err)
                    //when it's false, so this is safe to always attempt.
                    if !time_synced {
                        let now_utc = chrono::Utc::now().naive_utc();
                        let (new_ctx, result) = self.write_time(ctx, now_utc).await;
                        ctx = new_ctx;
                        match result {
                            Ok(()) => {
                                info!("<i>{}</>: inverter clock synced to {} UTC", self.config.name, now_utc);
                            }
                            Err(e) => {
                                debug!("<i>{}</>: skipping time sync: {}", self.config.name, e);
                            }
                        }
                        // whether it succeeded, failed, or writes are disabled, don't
                        // retry this every reconnect
                        time_synced = true;
                    }

                    let mut terminated = false;
                    // raw "Daily *" register values (all gain=10, i.e. tenths of kWh)
                    let mut daily_pv_raw: Option<u16> = None;
                    // "Total PV Production" is a 32-bit counter (registers 534-535, gain=10)
                    let mut total_pv_raw: Option<u32> = None;
                    let mut daily_batt_charge_raw: Option<u16> = None;
                    let mut daily_batt_discharge_raw: Option<u16> = None;
                    let mut daily_grid_bought_raw: Option<u16> = None;
                    let mut daily_grid_sold_raw: Option<u16> = None;
                    let mut daily_load_raw: Option<u16> = None;
                    let mut daily_gen_raw: Option<u16> = None;

                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            terminated = true;
                        }

                        if stats_interval.elapsed() > Duration::from_secs_f32(DEYE_STATS_DUMP_INTERVAL_SECS) {
                            stats_interval = Instant::now();
                            info!(
                                "<i>{}</>: 📊 query statistics: ok: <b>{}</>, errors: <b>{}</>, daily PV yield: <b>{:.1} kWh</>, total PV production: <b>{:.1} kWh</>",
                                self.config.name, self.poll_ok, self.poll_errors,
                                daily_pv_raw.unwrap_or_default() as f64 / 10.0,
                                total_pv_raw.unwrap_or_default() as f64 / 10.0,
                            );

                            //push all daily energy counters to postgres, natively (own
                            //table, own channel - does not touch DbTask), if configured.
                            if let Some(tx) = &self.config.deye_yield_transmitter {
                                let to_kwh = |raw: Option<u16>| raw.map(|x| x as f64 / 10.0);
                                let to_kwh32 = |raw: Option<u32>| raw.map(|x| x as f64 / 10.0);
                                let y = DeyeDailyYield {
                                    pv_yield_kwh: to_kwh(daily_pv_raw),
                                    pv_total_kwh: to_kwh32(total_pv_raw),
                                    battery_charge_kwh: to_kwh(daily_batt_charge_raw),
                                    battery_discharge_kwh: to_kwh(daily_batt_discharge_raw),
                                    grid_bought_kwh: to_kwh(daily_grid_bought_raw),
                                    grid_sold_kwh: to_kwh(daily_grid_sold_raw),
                                    load_consumption_kwh: to_kwh(daily_load_raw),
                                    generator_yield_kwh: to_kwh(daily_gen_raw),
                                };
                                let _ = tx.send(y);
                            }

                            if terminated { break; }
                        }

                        if poll_interval.elapsed() > Duration::from_secs_f32(DEYE_POLL_INTERVAL_SECS) {
                            poll_interval = Instant::now();
                            let (new_ctx, params) = self.read_params(ctx, &parameters).await?;
                            ctx = new_ctx;

                            if params.len() != parameters.len() {
                                error!(
                                    "<i>{}</>: incomplete parameter list (read: {}, expected: {}), reconnecting...",
                                    self.config.name, params.len(), parameters.len()
                                );
                                self.poll_errors += 1;
                                break;
                            } else {
                                self.poll_ok += 1;
                            }

                            for p in &params {
                                if let ParamValue::U16(v) = p.value {
                                    match p.name {
                                        DEYE_YIELD_PV => daily_pv_raw = v,
                                        DEYE_YIELD_BATTERY_CHARGE => daily_batt_charge_raw = v,
                                        DEYE_YIELD_BATTERY_DISCHARGE => daily_batt_discharge_raw = v,
                                        DEYE_YIELD_GRID_BOUGHT => daily_grid_bought_raw = v,
                                        DEYE_YIELD_GRID_SOLD => daily_grid_sold_raw = v,
                                        DEYE_YIELD_LOAD => daily_load_raw = v,
                                        DEYE_YIELD_GENERATOR => daily_gen_raw = v,
                                        _ => {}
                                    }
                                } else if let ParamValue::U32(v) = p.value {
                                    if p.name == DEYE_YIELD_PV_TOTAL {
                                        total_pv_raw = v;
                                    }
                                }
                            }

                            debug!("Query complete, dump results:");
                            for p in &params {
                                debug!("  {} ({:?}): {} {}", p.name, p.category, p.get_text_value(), p.unit.unwrap_or_default());
                            }

                            if terminated { break; }
                        }

                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                }
                Err(e) => {
                    error!("<i>{}</>: connection error: <b>{}</>", self.config.name, e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }

            if worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }
        }

        info!("{}: task stopped", self.config.name);
        Ok(())
    }
}
