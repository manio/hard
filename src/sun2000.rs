use crate::database::{CommandCode, DbTask};
use crate::lcdproc::{LcdTask, LcdTaskCommand};
use chrono::{Local, LocalResult, NaiveDateTime, TimeZone};
use influxdb::{Client, InfluxDbWriteable, Timestamp, Type};
use std::fmt;
use std::io;
use std::ops::Add;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::timeout;
use tokio_modbus::client::Context;
use tokio_modbus::prelude::*;

pub const SUN2000_POLL_INTERVAL_SECS: f32 = 2.0; //secs between polling
pub const SUN2000_STATS_DUMP_INTERVAL_SECS: f32 = 3600.0; //secs between showing stats
pub const SUN2000_ATTEMPTS_PER_PARAM: u8 = 3; //max read attempts per single parameter

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub enum ParamKind {
    Text(Option<String>),
    NumberU16(Option<u16>),
    NumberI16(Option<i16>),
    NumberU32(Option<u32>),
    NumberI32(Option<i32>),
}

impl fmt::Display for ParamKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParamKind::Text(v) => write!(f, "Text: {}", v.clone().unwrap()),
            ParamKind::NumberU16(v) => write!(f, "NumberU16: {}", v.clone().unwrap()),
            ParamKind::NumberI16(v) => write!(f, "NumberI16: {}", v.clone().unwrap()),
            ParamKind::NumberU32(v) => write!(f, "NumberU32: {}", v.clone().unwrap()),
            ParamKind::NumberI32(v) => write!(f, "NumberI32: {}", v.clone().unwrap()),
        }
    }
}

pub struct Alarm {
    name: &'static str,
    code: u16,
    severity: &'static str,
}

impl Alarm {
    pub fn new(name: &'static str, code: u16, severity: &'static str) -> Self {
        Self {
            name,
            code,
            severity,
        }
    }
}

#[derive(Clone)]
pub struct Parameter {
    name: String,
    value: ParamKind,
    desc: Option<&'static str>,
    unit: Option<&'static str>,
    gain: u16,
    reg_address: u16,
    len: u16,
    initial_read: bool,
    save_to_influx: bool,
}

impl Parameter {
    pub fn new(
        name: &'static str,
        value: ParamKind,
        desc: Option<&'static str>,
        unit: Option<&'static str>,
        gain: u16,
        reg_address: u16,
        len: u16,
        initial_read: bool,
        save_to_influx: bool,
    ) -> Self {
        Self {
            name: String::from(name),
            value,
            desc,
            unit,
            gain,
            reg_address,
            len,
            initial_read,
            save_to_influx,
        }
    }

    pub fn new_from_string(
        name: String,
        value: ParamKind,
        desc: Option<&'static str>,
        unit: Option<&'static str>,
        gain: u16,
        reg_address: u16,
        len: u16,
        initial_read: bool,
        save_to_influx: bool,
    ) -> Self {
        Self {
            name,
            value,
            desc,
            unit,
            gain,
            reg_address,
            len,
            initial_read,
            save_to_influx,
        }
    }

    pub fn get_text_value(&self) -> String {
        match &self.value {
            ParamKind::Text(v) => {
                return v.clone().unwrap();
            }
            ParamKind::NumberU16(v) => {
                return if self.gain != 1 {
                    (v.clone().unwrap() as f32 / self.gain as f32).to_string()
                } else {
                    v.clone().unwrap().to_string()
                }
            }
            ParamKind::NumberI16(v) => {
                return if self.gain != 1 {
                    (v.clone().unwrap() as f32 / self.gain as f32).to_string()
                } else {
                    v.clone().unwrap().to_string()
                }
            }
            ParamKind::NumberU32(v) => {
                return if self.gain != 1 {
                    (v.clone().unwrap() as f32 / self.gain as f32).to_string()
                } else {
                    if self.unit.unwrap_or_default() == "epoch" {
                        match *v {
                            Some(epoch_secs) => {
                                let naive = NaiveDateTime::from_timestamp(epoch_secs as i64, 0);
                                match Local.from_local_datetime(&naive) {
                                    LocalResult::Single(dt) => {
                                        format!("{}, {:?}", epoch_secs, dt.to_rfc2822())
                                    }
                                    _ => "timestamp conversion error".into(),
                                }
                            }
                            None => "None".into(),
                        }
                    } else {
                        v.clone().unwrap().to_string()
                    }
                }
            }
            ParamKind::NumberI32(v) => {
                return if self.gain != 1 {
                    (v.clone().unwrap() as f32 / self.gain as f32).to_string()
                } else {
                    v.clone().unwrap().to_string()
                }
            }
        }
    }

    pub fn get_influx_value(&self) -> influxdb::Type {
        match &self.value {
            ParamKind::Text(v) => {
                return Type::Text(v.clone().unwrap());
            }
            ParamKind::NumberU16(v) => {
                return if self.gain != 1 {
                    Type::Float(v.clone().unwrap() as f64 / self.gain as f64)
                } else {
                    Type::UnsignedInteger(v.clone().unwrap() as u64)
                }
            }
            ParamKind::NumberI16(v) => {
                return if self.gain != 1 {
                    Type::Float(v.clone().unwrap() as f64 / self.gain as f64)
                } else {
                    Type::SignedInteger(v.clone().unwrap() as i64)
                }
            }
            ParamKind::NumberU32(v) => {
                return if self.gain != 1 {
                    Type::Float(v.clone().unwrap() as f64 / self.gain as f64)
                } else {
                    Type::UnsignedInteger(v.clone().unwrap() as u64)
                }
            }
            ParamKind::NumberI32(v) => {
                return if self.gain != 1 {
                    Type::Float(v.clone().unwrap() as f64 / self.gain as f64)
                } else {
                    Type::SignedInteger(v.clone().unwrap() as i64)
                }
            }
        }
    }
}

pub struct Sun2000State {
    pub device_status: Option<u16>,
    pub storage_status: Option<i16>,
    pub grid_code: Option<u16>,
    pub state_1: Option<u16>,
    pub state_2: Option<u16>,
    pub state_3: Option<u32>,
    pub alarm_1: Option<u16>,
    pub alarm_2: Option<u16>,
    pub alarm_3: Option<u16>,
}

