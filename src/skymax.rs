use chrono::{DateTime, Utc};
use crc16::*;
use futures::io::Error;
use influxdb::{Client, InfluxDbWriteable};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::prelude::*;
use tokio::time::timeout;
use tokio_compat_02::FutureExt;

pub const SKYMAX_POLL_INTERVAL_SECS: f32 = 10.0; //secs between polling

//masks for status bits
pub const STATUS1_AC_CHARGE: u8 = 1 << 0;
pub const STATUS1_SCC_CHARGE: u8 = 1 << 1;
pub const STATUS1_LOAD: u8 = 1 << 4;
pub const STATUS2_FLOATING_CHARGE: u8 = 1 << 2;
pub const STATUS2_SWITCH: u8 = 1 << 1;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, InfluxDbWriteable)]
pub struct GeneralStatusParameters {
    time: DateTime<Utc>,
    voltage_grid: Option<f32>,
    freq_grid: Option<f32>,
    voltage_out: Option<f32>,
    freq_out: Option<f32>,
    load_va: Option<u16>,
    load_watt: Option<u16>,
    load_percent: Option<u8>,
    voltage_bus: Option<u16>,
    voltage_batt: Option<f32>,
    batt_charge_current: Option<u16>,
    batt_capacity: Option<u8>,
    temp_heatsink: Option<u16>,
    pv_input_current: Option<u16>,
    pv_input_voltage: Option<f32>,
    scc_voltage: Option<f32>,
    batt_discharge_current: Option<u32>,
    device_status: Option<u8>,
    batt_voltage_offset_for_fans_on: Option<u8>,
    eeprom_version: Option<u8>,
    pv_charging_power: Option<u32>,
    device_status2: Option<u8>,
}

impl GeneralStatusParameters {
    fn binary_to_u8(input: String) -> Option<u8> {
        let mut out_byte: u8 = 0;
        for (index, bit) in input.chars().rev().enumerate() {
            if bit == '1' {
                out_byte |= 1 << index;
            } else if bit != '0' {
                return None;
            }
        }
        Some(out_byte)
    }

    pub fn new(data: String) -> Option<Self> {
        //split input elements by space
        let mut elements: Vec<_> = data.split(" ").collect();

        //we need at least 21 values
        if elements.len() < 21 {
            return None;
        }

        Some(Self {
            time: Utc::now(),
            voltage_grid: elements.remove(0).parse().ok(),
            freq_grid: elements.remove(0).parse().ok(),
            voltage_out: elements.remove(0).parse().ok(),
            freq_out: elements.remove(0).parse().ok(),
            load_va: elements.remove(0).parse().ok(),
            load_watt: elements.remove(0).parse().ok(),
            load_percent: elements.remove(0).parse().ok(),
            voltage_bus: elements.remove(0).parse().ok(),
            voltage_batt: elements.remove(0).parse().ok(),
            batt_charge_current: elements.remove(0).parse().ok(),
            batt_capacity: elements.remove(0).parse().ok(),
            temp_heatsink: elements.remove(0).parse().ok(),
            pv_input_current: elements.remove(0).parse().ok(),
            pv_input_voltage: elements.remove(0).parse().ok(),
            scc_voltage: elements.remove(0).parse().ok(),
            batt_discharge_current: elements.remove(0).parse().ok(),
            device_status: GeneralStatusParameters::binary_to_u8(elements.remove(0).to_string()),
            batt_voltage_offset_for_fans_on: elements
                .remove(0)
                .to_string()
                .parse()
                .map(|v: u8| v * 10)
                .ok(), //unit is *10mV
            eeprom_version: elements.remove(0).parse().ok(),
            pv_charging_power: elements.remove(0).parse().ok(),
            device_status2: GeneralStatusParameters::binary_to_u8(elements.remove(0).to_string()),
        })
    }

    async fn save_to_influxdb(&self, influxdb_url: &String, thread_name: &String) -> Result<()> {
        // connect to influxdb
        let client = Client::new(influxdb_url, "skymax");

        match client
            .query(&self.clone().into_query("status_params"))
            .await
        {
            Ok(msg) => {
                debug!("{}: influxdb write success: {:?}", thread_name, msg);
            }
            Err(e) => {
                error!("{}: influxdb write error: {:?}", thread_name, e);
            }
        }

        Ok(())
    }
}

impl fmt::Display for GeneralStatusParameters {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "GeneralStatusParameters (QPIGS):\n")?;
        write!(
            f,
            "------------------------------------------------------------------------\n"
        )?;

        if self.voltage_grid.is_some() {
            write!(f, "  AC Grid voltage: {:?} V\n", self.voltage_grid.unwrap())?;
        };
        if self.freq_grid.is_some() {
            write!(f, "  AC Grid frequency: {:?} Hz\n", self.freq_grid.unwrap())?;
        };
        if self.voltage_out.is_some() {
            write!(f, "  AC out voltage: {:?} V\n", self.voltage_out.unwrap())?;
        };
        if self.freq_out.is_some() {
            write!(f, "  AC out frequency: {:?} Hz\n", self.freq_out.unwrap())?;
        };
        if self.load_percent.is_some() {
            write!(f, "  Load: {:?} %\n", self.load_percent.unwrap())?;
        };
        if self.load_watt.is_some() {
            write!(f, "  Load: {:?} W\n", self.load_watt.unwrap())?;
        };
        if self.load_va.is_some() {
            write!(f, "  Load: {:?} VA\n", self.load_va.unwrap())?;
        };
        if self.voltage_bus.is_some() {
            write!(f, "  Bus voltage: {:?} V\n", self.voltage_bus.unwrap())?;
        };
        if self.pv_input_voltage.is_some() {
            write!(
                f,
                "  PV input voltage: {:?} V\n",
                self.pv_input_voltage.unwrap()
            )?;
        };
        if self.pv_input_current.is_some() {
            write!(
                f,
                "  PV input current: {:?} A\n",
                self.pv_input_current.unwrap()
            )?;
        };
        if self.scc_voltage.is_some() {
            write!(f, "  SCC voltage: {:?} V\n", self.scc_voltage.unwrap())?;
        };
        if self.temp_heatsink.is_some() {
            write!(
                f,
                "  Heatsink temperature: {:?} °C\n",
                self.temp_heatsink.unwrap()
            )?;
        };
        if self.batt_capacity.is_some() {
            write!(
                f,
                "  Battery capacity: {:?} %\n",
                self.batt_capacity.unwrap()
            )?;
        };
        if self.voltage_batt.is_some() {
            write!(f, "  Battery voltage: {:?} V\n", self.voltage_batt.unwrap())?;
        };
        if self.batt_charge_current.is_some() {
            write!(
                f,
                "  Battery charge current: {:?} A\n",
                self.batt_charge_current.unwrap()
            )?;
        };
        if self.batt_discharge_current.is_some() {
            write!(
                f,
                "  Battery discharge current: {:?} A\n",
                self.batt_discharge_current.unwrap()
            )?;
        };
        match self.device_status {
            Some(status) => {
                write!(f, "  Device status: {:08b}\n", status)?;
                write!(
                    f,
                    "    |- Load status on: {:?}\n",
                    status & STATUS1_LOAD != 0
                )?;
                write!(
                    f,
                    "    |- SCC charge on: {:?}\n",
                    status & STATUS1_SCC_CHARGE != 0
                )?;
                write!(
                    f,
                    "    |- AC charge on: {:?}\n",
                    status & STATUS1_AC_CHARGE != 0
                )?;
            }
            None => (),
        };

        if self.batt_voltage_offset_for_fans_on.is_some() {
            write!(
                f,
                "  Battery voltage offset for fans on: {:?} mV\n",
                self.batt_voltage_offset_for_fans_on.unwrap()
            )?;
        };
        if self.eeprom_version.is_some() {
            write!(f, "  EEPROM version: {:?}\n", self.eeprom_version.unwrap())?;
        };
        if self.pv_charging_power.is_some() {
            write!(
                f,
                "  PV charging power: {:?} W\n",
                self.pv_charging_power.unwrap()
            )?;
        };
        match self.device_status2 {
            Some(status) => {
                write!(f, "  Device status2: {:03b}\n", status)?;
                write!(
                    f,
                    "    |- Charging to floating mode: {:?}\n",
                    status & STATUS2_FLOATING_CHARGE != 0
                )?;
                write!(f, "    |- Switch on: {:?}\n", status & STATUS2_SWITCH != 0)?;
            }
            None => (),
        };

        Ok(())
    }
}