impl Sun2000State {
    fn get_device_status_description(code: u16) -> &'static str {
        match code {
            0x0000 => "Standby: initializing",
            0x0001 => "Standby: detecting insulation resistance",
            0x0002 => "Standby: detecting irradiation",
            0x0003 => "Standby: grid detecting",
            0x0100 => "Starting",
            0x0200 => "On-grid",
            0x0201 => "Grid Connection: power limited",
            0x0202 => "Grid Connection: self-derating",
            0x0300 => "Shutdown: fault",
            0x0301 => "Shutdown: command",
            0x0302 => "Shutdown: OVGR",
            0x0303 => "Shutdown: communication disconnected",
            0x0304 => "Shutdown: power limited",
            0x0305 => "Shutdown: manual startup required",
            0x0306 => "Shutdown: DC switches disconnected",
            0x0307 => "Shutdown: rapid cutoff",
            0x0308 => "Shutdown: input underpowered",
            0x0401 => "Grid scheduling: cosphi-P curve",
            0x0402 => "Grid scheduling: Q-U curve",
            0x0403 => "Grid scheduling: PF-U curve",
            0x0404 => "Grid scheduling: dry contact",
            0x0405 => "Grid scheduling: Q-P curve",
            0x0500 => "Spot-check ready",
            0x0501 => "Spot-checking",
            0x0600 => "Inspecting",
            0x0700 => "AFCI self check",
            0x0800 => "I-V scanning",
            0x0900 => "DC input detection",
            0x0a00 => "Running: off-grid charging",
            0xa000 => "Standby: no irradiation",
            _ => "Unknown State",
        }
    }

    fn get_storage_status_description(code: i16) -> &'static str {
        match code {
            0 => "offline",
            1 => "standby",
            2 => "running",
            3 => "fault",
            4 => "sleep mode",
            _ => "Unknown State",
        }
    }

    #[rustfmt::skip]
    fn get_grid_code_description(code: u16) -> String {
        let grid_code = match code {
            0 => ("VDE-AR-N-4105", "Germany"),
            1 => ("NB/T 32004", "China"),
            2 => ("UTE C 15-712-1(A)", "France"),
            3 => ("UTE C 15-712-1(B)", "France"),
            4 => ("UTE C 15-712-1(C)", "France"),
            5 => ("VDE 0126-1-1-BU", "Bulgary"),
            6 => ("VDE 0126-1-1-GR(A)", "Greece"),
            7 => ("VDE 0126-1-1-GR(B)", "Greece"),
            8 => ("BDEW-MV", "Germany"),
            9 => ("G59-England", "UK"),
            10 => ("G59-Scotland", "UK"),
            11 => ("G83-England", "UK"),
            12 => ("G83-Scotland", "UK"),
            13 => ("CEI0-21", "Italy"),
            14 => ("EN50438-CZ", "Czech Republic"),
            15 => ("RD1699/661", "Spain"),
            16 => ("RD1699/661-MV480", "Spain"),
            17 => ("EN50438-NL", "Netherlands"),
            18 => ("C10/11", "Belgium"),
            19 => ("AS4777", "Australia"),
            20 => ("IEC61727", "General"),
            21 => ("Custom (50 Hz)", "Custom"),
            22 => ("Custom (60 Hz)", "Custom"),
            23 => ("CEI0-16", "Italy"),
            24 => ("CHINA-MV480", "China"),
            25 => ("CHINA-MV", "China"),
            26 => ("TAI-PEA", "Thailand"),
            27 => ("TAI-MEA", "Thailand"),
            28 => ("BDEW-MV480", "Germany"),
            29 => ("Custom MV480 (50 Hz)", "Custom"),
            30 => ("Custom MV480 (60 Hz)", "Custom"),
            31 => ("G59-England-MV480", "UK"),
            32 => ("IEC61727-MV480", "General"),
            33 => ("UTE C 15-712-1-MV480", "France"),
            34 => ("TAI-PEA-MV480", "Thailand"),
            35 => ("TAI-MEA-MV480", "Thailand"),
            36 => ("EN50438-DK-MV480", "Denmark"),
            37 => ("Japan standard (50 Hz)", "Japan"),
            38 => ("Japan standard (60 Hz)", "Japan"),
            39 => ("EN50438-TR-MV480", "Turkey"),
            40 => ("EN50438-TR", "Turkey"),
            41 => ("C11/C10-MV480", "Belgium"),
            42 => ("Philippines", "Philippines"),
            43 => ("Philippines-MV480", "Philippines"),
            44 => ("AS4777-MV480", "Australia"),
            45 => ("NRS-097-2-1", "South Africa"),
            46 => ("NRS-097-2-1-MV480", "South Africa"),
            47 => ("KOREA", "South Korea"),
            48 => ("IEEE 1547-MV480", "USA"),
            49 => ("IEC61727-60Hz", "General"),
            50 => ("IEC61727-60Hz-MV480", "General"),
            51 => ("CHINA_MV500", "China"),
            52 => ("ANRE", "Romania"),
            53 => ("ANRE-MV480", "Romania"),
            54 => ("ELECTRIC RULE NO.21-MV480", "California, USA"),
            55 => ("HECO-MV480", "Hawaii, USA"),
            56 => ("PRC_024_Eastern-MV480", "Eastern USA"),
            57 => ("PRC_024_Western-MV480", "Western USA"),
            58 => ("PRC_024_Quebec-MV480", "Quebec, Canada"),
            59 => ("PRC_024_ERCOT-MV480", "Texas, USA"),
            60 => ("PO12.3-MV480", "Spain"),
            61 => ("EN50438_IE-MV480", "Ireland"),
            62 => ("EN50438_IE", "Ireland"),
            63 => ("IEEE 1547a-MV480", "USA"),
            64 => ("Japan standard (MV420-50 Hz)", "Japan"),
            65 => ("Japan standard (MV420-60 Hz)", "Japan"),
            66 => ("Japan standard (MV440-50 Hz)", "Japan"),
            67 => ("Japan standard (MV440-60 Hz)", "Japan"),
            68 => ("IEC61727-50Hz-MV500", "General"),
            70 => ("CEI0-16-MV480", "Italy"),
            71 => ("PO12.3", "Spain"),
            72 => ("Japan standard (MV400-50 Hz)", "Japan"),
            73 => ("Japan standard (MV400-60 Hz)", "Japan"),
            74 => ("CEI0-21-MV480", "Italy"),
            75 => ("KOREA-MV480", "South Korea"),
            76 => ("Egypt ETEC", "Egypt"),
            77 => ("Egypt ETEC-MV480", "Egypt"),
            78 => ("CHINA_MV800", "China"),
            79 => ("IEEE 1547-MV600", "USA"),
            80 => ("ELECTRIC RULE NO.21-MV600", "California, USA"),
            81 => ("HECO-MV600", "Hawaii, USA"),
            82 => ("PRC_024_Eastern-MV600", "Eastern USA"),
            83 => ("PRC_024_Western-MV600", "Western USA"),
            84 => ("PRC_024_Quebec-MV600", "Quebec, Canada"),
            85 => ("PRC_024_ERCOT-MV600", "Texas, USA"),
            86 => ("IEEE 1547a-MV600", "USA"),
            87 => ("EN50549-LV", "Ireland"),
            88 => ("EN50549-MV480", "Ireland"),
            89 => ("Jordan-Transmission", "Jordan"),
            90 => ("Jordan-Transmission-MV480", "Jordan"),
            91 => ("NAMIBIA", "Namibia"),
            92 => ("ABNT NBR 16149", "Brazil"),
            93 => ("ABNT NBR 16149-MV480", "Brazil"),
            94 => ("SA_RPPs", "South Africa"),
            95 => ("SA_RPPs-MV480", "South Africa"),
            96 => ("INDIA", "India"),
            97 => ("INDIA-MV500", "India"),
            98 => ("ZAMBIA", "Zambia"),
            99 => ("ZAMBIA-MV480", "Zambia"),
            100 => ("Chile", "Chile"),
            101 => ("Chile-MV480", "Chile"),
            102 => ("CHINA-MV500-STD", "China"),
            103 => ("CHINA-MV480-STD", "China"),
            104 => ("Mexico-MV480", "Mexico"),
            105 => ("Malaysian", "Malaysia"),
            106 => ("Malaysian-MV480", "Malaysia"),
            107 => ("KENYA_ETHIOPIA", "East Africa"),
            108 => ("KENYA_ETHIOPIA-MV480", "East Africa"),
            109 => ("G59-England-MV800", "UK"),
            110 => ("NEGERIA", "Negeria"),
            111 => ("NEGERIA-MV480", "Negeria"),
            112 => ("DUBAI", "Dubai"),
            113 => ("DUBAI-MV480", "Dubai"),
            114 => ("Northern Ireland", "Northern Ireland"),
            115 => ("Northern Ireland-MV480", "Northern Ireland"),
            116 => ("Cameroon", "Cameroon"),
            117 => ("Cameroon-MV480", "Cameroon"),
            118 => ("Jordan Distribution", "Jordan"),
            119 => ("Jordan Distribution-MV480", "Jordan"),
            120 => ("Custom MV600-50 Hz", "Custom"),
            121 => ("AS4777-MV800", "Australia"),
            122 => ("INDIA-MV800", "India"),
            123 => ("IEC61727-MV800", "General"),
            124 => ("BDEW-MV800", "Germany"),
            125 => ("ABNT NBR 16149-MV800", "Brazil"),
            126 => ("UTE C 15-712-1-MV800", "France"),
            127 => ("Chile-MV800", "Chile"),
            128 => ("Mexico-MV800", "Mexico"),
            129 => ("EN50438-TR-MV800", "Turkey"),
            130 => ("TAI-PEA-MV800", "Thailand"),
            133 => ("NRS-097-2-1-MV800", "South Africa"),
            134 => ("SA_RPPs-MV800", "South Africa"),
            135 => ("Jordan-Transmission-MV800", "Jordan"),
            136 => ("Jordan-Distribution-MV800", "Jordan"),
            137 => ("Egypt ETEC-MV800", "Egypt"),
            138 => ("DUBAI-MV800", "Dubai"),
            139 => ("SAUDI-MV800", "Saudi Arabia"),
            140 => ("EN50438_IE-MV800", "Ireland"),
            141 => ("EN50549-MV800", "Ireland"),
            142 => ("Northern Ireland-MV800", "Northern Ireland"),
            143 => ("CEI0-21-MV800", "Italy"),
            144 => ("IEC 61727-MV800-60Hz", "General"),
            145 => ("NAMIBIA_MV480", "Namibia"),
            146 => ("Japan (LV202-50 Hz)", "Japan"),
            147 => ("Japan (LV202-60 Hz)", "Japan"),
            148 => ("Pakistan-MV800", "Pakistan"),
            149 => ("BRASIL-ANEEL-MV800", "Brazil"),
            150 => ("Israel-MV800", "Israel"),
            151 => ("CEI0-16-MV800", "Italy"),
            152 => ("ZAMBIA-MV800", "Zambia"),
            153 => ("KENYA_ETHIOPIA-MV800", "East Africa"),
            154 => ("NAMIBIA_MV800", "Namibia"),
            155 => ("Cameroon-MV800", "Cameroon"),
            156 => ("NIGERIA-MV800", "Nigeria"),
            157 => ("ABUDHABI-MV800", "Abu Dhabi"),
            158 => ("LEBANON", "Lebanon"),
            159 => ("LEBANON-MV480", "Lebanon"),
            160 => ("LEBANON-MV800", "Lebanon"),
            161 => ("ARGENTINA-MV800", "Argentina"),
            162 => ("ARGENTINA-MV500", "Argentina"),
            163 => ("Jordan-Transmission-HV", "Jordan"),
            164 => ("Jordan-Transmission-HV480", "Jordan"),
            165 => ("Jordan-Transmission-HV800", "Jordan"),
            166 => ("TUNISIA", "Tunisia"),
            167 => ("TUNISIA-MV480", "Tunisia"),
            168 => ("TUNISIA-MV800", "Tunisia"),
            169 => ("JAMAICA-MV800", "Jamaica"),
            170 => ("AUSTRALIA-NER", "Australia"),
            171 => ("AUSTRALIA-NER-MV480", "Australia"),
            172 => ("AUSTRALIA-NER-MV800", "Australia"),
            173 => ("SAUDI", "Saudi Arabia"),
            174 => ("SAUDI-MV480", "Saudi Arabia"),
            175 => ("Ghana-MV480", "Ghana"),
            176 => ("Israel", "Israel"),
            177 => ("Israel-MV480", "Israel"),
            178 => ("Chile-PMGD", "Chile"),
            179 => ("Chile-PMGD-MV480", "Chile"),
            180 => ("VDE-AR-N4120-HV", "Germany"),
            181 => ("VDE-AR-N4120-HV480", "Germany"),
            182 => ("VDE-AR-N4120-HV800", "Germany"),
            183 => ("IEEE 1547-MV800", "USA"),
            184 => ("Nicaragua-MV800", "Nicaragua"),
            185 => ("IEEE 1547a-MV800", "USA"),
            186 => ("ELECTRIC RULE NO.21-MV800", "California, USA"),
            187 => ("HECO-MV800", "Hawaii, USA"),
            188 => ("PRC_024_Eastern-MV800", "Eastern USA"),
            189 => ("PRC_024_Western-MV800", "Western USA"),
            190 => ("PRC_024_Quebec-MV800", "Quebec, Canada"),
            191 => ("PRC_024_ERCOT-MV800", "Texas, USA"),
            192 => ("Custom-MV800-50Hz", "Custom"),
            193 => ("RD1699/661-MV800", "Spain"),
            194 => ("PO12.3-MV800", "Spain"),
            195 => ("Mexico-MV600", "Mexico"),
            196 => ("Vietnam-MV800", "Vietnam"),
            197 => ("CHINA-LV220/380", "China"),
            198 => ("SVG-LV", "Dedicated"),
            199 => ("Vietnam", "Vietnam"),
            200 => ("Vietnam-MV480", "Vietnam"),
            201 => ("Chile-PMGD-MV800", "Chile"),
            202 => ("Ghana-MV800", "Ghana"),
            203 => ("TAIPOWER", "Taiwan"),
            204 => ("TAIPOWER-MV480", "Taiwan"),
            205 => ("TAIPOWER-MV800", "Taiwan"),
            206 => ("IEEE 1547-LV208", "USA"),
            207 => ("IEEE 1547-LV240", "USA"),
            208 => ("IEEE 1547a-LV208", "USA"),
            209 => ("IEEE 1547a-LV240", "USA"),
            210 => ("ELECTRIC RULE NO.21-LV208", "USA"),
            211 => ("ELECTRIC RULE NO.21-LV240", "USA"),
            212 => ("HECO-O+M+H-LV208", "USA"),
            213 => ("HECO-O+M+H-LV240", "USA"),
            214 => ("PRC_024_Eastern-LV208", "USA"),
            215 => ("PRC_024_Eastern-LV240", "USA"),
            216 => ("PRC_024_Western-LV208", "USA"),
            217 => ("PRC_024_Western-LV240", "USA"),
            218 => ("PRC_024_ERCOT-LV208", "USA"),
            219 => ("PRC_024_ERCOT-LV240", "USA"),
            220 => ("PRC_024_Quebec-LV208", "USA"),
            221 => ("PRC_024_Quebec-LV240", "USA"),
            222 => ("ARGENTINA-MV480", "Argentina"),
            223 => ("Oman", "Oman"),
            224 => ("Oman-MV480", "Oman"),
            225 => ("Oman-MV800", "Oman"),
            226 => ("Kuwait", "Kuwait"),
            227 => ("Kuwait-MV480", "Kuwait"),
            228 => ("Kuwait-MV800", "Kuwait"),
            229 => ("Bangladesh", "Bangladesh"),
            230 => ("Bangladesh-MV480", "Bangladesh"),
            231 => ("Bangladesh-MV800", "Bangladesh"),
            232 => ("Chile-Net_Billing", "Chile"),
            233 => ("EN50438-NL-MV480", "Netherlands"),
            234 => ("Bahrain", "Bahrain"),
            235 => ("Bahrain-MV480", "Bahrain"),
            236 => ("Bahrain-MV800", "Bahrain"),
            238 => ("Japan-MV550-50Hz", "Japan"),
            239 => ("Japan-MV550-60Hz", "Japan"),
            241 => ("ARGENTINA", "Argentina"),
            242 => ("KAZAKHSTAN-MV800", "Kazakhstan"),
            243 => ("Mauritius", "Mauritius"),
            244 => ("Mauritius-MV480", "Mauritius"),
            245 => ("Mauritius-MV800", "Mauritius"),
            246 => ("Oman-PDO-MV800", "Oman"),
            247 => ("EN50438-SE", "Sweden"),
            248 => ("TAI-MEA-MV800", "Thailand"),
            249 => ("Pakistan", "Pakistan"),
            250 => ("Pakistan-MV480", "Pakistan"),
            251 => ("PORTUGAL-MV800", "Portugal"),
            252 => ("HECO-L+M-LV208", "USA"),
            253 => ("HECO-L+M-LV240", "USA"),
            254 => ("C10/11-MV800", "Belgium"),
            255 => ("Austria", "Austria"),
            256 => ("Austria-MV480", "Austria"),
            257 => ("G98", "UK"),
            258 => ("G99-TYPEA-LV", "UK"),
            259 => ("G99-TYPEB-LV", "UK"),
            260 => ("G99-TYPEB-HV", "UK"),
            261 => ("G99-TYPEB-HV-MV480", "UK"),
            262 => ("G99-TYPEB-HV-MV800", "UK"),
            263 => ("G99-TYPEC-HV-MV800", "UK"),
            264 => ("G99-TYPED-MV800", "UK"),
            265 => ("G99-TYPEA-HV", "UK"),
            266 => ("CEA-MV800", "India"),
            267 => ("EN50549-MV400", "Europe"),
            268 => ("VDE-AR-N4110", "Germany"),
            269 => ("VDE-AR-N4110-MV480", "Germany"),
            270 => ("VDE-AR-N4110-MV800", "Germany"),
            271 => ("Panama-MV800", "Panama"),
            272 => ("North Macedonia-MV800", "North Macedonia"),
            273 => ("NTS", "Spain"),
            274 => ("NTS-MV480", "Spain"),
            275 => ("NTS-MV800", "Spain"),
            _ => ("unknown", "unknown"),
        };
        format!("standard: {:?}, country: {:?}", grid_code.0, grid_code.1)
    }

    #[rustfmt::skip]
    fn get_state1_description(code: u16) -> String {
        let mut descr = String::from("");
        let state1_masks = vec! [
            (0b0000_0000_0000_0001, "standby"),
            (0b0000_0000_0000_0010, "grid-connected"),
            (0b0000_0000_0000_0100, "grid-connected normally"),
            (0b0000_0000_0000_1000, "grid connection with derating due to power rationing"),
            (0b0000_0000_0001_0000, "grid connection with derating due to internal causes of the solar inverter"),
            (0b0000_0000_0010_0000, "normal stop"),
            (0b0000_0000_0100_0000, "stop due to faults"),
            (0b0000_0000_1000_0000, "stop due to power rationing"),
            (0b0000_0001_0000_0000, "shutdown"),
            (0b0000_0010_0000_0000, "spot check"),
        ];
        for mask in state1_masks {
            if code & mask.0 > 0 {
                descr = descr.add(mask.1).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
        }
        descr
    }

    #[rustfmt::skip]
    fn get_state2_description(code: u16) -> String {
        let mut descr = String::from("");
        let state2_masks = vec! [
            (0b0000_0000_0000_0001, ("locked", "unlocked")),
            (0b0000_0000_0000_0010, ("PV disconnected", "PV connected")),
            (0b0000_0000_0000_0100, ("no DSP data collection", "DSP data collection")),
        ];
        for mask in state2_masks {
            if code & mask.0 > 0 {
                descr = descr.add(mask.1.1).add(" | ");
            } else {
                descr = descr.add(mask.1.0).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
        }
        descr
    }

    #[rustfmt::skip]
    fn get_state3_description(code: u32) -> String {
        let mut descr = String::from("");
        let state3_masks = vec! [
            (0b0000_0000_0000_0000_0000_0000_0000_0001, ("on-grid", "off-grid")),
            (0b0000_0000_0000_0000_0000_0000_0000_0010, ("off-grid switch disabled", "off-grid switch enabled",)),
        ];
        for mask in state3_masks {
            if code & mask.0 > 0 {
                descr = descr.add(mask.1.1).add(" | ");
            } else {
                descr = descr.add(mask.1.0).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
        }
        descr
    }

    #[rustfmt::skip]
    fn get_alarm1_description(code: u16) -> String {
        let mut descr = String::from("");
        let alarm1_masks = vec! [
            (0b0000_0000_0000_0001, Alarm::new("High String Input Voltage", 2001, "Major")),
            (0b0000_0000_0000_0010, Alarm::new("DC Arc Fault", 2002, "Major")),
            (0b0000_0000_0000_0100, Alarm::new("String Reverse Connection", 2011, "Major")),
            (0b0000_0000_0000_1000, Alarm::new("String Current Backfeed", 2012, "Warning")),
            (0b0000_0000_0001_0000, Alarm::new("Abnormal String Power", 2013, "Warning")),
            (0b0000_0000_0010_0000, Alarm::new("AFCI Self-Check Fail", 2021, "Major")),
            (0b0000_0000_0100_0000, Alarm::new("Phase Wire Short-Circuited to PE", 2031, "Major")),
            (0b0000_0000_1000_0000, Alarm::new("Grid Loss", 2032, "Major")),
            (0b0000_0001_0000_0000, Alarm::new("Grid Undervoltage", 2033, "Major")),
            (0b0000_0010_0000_0000, Alarm::new("Grid Overvoltage", 2034, "Major")),
            (0b0000_0100_0000_0000, Alarm::new("Grid Volt. Imbalance", 2035, "Major")),
            (0b0000_1000_0000_0000, Alarm::new("Grid Overfrequency", 2036, "Major")),
            (0b0001_0000_0000_0000, Alarm::new("Grid Underfrequency", 2037, "Major")),
            (0b0010_0000_0000_0000, Alarm::new("Unstable Grid Frequency", 2038, "Major")),
            (0b0100_0000_0000_0000, Alarm::new("Output Overcurrent", 2039, "Major")),
            (0b1000_0000_0000_0000, Alarm::new("Output DC Component Overhigh", 2040, "Major")),
        ];
        for mask in alarm1_masks {
            if code & mask.0 > 0 {
                descr = descr.add(
                    format!("code={} {:?} severity={}", mask.1.code, mask.1.name, mask.1.severity).as_str()
                ).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
            descr
        } else {
            "None".into()
        }
    }

    #[rustfmt::skip]
    fn get_alarm2_description(code: u16) -> String {
        let mut descr = String::from("");
        let alarm2_masks = vec! [
            (0b0000_0000_0000_0001, Alarm::new("Abnormal Residual Current", 2051, "Major")),
            (0b0000_0000_0000_0010, Alarm::new("Abnormal Grounding", 2061, "Major")),
            (0b0000_0000_0000_0100, Alarm::new("Low Insulation Resistance", 2062, "Major")),
            (0b0000_0000_0000_1000, Alarm::new("Overtemperature", 2063, "Minor")),
            (0b0000_0000_0001_0000, Alarm::new("Device Fault", 2064, "Major")),
            (0b0000_0000_0010_0000, Alarm::new("Upgrade Failed or Version Mismatch", 2065, "Minor")),
            (0b0000_0000_0100_0000, Alarm::new("License Expired", 2066, "Warning")),
            (0b0000_0000_1000_0000, Alarm::new("Faulty Monitoring Unit", 61440, "Minor")),
            (0b0000_0001_0000_0000, Alarm::new("Faulty Power Collector", 2067, "Major")),
            (0b0000_0010_0000_0000, Alarm::new("Battery abnormal", 2068, "Minor")),
            (0b0000_0100_0000_0000, Alarm::new("Active Islanding", 2070, "Major")),
            (0b0000_1000_0000_0000, Alarm::new("Passive Islanding", 2071, "Major")),
            (0b0001_0000_0000_0000, Alarm::new("Transient AC Overvoltage", 2072, "Major")),
            (0b0010_0000_0000_0000, Alarm::new("Peripheral port short circuit", 2075, "Warning")),
            (0b0100_0000_0000_0000, Alarm::new("Churn output overload", 2077, "Major")),
            (0b1000_0000_0000_0000, Alarm::new("Abnormal PV module configuration", 2080, "Major")),
        ];
        for mask in alarm2_masks {
            if code & mask.0 > 0 {
                descr = descr.add(
                    format!("code={} {:?} severity={}", mask.1.code, mask.1.name, mask.1.severity).as_str()
                ).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
            descr
        } else {
            "None".into()
        }
    }

    #[rustfmt::skip]
    fn get_alarm3_description(code: u16) -> String {
        let mut descr = String::from("");
        let alarm3_masks = vec! [
            (0b0000_0000_0000_0001, Alarm::new("Optimizer fault", 2081, "Warning")),
            (0b0000_0000_0000_0010, Alarm::new("Built-in PID operation abnormal", 2085, "Minor")),
            (0b0000_0000_0000_0100, Alarm::new("High input string voltage to ground", 2014, "Major")),
            (0b0000_0000_0000_1000, Alarm::new("External Fan Abnormal", 2086, "Major")),
            (0b0000_0000_0001_0000, Alarm::new("Battery Reverse Connection", 2069, "Major")),
            (0b0000_0000_0010_0000, Alarm::new("On-grid/Off-grid controller abnormal", 2082, "Major")),
            (0b0000_0000_0100_0000, Alarm::new("PV String Loss", 2015, "Warning")),
            (0b0000_0000_1000_0000, Alarm::new("Internal Fan Abnormal", 2087, "Major")),
            (0b0000_0001_0000_0000, Alarm::new("DC Protection Unit Abnormal", 2088, "Major")),
        ];
        for mask in alarm3_masks {
            if code & mask.0 > 0 {
                descr = descr.add(
                    format!("code={} {:?} severity={}", mask.1.code, mask.1.name, mask.1.severity).as_str()
                ).add(" | ");
            }
        }
        if !descr.is_empty() {
            descr.pop();
            descr.pop();
            descr.pop();
            descr
        } else {
            "None".into()
        }
    }

    fn set_new_status(
        &mut self,
        thread_name: &String,
        device_status: Option<u16>,
        storage_status: Option<i16>,
        grid_code: Option<u16>,
        state_1: Option<u16>,
        state_2: Option<u16>,
        state_3: Option<u32>,
        alarm_1: Option<u16>,
        alarm_2: Option<u16>,
        alarm_3: Option<u16>,
    ) -> bool {
        let mut failure = false;
        if device_status.is_some() && self.device_status != device_status {
            info!(
                "{}: status: {}",
                thread_name,
                Sun2000State::get_device_status_description(device_status.unwrap())
            );
            self.device_status = device_status;
        }
        if storage_status.is_some() && self.storage_status != storage_status {
            info!(
                "{}: storage status: {}",
                thread_name,
                Sun2000State::get_storage_status_description(storage_status.unwrap())
            );
            self.storage_status = storage_status;
        }
        if grid_code.is_some() && self.grid_code != grid_code {
            info!(
                "{}: grid: {}",
                thread_name,
                Sun2000State::get_grid_code_description(grid_code.unwrap())
            );
            self.grid_code = grid_code;
        }
        if state_1.is_some() && self.state_1 != state_1 {
            info!(
                "{}: state_1: {}",
                thread_name,
                Sun2000State::get_state1_description(state_1.unwrap())
            );
            self.state_1 = state_1;
        }
        if state_2.is_some() && self.state_2 != state_2 {
            info!(
                "{}: state_2: {}",
                thread_name,
                Sun2000State::get_state2_description(state_2.unwrap())
            );
            self.state_2 = state_2;
        }
        if state_3.is_some() && self.state_3 != state_3 {
            info!(
                "{}: state_3: {}",
                thread_name,
                Sun2000State::get_state3_description(state_3.unwrap())
            );
            self.state_3 = state_3;
        }
        if alarm_1.is_some() && self.alarm_1 != alarm_1 {
            if alarm_1.unwrap() != 0 || self.alarm_1.is_some() {
                info!(
                    "{}: alarm_1: {}",
                    thread_name,
                    Sun2000State::get_alarm1_description(alarm_1.unwrap())
                );
            }
            self.alarm_1 = alarm_1;
            failure = alarm_1.unwrap() != 0;
        }
        if alarm_2.is_some() && self.alarm_2 != alarm_2 {
            if alarm_2.unwrap() != 0 || self.alarm_2.is_some() {
                info!(
                    "{}: alarm_2: {}",
                    thread_name,
                    Sun2000State::get_alarm2_description(alarm_2.unwrap())
                );
            }
            self.alarm_2 = alarm_2;
            failure = alarm_2.unwrap() != 0;
        }
        if alarm_3.is_some() && self.alarm_3 != alarm_3 {
            if alarm_3.unwrap() != 0 || self.alarm_3.is_some() {
                info!(
                    "{}: alarm_3: {}",
                    thread_name,
                    Sun2000State::get_alarm3_description(alarm_3.unwrap())
                );
            }
            self.alarm_3 = alarm_3;
            failure = alarm_3.unwrap() != 0;
        }
        failure
    }
}

pub struct Sun2000 {
    pub name: String,
    pub host_port: String,
    pub poll_ok: u64,
    pub poll_errors: u64,
    pub influxdb_url: Option<String>,
    pub lcd_transmitter: Sender<LcdTask>,
    pub db_transmitter: Sender<DbTask>,
    pub mode_change_script: Option<String>,
    pub optimizers: bool,
    pub battery_installed: bool,
    pub dongle_connection: bool,
}

impl Sun2000 {
    #[rustfmt::skip]
    pub fn param_table() -> Vec<Parameter> {
        vec![
            Parameter::new("model_name", ParamKind::Text(None), None,  None, 1, 30000, 15, true, false),
            Parameter::new("serial_number", ParamKind::Text(None), None,  None, 1, 30015, 10, true, false),
            Parameter::new("product_number", ParamKind::Text(None), None,  None, 1, 30025, 10, true, false),
            Parameter::new("model_id", ParamKind::NumberU16(None), None, None, 1, 30070, 1, true, false),
            Parameter::new("nb_pv_strings", ParamKind::NumberU16(None), None, None, 1, 30071, 1, true, false),
            Parameter::new("nb_mpp_tracks", ParamKind::NumberU16(None), None, None, 1, 30072, 1, true, false),
            Parameter::new("rated_power", ParamKind::NumberU32(None), None, Some("W"), 1, 30073, 2, true, false),
            Parameter::new("P_max", ParamKind::NumberU32(None), None, Some("W"), 1, 30075, 2, false, false),
            Parameter::new("S_max", ParamKind::NumberU32(None), None, Some("VA"), 1, 30077, 2, false, false),
            Parameter::new("Q_max_out", ParamKind::NumberI32(None), None, Some("VAr"), 1, 30079, 2, false, false),
            Parameter::new("Q_max_in", ParamKind::NumberI32(None), None, Some("VAr"), 1, 30081, 2, false, false),
            Parameter::new("state_1", ParamKind::NumberU16(None), None, Some("state_bitfield16"), 1, 32000, 1, false, false),
            Parameter::new("state_2", ParamKind::NumberU16(None), None, Some("state_opt_bitfield16"), 1, 32002, 1, false, false),
            Parameter::new("state_3", ParamKind::NumberU32(None), None, Some("state_opt_bitfield32"), 1, 32003, 2, false, false),
            Parameter::new("alarm_1", ParamKind::NumberU16(None), None, Some("alarm_bitfield16"), 1, 32008, 1, false, false),
            Parameter::new("alarm_2", ParamKind::NumberU16(None), None, Some("alarm_bitfield16"), 1, 32009, 1, false, false),
            Parameter::new("alarm_3", ParamKind::NumberU16(None), None, Some("alarm_bitfield16"), 1, 32010, 1, false, false),
            Parameter::new("input_power", ParamKind::NumberI32(None), None, Some("W"), 1, 32064, 2, false, true),
            Parameter::new("line_voltage_A_B", ParamKind::NumberU16(None), Some("grid_voltage"), Some("V"), 10, 32066, 1, false, true),
            Parameter::new("line_voltage_B_C", ParamKind::NumberU16(None), None, Some("V"), 10, 32067, 1, false, true),
            Parameter::new("line_voltage_C_A", ParamKind::NumberU16(None), None, Some("V"), 10, 32068, 1, false, true),
            Parameter::new("phase_A_voltage", ParamKind::NumberU16(None), None, Some("V"), 10, 32069, 1, false, true),
            Parameter::new("phase_B_voltage", ParamKind::NumberU16(None), None, Some("V"), 10, 32070, 1, false, true),
            Parameter::new("phase_C_voltage", ParamKind::NumberU16(None), None, Some("V"), 10, 32071, 1, false, true),
            Parameter::new("phase_A_current", ParamKind::NumberI32(None), Some("grid_current"), Some("A"), 1000, 32072, 2, false, true),
            Parameter::new("phase_B_current", ParamKind::NumberI32(None), None, Some("A"), 1000, 32074, 2, false, true),
            Parameter::new("phase_C_current", ParamKind::NumberI32(None), None, Some("A"), 1000, 32076, 2, false, true),
            Parameter::new("day_active_power_peak", ParamKind::NumberI32(None), None, Some("W"), 1, 32078, 2, false, false),
            Parameter::new("active_power", ParamKind::NumberI32(None), None, Some("W"), 1, 32080, 2, false, true),
            Parameter::new("reactive_power", ParamKind::NumberI32(None), None, Some("VA"), 1, 32082, 2, false, true),
            Parameter::new("power_factor", ParamKind::NumberI16(None), None, None, 1000, 32084, 1, false, true),
            Parameter::new("grid_frequency", ParamKind::NumberU16(None), None, Some("Hz"), 100, 32085, 1, false, true),
            Parameter::new("efficiency", ParamKind::NumberU16(None), None, Some("%"), 100, 32086, 1, false, true),
            Parameter::new("internal_temperature", ParamKind::NumberI16(None), None, Some("°C"), 10, 32087, 1, false, true),
            Parameter::new("insulation_resistance", ParamKind::NumberU16(None), None, Some("MΩ"), 100, 32088, 1, false, false),
            Parameter::new("device_status", ParamKind::NumberU16(None), None, Some("status_enum"), 1, 32089, 1, false, true),
            Parameter::new("fault_code", ParamKind::NumberU16(None), None, None, 1, 32090, 1, false, false),
            Parameter::new("startup_time", ParamKind::NumberU32(None), None, Some("epoch"), 1, 32091, 2, false, false),
            Parameter::new("shutdown_time", ParamKind::NumberU32(None), None, Some("epoch"), 1, 32093, 2, false, false),
            Parameter::new("accumulated_yield_energy", ParamKind::NumberU32(None), None, Some("kWh"), 100, 32106, 2, false, true),
            Parameter::new("unknown_time_1", ParamKind::NumberU32(None), None, Some("epoch"), 1, 32110, 2, false, false),
            Parameter::new("unknown_time_2", ParamKind::NumberU32(None), None, Some("epoch"), 1, 32156, 2, false, false),
            Parameter::new("unknown_time_3", ParamKind::NumberU32(None), None, Some("epoch"), 1, 32160, 2, false, false),
            Parameter::new("unknown_time_4", ParamKind::NumberU32(None), None, Some("epoch"), 1, 35113, 2, false, false),
            Parameter::new("storage_status", ParamKind::NumberI16(None), None, Some("storage_status_enum"), 1, 37000, 1, false, false),
            Parameter::new("storage_charge_discharge_power", ParamKind::NumberI32(None), None, Some("W"), 1, 37001, 2, false, false),
            Parameter::new("power_meter_active_power", ParamKind::NumberI32(None), None, Some("W"), 1, 37113, 2, false, false),
            Parameter::new("grid_A_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37101, 2, false, true),
            Parameter::new("grid_B_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37103, 2, false, true),
            Parameter::new("grid_C_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37105, 2, false, true),
            Parameter::new("active_grid_A_current", ParamKind::NumberI32(None), None, Some("I"), 100, 37107, 2, false, true),
            Parameter::new("active_grid_B_current", ParamKind::NumberI32(None), None, Some("I"), 100, 37109, 2, false, true),
            Parameter::new("active_grid_C_current", ParamKind::NumberI32(None), None, Some("I"), 100, 37111, 2, false, true),
            Parameter::new("active_grid_power_factor", ParamKind::NumberI16(None), None, None, 1000, 37117, 1, false, false),
            Parameter::new("active_grid_frequency", ParamKind::NumberI16(None), None, Some("Hz"), 100, 37118, 1, false, true),
            Parameter::new("grid_exported_energy", ParamKind::NumberI32(None), None, Some("kWh"), 100, 37119, 2, false, false),
            Parameter::new("grid_accumulated_energy", ParamKind::NumberU32(None), None, Some("kWh"), 100, 37121, 2, false, false),
            Parameter::new("active_grid_A_B_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37126, 2, false, true),
            Parameter::new("active_grid_B_C_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37128, 2, false, true),
            Parameter::new("active_grid_C_A_voltage", ParamKind::NumberI32(None), None, Some("V"), 10, 37130, 2, false, true),
            Parameter::new("active_grid_A_power", ParamKind::NumberI32(None), None, Some("W"), 1, 37132, 2, false, true),
            Parameter::new("active_grid_B_power", ParamKind::NumberI32(None), None, Some("W"), 1, 37134, 2, false, true),
            Parameter::new("active_grid_C_power", ParamKind::NumberI32(None), None, Some("W"), 1, 37136, 2, false, true),
            Parameter::new("daily_yield_energy", ParamKind::NumberU32(None), None, Some("kWh"), 100, 32114, 2, false, true),
            Parameter::new("system_time", ParamKind::NumberU32(None), None, Some("epoch"), 1, 40000, 2, false, false),
            Parameter::new("unknown_time_5", ParamKind::NumberU32(None), None, Some("epoch"), 1, 40500, 2, false, false),
            Parameter::new("grid_code", ParamKind::NumberU16(None), None, Some("grid_enum"), 1, 42000, 1, false, false),
            Parameter::new("time_zone", ParamKind::NumberI16(None), None, Some("min"), 1, 43006, 1, false, false),
        ]
    }

    async fn save_to_influxdb(
        client: influxdb::Client,
        thread_name: &String,
        param: Parameter,
    ) -> Result<()> {
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();

        let mut query = Timestamp::Milliseconds(since_the_epoch).into_query(&param.name);
        query = query.add_field("value", param.get_influx_value());

        match client.query(&query).await {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", thread_name, msg);
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", thread_name, e);
            }
        }

        Ok(())
    }

    async fn save_ms_to_influxdb(
        client: influxdb::Client,
        thread_name: &String,
        ms: u64,
        param_count: usize,
    ) -> Result<()> {
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();

        let mut query = Timestamp::Milliseconds(since_the_epoch).into_query("inverter_query_time");
        query = query.add_field("value", ms);
        query = query.add_field("param_count", param_count as u8);

        match client.query(&query).await {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", thread_name, msg);
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", thread_name, e);
            }
        }

        Ok(())
    }

    async fn read_params(
        &mut self,
        mut ctx: Context,
        parameters: &Vec<Parameter>,
        initial_read: bool,
    ) -> io::Result<(Context, Vec<Parameter>)> {
        // connect to influxdb
        let client = match &self.influxdb_url {
            Some(url) => Some(Client::new(url, "sun2000")),
            None => None,
        };

        let mut params: Vec<Parameter> = vec![];
        let now = Instant::now();
        for p in parameters.into_iter().filter(|s| {
            (initial_read && s.initial_read)
                || (!initial_read
                    && (s.save_to_influx
                        || s.name.starts_with("state_")
                        || s.name.starts_with("alarm_")
                        || s.name.ends_with("_status")
                        || s.name.ends_with("_code")))
        }) {
            let mut attempts = 0;
            while attempts < SUN2000_ATTEMPTS_PER_PARAM {
                attempts = attempts + 1;
                debug!("-> obtaining {} ({:?})...", p.name, p.desc);
                let retval = ctx.read_holding_registers(p.reg_address, p.len);
                let read_res;
                match timeout(Duration::from_secs_f32(3.5), retval).await {
                    Ok(res) => {
                        read_res = res;
                    }
                    Err(e) => {
                        error!(
                            "{}: read timeout (attempt #{} of {}), register: {}, error: {}",
                            self.name, attempts, SUN2000_ATTEMPTS_PER_PARAM, p.name, e
                        );
                        continue;
                    }
                }
                match read_res {
                    Ok(data) => {
                        let mut val;
                        match &p.value {
                            ParamKind::Text(_) => {
                                let bytes: Vec<u8> = data.iter().fold(vec![], |mut x, elem| {
                                    if (elem >> 8) as u8 != 0 {
                                        x.push((elem >> 8) as u8);
                                    }
                                    if (elem & 0xff) as u8 != 0 {
                                        x.push((elem & 0xff) as u8);
                                    }
                                    x
                                });
                                let id = String::from_utf8(bytes).unwrap();
                                val = ParamKind::Text(Some(id));
                            }
                            ParamKind::NumberU16(_) => {
                                debug!("-> {} = {:?}", p.name, data);
                                val = ParamKind::NumberU16(Some(data[0] as u16));
                            }
                            ParamKind::NumberI16(_) => {
                                debug!("-> {} = {:?}", p.name, data);
                                val = ParamKind::NumberI16(Some(data[0] as i16));
                            }
                            ParamKind::NumberU32(_) => {
                                let new_val: u32 = ((data[0] as u32) << 16) | data[1] as u32;
                                debug!("-> {} = {:X?} {:X}", p.name, data, new_val);
                                val = ParamKind::NumberU32(Some(new_val));
                                if p.unit.unwrap_or_default() == "epoch" && new_val == 0 {
                                    //zero epoch makes no sense, let's set it to None
                                    val = ParamKind::NumberU32(None);
                                }
                            }
                            ParamKind::NumberI32(_) => {
                                let new_val: i32 =
                                    ((data[0] as i32) << 16) | (data[1] as u32) as i32;
                                debug!("-> {} = {:X?} {:X}", p.name, data, new_val);
                                val = ParamKind::NumberI32(Some(new_val));
                            }
                        }
                        let param = Parameter::new_from_string(
                            p.name.clone(),
                            val,
                            p.desc.clone(),
                            p.unit.clone(),
                            p.gain,
                            p.reg_address,
                            p.len,
                            p.initial_read,
                            p.save_to_influx,
                        );
                        params.push(param.clone());

                        //write data to influxdb if configured
                        if let Some(c) = client.clone() {
                            if !initial_read && p.save_to_influx {
                                let _ = Sun2000::save_to_influxdb(c, &self.name, param).await;
                            }
                        }

                        break; //read next parameter
                    }
                    Err(e) => {
                        error!(
                            "{}: read error (attempt #{} of {}), register: {}, error: {}",
                            self.name, attempts, SUN2000_ATTEMPTS_PER_PARAM, p.name, e
                        );
                        continue;
                    }
                }
            }
        }

        let elapsed = now.elapsed();
        let ms = (elapsed.as_secs() * 1_000) + (elapsed.subsec_nanos() / 1_000_000) as u64;
        debug!(
            "{}: read {} parameters [⏱ {} ms]",
            self.name,
            params.len(),
            ms
        );

        //save query time
        if let Some(c) = client {
            let _ = Sun2000::save_ms_to_influxdb(c, &self.name, ms, params.len()).await;
        }
        Ok((ctx, params))
    }

    #[rustfmt::skip]
    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);
        let mut poll_interval = Instant::now();
        let mut stats_interval = Instant::now();
        let mut terminated = false;

        let mut state = Sun2000State {
            device_status: None,
            storage_status: None,
            grid_code: None,
            state_1: None,
            state_2: None,
            state_3: None,
            alarm_1: None,
            alarm_2: None,
            alarm_3: None,
        };

        loop {
            if terminated || worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            let socket_addr = self.host_port.parse().unwrap();

            let slave;
            if self.dongle_connection {
                //USB dongle connection: Slave ID has to be 0x01
                slave = Slave(0x01);
            } else {
                //internal wifi: Slave ID has to be 0x00, otherwise the inverter is not responding
                slave = Slave(0x00);
            }

            info!("{}: connecting to {}...", self.name, self.host_port);
            let retval = tcp::connect_slave(socket_addr, slave);
            let conn;
            match timeout(Duration::from_secs(5), retval).await {
                Ok(res) => { conn = res; }
                Err(e) => {
                    error!("{}: connect timeout: {}", self.name, e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }

            match conn {
                Ok(mut ctx) => {
                    info!("{}: connected successfully", self.name);
                    //initial parameters table
                    let mut parameters = Sun2000::param_table();
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    //obtaining all parameters from inverter
                    let (new_ctx, params) = self.read_params(ctx, &parameters, true).await?;
                    ctx = new_ctx;
                    let mut nb_pv_strings: Option<u16> = None;
                    for p in &params {
                        match &p.value {
                            ParamKind::NumberU16(n) => {
                                match p.name.as_ref() {
                                    "nb_pv_strings" => nb_pv_strings = *n,
                                    "grid_code" => {
                                        //set and print initial grid code
                                        state.set_new_status(
                                            &self.name, None, None, *n, None, None, None, None,
                                            None, None,
                                        );
                                    }
                                    _ => {}
                                }
                            }
                            ParamKind::Text(_) => match p.name.as_ref() {
                                "model_name" => {
                                    info!("{}: model name: {}", self.name, &p.get_text_value());
                                }
                                "serial_number" => {
                                    info!("{}: serial number: {}", self.name, &p.get_text_value());
                                }
                                "product_number" => {
                                    info!("{}: product number: {}", self.name, &p.get_text_value());
                                }
                                _ => {}
                            },
                            ParamKind::NumberU32(_) => match p.name.as_ref() {
                                "rated_power" => {
                                    info!(
                                        "{}: rated power: {} {}",
                                        self.name,
                                        &p.get_text_value(),
                                        p.unit.clone().unwrap_or_default()
                                    );
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }

                    match nb_pv_strings {
                        Some(n) => {
                            info!("{}: number of available strings: {}", self.name, n);
                            for i in 1..=n {
                                parameters.push(Parameter::new_from_string(format!("pv_{:02}_voltage", i), ParamKind::NumberI16(None), None, Some("V"), 10, 32014 + i*2, 1, false, true));
                                parameters.push(Parameter::new_from_string(format!("pv_{:02}_current", i), ParamKind::NumberI16(None), None, Some("A"), 100, 32015 + i*2, 1, false, true));
                            }
                        }
                        None => {}
                    }

                    if self.optimizers {
                        info!("{}: config: optimizers enabled", self.name);
                        parameters.push(Parameter::new("nb_optimizers", ParamKind::NumberU16(None), None, None, 1, 37200, 1, false, false));
                        parameters.push(Parameter::new("nb_online_optimizers", ParamKind::NumberU16(None), None, None, 1, 37201, 1, false, true));
                    }

                    if self.battery_installed {
                        info!("{}: config: battery installed", self.name);
                        parameters.push(Parameter::new("storage_working_mode", ParamKind::NumberI16(None), None, Some("storage_working_mode_enum"), 1, 47004, 1, false, true));
                        parameters.push(Parameter::new("storage_time_of_use_price", ParamKind::NumberI16(None), None, Some("storage_tou_price_enum"), 1, 47027, 1, false, true));
                        parameters.push(Parameter::new("storage_lcoe", ParamKind::NumberU32(None), None, None, 1000, 47069, 2, false, true));
                        parameters.push(Parameter::new("storage_maximum_charging_power", ParamKind::NumberU32(None), None, Some("W"), 1, 47075, 2, false, true));
                        parameters.push(Parameter::new("storage_maximum_discharging_power", ParamKind::NumberU32(None), None, Some("W"), 1, 47077, 2, false, true));
                        parameters.push(Parameter::new("storage_power_limit_grid_tied_point", ParamKind::NumberI32(None), None, Some("W"), 1, 47079, 2, false, true));
                        parameters.push(Parameter::new("storage_charging_cutoff_capacity", ParamKind::NumberU16(None), None, Some("%"), 10, 47081, 1, false, true));
                        parameters.push(Parameter::new("storage_discharging_cutoff_capacity", ParamKind::NumberU16(None), None, Some("%"), 10, 47082, 1, false, true));
                        parameters.push(Parameter::new("storage_forced_charging_and_discharging_period", ParamKind::NumberU16(None), None, Some("min"), 1, 47083, 1, false, true));
                        parameters.push(Parameter::new("storage_forced_charging_and_discharging_power", ParamKind::NumberI32(None), None, Some("min"), 1, 47084, 2, false, true));
                        parameters.push(Parameter::new("storage_current_day_charge_capacity", ParamKind::NumberU32(None), None, Some("kWh"), 100, 37015, 2, false, true));
                        parameters.push(Parameter::new("storage_current_day_discharge_capacity", ParamKind::NumberU32(None), None, Some("kWh"), 100, 37017, 2, false, true));
                    }

                    let mut daily_yield_energy: Option<u32> = None;
                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            debug!("{}: Got terminate signal from main", self.name);
                            terminated = true;
                        }

                        if terminated
                            || stats_interval.elapsed()
                                > Duration::from_secs_f32(SUN2000_STATS_DUMP_INTERVAL_SECS)
                        {
                            stats_interval = Instant::now();
                            info!(
                                "{}: 📊 inverter query statistics: ok: {}, errors: {}, daily energy yield: {:.1} kWh",
                                self.name, self.poll_ok, self.poll_errors,
                                daily_yield_energy.unwrap_or_default() as f64 / 100.0,
                            );

                            //push daily yield to postgres
                            let task = DbTask {
                                command: CommandCode::UpdateDailyEnergyYield,
                                value: {if let Some(x) = daily_yield_energy {Some(x as i32)} else {None}},
                            };
                            let _ = self.db_transmitter.send(task);

                            if terminated {
                                break;
                            }
                        }

                        if poll_interval.elapsed()
                            > Duration::from_secs_f32(SUN2000_POLL_INTERVAL_SECS)
                        {
                            poll_interval = Instant::now();
                            let mut device_status: Option<u16> = None;
                            let mut storage_status: Option<i16> = None;
                            let mut grid_code: Option<u16> = None;
                            let mut state_1: Option<u16> = None;
                            let mut state_2: Option<u16> = None;
                            let mut state_3: Option<u32> = None;
                            let mut alarm_1: Option<u16> = None;
                            let mut alarm_2: Option<u16> = None;
                            let mut alarm_3: Option<u16> = None;
                            let mut active_power: Option<i32> = None;

                            //obtaining all parameters from inverter
                            let (new_ctx, params) =
                                self.read_params(ctx, &parameters, false).await?;
                            ctx = new_ctx;
                            for p in &params {
                                match p.value {
                                    ParamKind::NumberU16(n) => match p.name.as_ref() {
                                        "fault_code" => match n {
                                            Some(fault_code) => {
                                                if fault_code != 0 {
                                                    error!(
                                                        "{} inverter fault code is: {:#08X}",
                                                        self.name, fault_code
                                                    );
                                                }
                                            }
                                            _ => {}
                                        },
                                        "device_status" => device_status = n,
                                        "grid_code" => grid_code = n,
                                        "state_1" => state_1 = n,
                                        "state_2" => state_2 = n,
                                        "alarm_1" => alarm_1 = n,
                                        "alarm_2" => alarm_2 = n,
                                        "alarm_3" => alarm_3 = n,
                                        _ => {}
                                    },
                                    ParamKind::NumberI16(n) => match p.name.as_ref() {
                                        "storage_status" => storage_status = n,
                                        _ => {}
                                    },
                                    ParamKind::NumberU32(n) => match p.name.as_ref() {
                                        "state_3" => state_3 = n,
                                        "daily_yield_energy" => daily_yield_energy = n,
                                        _ => {}
                                    },
                                    ParamKind::NumberI32(n) => match p.name.as_ref() {
                                        "active_power" => active_power = n,
                                        _ => {}
                                    },
                                    _ => {}
                                }
                            }

                            let param_count = parameters.iter().filter(|s| s.save_to_influx ||
                                s.name.starts_with("state_") ||
                                s.name.starts_with("alarm_") ||
                                s.name.ends_with("_status") ||
                                s.name.ends_with("_code")).count();
                            if params.len() != param_count {
                                error!("{}: problem obtaining a complete parameter list (read: {}, expected: {}), reconnecting...", self.name, params.len(), param_count);
                                self.poll_errors = self.poll_errors + 1;
                                break;
                            } else {
                                self.poll_ok = self.poll_ok + 1;
                            }

                            //setting new inverter state/alarm
                            state.set_new_status(
                                &self.name,
                                device_status,
                                storage_status,
                                grid_code,
                                state_1,
                                state_2,
                                state_3,
                                alarm_1,
                                alarm_2,
                                alarm_3,
                            );

                            //pass PV info to Lcdproc
                            let task = LcdTask {
                                command: LcdTaskCommand::SetLineText,
                                int_arg: 0,
                                string_arg: Some(format!("PV {:.1} kWh, {} W",
                                    daily_yield_energy.unwrap_or_default() as f64 / 100.0,
                                    active_power.unwrap_or_default(),
                                ))};
                            let _ = self.lcd_transmitter.send(task);

                            //process obtained parameters
                            debug!("Query complete, dump results:");
                            for p in &params {
                                debug!(
                                    "  {} ({:?}): {} {}",
                                    p.name,
                                    p.desc.clone().unwrap_or_default(),
                                    p.get_text_value(),
                                    p.unit.clone().unwrap_or_default()
                                );
                            }
                        }

                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                }
                Err(e) => {
                    error!("{}", e);
                }
            }
        }

        info!("{}: task stopped", self.name);
        Ok(())
    }
}