pub struct InverterMode {
    pub last_change: Instant,
    pub mode: char,
}

impl InverterMode {
    fn get_mode_description(mode: char) -> &'static str {
        match mode {
            'P' => "Power On Mode",
            'S' => "Standby Mode",
            'L' => "Line Mode",
            'B' => "Battery Mode",
            'F' => "Fault Mode",
            'H' => "Power Saving Mode",
            _ => "Unknown",
        }
    }

    fn set_new_mode(&mut self, current_mode: char, thread_name: &String) -> bool {
        if self.mode != current_mode {
            warn!(
                "{}: inverter mode changed from {:?} to {:?} after {:?}",
                thread_name,
                InverterMode::get_mode_description(self.mode),
                InverterMode::get_mode_description(current_mode),
                self.last_change.elapsed()
            );
            self.mode = current_mode;
            self.last_change = Instant::now();
            return true;
        }
        false
    }
}

pub struct Skymax {
    pub name: String,
    pub device_path: String,
    pub poll_ok: u64,
    pub poll_errors: u64,
    pub influxdb_url: Option<String>,
}

impl Skymax {
    fn fix_crc16_byte(input: u8) -> u8 {
        /* function for adjusting CRC values to not cover "special" bytes */
        if input == 0x28 || input == 0x0d || input == 0x0a {
            input + 1
        } else {
            input
        }
    }

    fn verify_input_data(mut data: Vec<u8>) -> std::result::Result<String, String> {
        debug!("input data={:02X?}", data);

        //check for start/stop sequence
        if data.pop().unwrap() != 0x0d {
            return Err("received data is not properly terminated".to_string());
        }
        if data.get(0).unwrap() != &('(' as u8) {
            return Err("incorrect start sequence in received data".to_string());
        }

        //get crc from data
        let frame_crc_lo = data.pop().unwrap() as u8;
        let frame_crc_hi = data.pop().unwrap() as u8;

        //calculate xmodem checksum
        let crc = State::<XMODEM>::calculate(data.as_slice());

        //fix and compare checksum
        if Skymax::fix_crc16_byte((crc & 0xff) as u8) == frame_crc_lo
            && Skymax::fix_crc16_byte((crc >> 8) as u8) == frame_crc_hi
        {
            trace!("crc ok (0x{:04X})", crc);
        } else {
            return Err(format!(
                "crc error in received data, got: 0x{:02X}{:02X}, expected: 0x{:04X}",
                frame_crc_hi, frame_crc_lo, crc
            ));
        }

        //removing starting '(' mark
        data.remove(0);

        //data is now ready for converting to ASCII
        String::from_utf8(data).or(Err("error converting received data to ASCII".to_string()))
    }

    pub async fn query_inverter(
        &mut self,
        mut device: File,
        command: String,
        reply_size: usize,
    ) -> Result<(Option<String>, File)> {
        let mut buffer = vec![0u8; reply_size];
        let mut output_cmd: Vec<u8> = vec![];
        let mut out: Option<String> = None;

        //add main command string
        output_cmd.append(&mut command.clone().into_bytes());
        //calculate xmodem checksum
        let crc = State::<XMODEM>::calculate(output_cmd.as_slice());
        //fix and add checksum
        output_cmd.push(Skymax::fix_crc16_byte((crc >> 8) as u8));
        output_cmd.push(Skymax::fix_crc16_byte((crc & 0xff) as u8));
        //terminate command
        output_cmd.push(0x0d);

        debug!(
            "{}: sending cmd={} crc=0x{:04X} data={:02X?}",
            self.name, command, crc, output_cmd
        );
        if let Err(e) = device.write_all(&output_cmd).await {
            error!("{}: write error: {:?}", self.name, e);
            return Ok((out, device));
        }
        let now = Instant::now();

        let retval = device.read_exact(&mut buffer);
        match timeout(Duration::from_secs(5), retval).await {
            Ok(res) => {
                match res {
                    Ok(n) => {
                        let elapsed = now.elapsed();
                        if n != reply_size {
                            error!("{}: received data is not complete: read {} bytes, expected {} bytes", self.name, n, reply_size);
                        } else {
                            match Skymax::verify_input_data(buffer) {
                                Ok(data) => {
                                    self.poll_ok = self.poll_ok + 1;
                                    info!(
                                        "{}: read {} bytes [⏱  {} ms]: {:?}, ok: {}, errors: {}",
                                        self.name,
                                        n,
                                        (elapsed.as_secs() * 1_000)
                                            + (elapsed.subsec_nanos() / 1_000_000) as u64,
                                        &data,
                                        self.poll_ok,
                                        self.poll_errors
                                    );
                                    out = Some(data);
                                }
                                Err(e) => {
                                    self.poll_errors = self.poll_errors + 1;
                                    error!("{}: data verify failed: {}", self.name, e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("{}: file read error: {}", self.name, e);
                    }
                }
            }
            Err(e) => {
                error!("{}: response timeout: {}", self.name, e);
            }
        }

        Ok((out, device))
    }

    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);
        let mut poll_interval = Instant::now();
        let mut terminated = false;
        let mut inverter_mode: Option<InverterMode> = None;

        loop {
            if terminated || worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            info!("{}: opening device: {:?}", self.name, self.device_path);
            let mut options = OpenOptions::new();
            match options.read(true).write(true).open(&self.device_path).await {
                Ok(mut file) => {
                    info!("{}: device opened", self.name);
                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            debug!("{}: Got terminate signal from main", self.name);
                            terminated = true;
                            break;
                        }

                        if poll_interval.elapsed()
                            > Duration::from_secs_f32(SKYMAX_POLL_INTERVAL_SECS)
                        {
                            poll_interval = Instant::now();

                            //get general status parameters
                            let (buffer, new_handle) =
                                self.query_inverter(file, "QPIGS".into(), 110).await?;
                            file = new_handle;
                            match buffer {
                                Some(data) => {
                                    let params = GeneralStatusParameters::new(data.clone());
                                    match params {
                                        Some(parameters) => {
                                            debug!("{}: {}", self.name, parameters);

                                            //write data to influxdb if configured
                                            match &self.influxdb_url {
                                                Some(url) => {
                                                    // By calling compat on the async function, everything inside it is able
                                                    // to use Tokio 0.2 features.
                                                    let _ = parameters
                                                        .save_to_influxdb(url, &self.name)
                                                        .compat()
                                                        .await;
                                                }
                                                None => (),
                                            }
                                        }
                                        _ => {
                                            error!(
                                                "{}: QPIGS: error parsing values for data: {:02X?}",
                                                self.name, data
                                            );
                                        }
                                    }
                                }
                                None => {
                                    break;
                                }
                            }

                            //get mode
                            let (buffer, new_handle) =
                                self.query_inverter(file, "QMOD".into(), 5).await?;
                            file = new_handle;
                            match buffer {
                                Some(data) => match data.chars().nth(0) {
                                    Some(current_mode) => {
                                        inverter_mode = Some(match inverter_mode {
                                            Some(mut inv_mode) => {
                                                inv_mode.set_new_mode(current_mode, &self.name);
                                                inv_mode
                                            }
                                            None => {
                                                info!(
                                                    "{}: inverter mode: {}",
                                                    self.name,
                                                    InverterMode::get_mode_description(
                                                        current_mode
                                                    )
                                                );
                                                InverterMode {
                                                    last_change: Instant::now(),
                                                    mode: current_mode,
                                                }
                                            }
                                        });
                                    }
                                    None => {
                                        error!("{}: error parsing mode (no input data)", self.name);
                                    }
                                },
                                None => {
                                    break;
                                }
                            }
                        }

                        thread::sleep(Duration::from_millis(30));
                    }
                }
                Err(e) => {
                    error!("{}: error opening device: {:?}", self.name, e);
                    thread::sleep(Duration::from_secs(10));
                    continue;
                }
            }
        }

        info!("{}: Stopping task", self.name);
        Ok(())
    }
}
