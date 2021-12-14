use crate::asyncfile::AsyncFile;
use crate::onewire::StateMachine;
use crate::skymax::Skymax;
use chrono::{DateTime, Utc};
use crc16::*;
use influxdb::{Client, InfluxDbWriteable};
use simplelog::*;
use std::fmt;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use termios::*;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tokio_compat_02::FutureExt;

pub const REMEHA_POLL_INTERVAL_SECS: f32 = 5.0; //secs between polling
pub const REMEHA_STATS_DUMP_INTERVAL_SECS: f32 = 3600.0; //secs between showing stats

pub const FRAME_BEGIN: u8 = 0x02;
pub const FRAME_END: u8 = 0x03;

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, InfluxDbWriteable)]
pub struct SampleData {
    time: DateTime<Utc>,

    //recom: group 0: status bytes
    status_code: u8,
    failure_code: u8,
    error_code: u8,
    substatus_code: u8,

    //recom: group 1: temperatures
    flow_temp: f32,
    return_temp: f32,
    calorifier_temp: f32,
    outside_temp: f32,
    control_temp: f32,
    internal_setpoint: f32,
    ch_setpoint: f32,
    dhw_setpoint: f32,
    dhw_in_temp: f32,
    room_temp: f32,
    room_temp_setpoint: f32,
    dhw_setpoint_hmi: u8,
    boiler_control_temp: f32,
    ch_setpoint_hmi: u8,
    solar_temp: f32,

    //recom: group 2
    airflow_setpoint: u16,
    airflow: u16,
    ionisation_current: f32,
    pump_power: u8,
    hydr_pressure: f32,
    dhw_flow: f32,
    actual_power: u8,
    available_power: u8,
    required_output: u8,
}

impl SampleData {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            time: Utc::now(),
            status_code: data[40],
            failure_code: data[41],
            error_code: data[42],
            substatus_code: data[43],

            flow_temp: (((data[1] as u16) << 8) + data[0] as u16) as f32 / 100.0,
            return_temp: (((data[3] as u16) << 8) + data[2] as u16) as f32 / 100.0,
            calorifier_temp: (((data[9] as u16) << 8) + data[8] as u16) as f32 / 100.0,
            outside_temp: (((data[7] as u16) << 8) + data[6] as u16) as f32 / 100.0,
            control_temp: (((data[52] as u16) << 8) + data[51] as u16) as f32 / 100.0,
            internal_setpoint: (((data[28] as u16) << 8) + data[27] as u16) as f32 / 100.0,
            ch_setpoint: (((data[17] as u16) << 8) + data[16] as u16) as f32 / 100.0,
            dhw_setpoint: (((data[19] as u16) << 8) + data[18] as u16) as f32 / 100.0,
            dhw_in_temp: (((data[5] as u16) << 8) + data[4] as u16) as f32 / 100.0,
            room_temp: (((data[15] as u16) << 8) + data[14] as u16) as f32 / 100.0,
            room_temp_setpoint: (((data[21] as u16) << 8) + data[20] as u16) as f32 / 100.0,
            dhw_setpoint_hmi: data[61],
            boiler_control_temp: (((data[13] as u16) << 8) + data[12] as u16) as f32 / 100.0,
            ch_setpoint_hmi: data[60],
            solar_temp: (((data[57] as u16) << 8) + data[56] as u16) as f32 / 100.0,

            airflow_setpoint: ((data[23] as u16) << 8) + data[22] as u16,
            airflow: ((data[25] as u16) << 8) + data[24] as u16,
            ionisation_current: data[26] as f32 / 10.0,
            pump_power: data[30],
            hydr_pressure: data[49] as f32 / 10.0,
            dhw_flow: (((data[54] as u16) << 8) + data[53] as u16) as f32 / 100.0,
            actual_power: data[33],
            available_power: data[29],
            required_output: data[32],
        }
    }

    fn get_status_code_description(code: u8) -> &'static str {
        match code {
            0 => "Standby 💤",
            1 => "Boiler start",
            2 => "Burner start 🕯️",
            3 => "Burning CH 🔥 ◾ central heating 🛖",
            4 => "Burning DHW 🔥 ◾ domestic hot water 🚰",
            5 => "Burner stop",
            6 => "Boiler stop 🎯",
            8 => "Controlled stop",
            9 => "Blocking mode",
            10 => "Locking mode",
            11 => "Chimney mode L",
            12 => "Chimney mode h",
            13 => "Chimney mode H",
            15 => "Manual-heatdemand",
            16 => "Boiler-frost-protection",
            17 => "De-airation",
            18 => "Controller temp protection",
            _ => "Unknown State",
        }
    }

    fn get_substatus_code_description(code: u8) -> &'static str {
        match code {
            0 => "Standby",
            1 => "Anti-cycling",
            2 => "Open hydraulic valve",
            3 => "Pump start",
            4 => "Wait for burner start",
            10 => "Open external gas valve",
            11 => "Fan to fluegasvalve speed",
            12 => "Open fluegasvalve",
            13 => "Pre-purge",
            14 => "Wait for release",
            15 => "Burner start",
            16 => "VPS test",
            17 => "Pre-ignition",
            18 => "Ignition",
            19 => "Flame check",
            20 => "Interpurge",
            30 => "Normal internal setpoint",
            31 => "Limited internal setpoint",
            32 => "Normal power control",
            33 => "Gradient control level 1",
            34 => "Gradient control level 2",
            35 => "Gradient control level 3",
            36 => "Flame protection",
            37 => "Stabilization time",
            38 => "Cold start",
            39 => "Limited power Tfg",
            40 => "Burner stop",
            41 => "Post purge",
            42 => "Fan to fluegasvalve speed",
            43 => "Close fluegasvalve",
            44 => "Stop fan",
            45 => "Close external gas valve",
            60 => "Pump post running",
            61 => "Pump stop",
            62 => "Close hydraulic valve",
            63 => "Start anti-cycle timer",
            255 => "Reset wait time",
            _ => "Unknown Sub-State",
        }
    }

    fn get_failure_code_description(code: u8) -> &'static str {
        match code {
            0 => "PSU not connected (Locking 0)",
            1 => "SU parameter fault (Locking 1)",
            2 => "02:T Flow(/HeatExch.) closed",
            3 => "03:T Flow(/HeatExch.) open",
            4 => "04:T Flow(/HeatExch.) < min.",
            5 => "05:T Flow(/HeatExch.) > max.",
            6 => "T Return closed (Locking 6)",
            7 => "T Return open (Locking 7)",
            8 => "T Return < min. (Locking 8)",
            9 => "T Return > max. (Locking 9)",
            10 => "10:dT(Flow(/HeatExch),Return) > max.",
            11 => "11:dT(Return,Flow(/HeatExch)) > max.",
            12 => "STB activated (Locking 12)",
            14 => "5x Unsuccessful start (Locking 14)",
            15 => "5x VPS test failure (Locking 15)",
            16 => "False flame (Locking 16)",
            17 => "SU Gasvalve driver error (Locking 17)",
            32 => "T Flow closed (Locking 32)",
            33 => "T Flow open (Locking 33)",
            34 => "Fan out of control range(/De-air test failed) (Locking 34)",
            35 => "Return over Flow temp. (Locking 35)",
            36 => "5x Flame loss (Locking 36)",
            37 => "SU communication (Locking 37)",
            38 => "SCU-S communication (Locking 38)",
            39 => "BL input as lockout (Locking 39)",
            41 => "E11: Airbox(/PCB) temp. > max.",
            42 => "Low water pressure (Locking 42)",
            43 => "No gradient (Locking 43)",
            50 => "External PSU timeout (Locking 50)",
            51 => "Onboard PSU timeout (Locking 51)",
            52 => "GVC lockout (Locking 52)",
            255 => "No locking",
            _ => "Unknown locking code",
        }
    }

    fn get_error_code_description(code: u8) -> &'static str {
        match code {
            0 => "PCU parameter fault (Blocking 0)",
            1 => "T Flow > max.(Blocking 1)",
            2 => "dT/s Flow > max. (Blocking 2)",
            3 => "T HeatExch > max.(Blocking 3)",
            4 => "dT/s HeatExch > max.(Blocking 4)",
            5 => "dT(heatExch,Return) > max. (Blocking 5)",
            6 => "dT(Flow,HeatExch) > max.(Blocking 6)",
            7 => "dT(Flow,Return) > max.(Blocking 7)",
            8 => "No release signal(Blocking 8)",
            9 => "L-N swept(Blocking 9)",
            10 => "Blocking signal ex frost(Blocking 10)",
            11 => "Blocking signal inc frost(Blocking 11)",
            12 => "HMI not connected(Blocking 12)",
            13 => "SCU communication(Blocking 13)",
            14 => "Min. water pressure(Blocking 14)",
            15 => "Min. gas pressure(Blocking 15)",
            16 => "Ident. SU mismatch(Blocking 16)",
            17 => "Ident. dF/dU table error(Blocking 17)",
            18 => "Ident. PSU mismatch(Blocking 18)",
            19 => "Ident. dF/dU needed(Blocking 19)",
            20 => "Identification running(Blocking 20)",
            21 => "SU communications lost(Blocking 21)",
            22 => "Flame lost(Blocking 22)",
            24 => "VPS test failed(Blocking 24)",
            25 => "Internal SU error(Blocking 25)",
            26 => "Calorifier sensor error(Blocking 26)",
            27 => "DHW in sensor error(Blocking 27)",
            28 => "Reset in progress...(Blocking 28)",
            29 => "GVC parameter changed(Blocking 29)",
            31 => "31:-Flue gas temp limit exceeded",
            32 => "32:-Flue gas sensor error",
            33 => "33:-Internal PCU fault",
            34 => "34:-Diff between Tfg1 and Tfg2",
            35 => "35:-Flue gas temp 5* burner stop",
            36 => "36:-Flow temp 5* burner stop",
            41 => "41: Dt (Tf,Tr)  deair failed",
            43 => "43:Grad. low at burnerstart",
            44 => "44: DeltaT (Tf, Tr) too high",
            45 => "45: Air pressure too high",
            255 => "No blocking",
            _ => "Unknown blocking code",
        }
    }

    async fn save_to_influxdb(&self, influxdb_url: &String, display_name: &String) -> Result<()> {
        // connect to influxdb
        let client = Client::new(influxdb_url, "remeha");

        match client.query(&self.clone().into_query("sample_data")).await {
            Ok(msg) => {
                debug!("{} influxdb write success: {:?}", display_name, msg);
            }
            Err(e) => {
                error!("{} influxdb write error: {:?}", display_name, e);
            }
        }

        Ok(())
    }
}

impl fmt::Display for SampleData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "sample data:\n")?;
        write!(f, "----------------------------------------------------\n")?;
        write!(
            f,
            "Status: {}: {}\n",
            self.status_code,
            SampleData::get_status_code_description(self.status_code)
        )?;
        write!(
            f,
            "Substatus: {}: {}\n",
            self.substatus_code,
            SampleData::get_substatus_code_description(self.substatus_code)
        )?;
        write!(
            f,
            "Failure/Locking: {}: {}\n",
            self.failure_code,
            SampleData::get_failure_code_description(self.failure_code)
        )?;
        write!(
            f,
            "Error/Blocking: {}: {}\n",
            self.error_code,
            SampleData::get_error_code_description(self.error_code)
        )?;
        write!(f, "\n")?;
        write!(f, "Flow temp.: {} °C\n", self.flow_temp)?;
        write!(f, "Return temp.: {} °C\n", self.return_temp)?;
        write!(f, "Calorifier temp.: {} °C\n", self.calorifier_temp)?;
        write!(f, "Outside temp.: {} °C\n", self.outside_temp)?;
        write!(f, "Control temp.: {} °C\n", self.control_temp)?;
        write!(f, "Internal setpoint: {} °C\n", self.internal_setpoint)?;
        write!(f, "CH setpoint: {} °C\n", self.ch_setpoint)?;
        write!(f, "DHW setpoint: {} °C\n", self.dhw_setpoint)?;
        write!(f, "DHW-in temp.: {} °C\n", self.dhw_in_temp)?;
        write!(f, "Room temp.: {} °C\n", self.room_temp)?;
        write!(f, "Room temp setpoint: {} °C\n", self.room_temp_setpoint)?;
        write!(f, "DHW setpoint HMI: {} °C\n", self.dhw_setpoint_hmi)?;
        write!(f, "Boiler Control temp.: {} °C\n", self.boiler_control_temp)?;
        write!(f, "CH setpoint HMI: {} °C\n", self.ch_setpoint_hmi)?;
        write!(f, "Solar. Temp.: {} °C\n", self.solar_temp)?;
        write!(f, "\n")?;
        write!(f, "Airflow setpoint: {}\n", self.airflow_setpoint)?;
        write!(f, "Airflow: {}\n", self.airflow)?;
        write!(f, "Ionisation current: {} uA\n", self.ionisation_current)?;
        write!(f, "Pump: {} %\n", self.pump_power)?;
        write!(f, "Hydr pressure: {} bar\n", self.hydr_pressure)?;
        write!(f, "DHW Flow: {} l/min\n", self.dhw_flow)?;
        write!(f, "Actual power: {} %\n", self.actual_power)?;
        write!(f, "Available power: {} %\n", self.available_power)?;
        write!(f, "Required output: {} %\n", self.required_output)?;

        Ok(())
    }
}

pub struct RemehaState {
    pub status_code: u8,
    pub failure_code: u8,
    pub error_code: u8,
    pub substatus_code: u8,
}

impl RemehaState {
    fn set_new_status(
        &mut self,
        display_name: &String,
        status_code: u8,
        substatus_code: u8,
        failure_code: u8,
        error_code: u8,
    ) -> bool {
        let mut failure = false;
        if self.status_code != status_code {
            self.status_code = status_code;
            info!(
                "{} Status: <blue>{}:</><i>{}</>",
                display_name,
                self.status_code,
                SampleData::get_status_code_description(self.status_code),
            );
        }
        if self.substatus_code != substatus_code {
            self.substatus_code = substatus_code;
            info!(
                "{} Substatus: <blue>{}:</><i>{}</>",
                display_name,
                self.substatus_code,
                SampleData::get_substatus_code_description(self.substatus_code),
            );
        }
        if self.failure_code != failure_code {
            self.failure_code = failure_code;
            info!(
                "{} Failure/Locking: <blue>{}:</><i>{}</>",
                display_name,
                self.failure_code,
                SampleData::get_failure_code_description(self.failure_code),
            );
            failure = true;
        }
        if self.error_code != error_code {
            self.error_code = error_code;
            info!(
                "{} Error/Blocking: <blue>{}:</><i>{}</>",
                display_name,
                self.error_code,
                SampleData::get_error_code_description(self.error_code),
            );
            failure = true;
        }
        failure
    }

    fn show_status(&self, display_name: &String) {
        info!(
            "{} Status: <blue>{}:</><i>{}</>",
            display_name,
            self.status_code,
            SampleData::get_status_code_description(self.status_code),
        );
        info!(
            "{} Substatus: <blue>{}:</><i>{}</>",
            display_name,
            self.substatus_code,
            SampleData::get_substatus_code_description(self.substatus_code),
        );
        info!(
            "{} Failure/Locking: <blue>{}:</><i>{}</>",
            display_name,
            self.failure_code,
            SampleData::get_failure_code_description(self.failure_code),
        );
        info!(
            "{} Error/Blocking: <blue>{}:</><i>{}</>",
            display_name,
            self.error_code,
            SampleData::get_error_code_description(self.error_code),
        );
    }
}

pub struct Remeha {
    pub display_name: String,
    pub device_path: String,
    pub poll_ok: u64,
    pub poll_errors: u64,
    pub influxdb_url: Option<String>,
    pub state_change_script: Option<String>,
}

impl Remeha {
    fn verify_input_data(mut data: Vec<u8>) -> std::result::Result<(), String> {
        debug!("input data={:02X?}", data);

        //check start sequence
        if data.remove(0) != FRAME_BEGIN {
            return Err("incorrect start sequence in received data".to_string());
        }
        //check for stop sequence
        if data.pop().unwrap() != FRAME_END {
            return Err("received data is not properly terminated".to_string());
        }

        //check for frame length
        if data[3] as usize != data.len() {
            return Err("received data has invalid length".to_string());
        }

        //get crc from data
        let frame_crc = ((data.pop().unwrap() as u16) << 8) + data.pop().unwrap() as u16;

        //calculate modbus checksum
        let crc = State::<MODBUS>::calculate(data.as_slice());

        //compare checksum
        if crc == frame_crc {
            trace!("crc ok (0x{:04X})", crc);
        } else {
            return Err(format!(
                "crc error in received data, got: 0x{:04X}, expected: 0x{:04X}",
                frame_crc, crc
            ));
        };

        Ok(())
    }

    pub async fn query_boiler(
        &mut self,
        mut device: AsyncFile,
        function_code: u16,
        data: u16,
        reply_size: usize,
    ) -> io::Result<(Option<Vec<u8>>, AsyncFile)> {
        let mut buffer = vec![0u8; reply_size];
        let mut output_cmd: Vec<u8> = vec![];
        let mut out: Option<Vec<u8>> = None;

        //the protocol looks like a modbus
        output_cmd.push(0xfe); //slave ID?
        output_cmd.push((function_code >> 8) as u8); //function code?
        output_cmd.push((function_code & 0xff) as u8); //function code?
        output_cmd.push(0x00); //here will be frame length (with crc) and without 2 start/end bytes
        output_cmd.push((data >> 8) as u8); //data?
        output_cmd.push((data & 0xff) as u8); //data?
        output_cmd[3] = (output_cmd.len() + 2) as u8; //set a frame length

        //calculate and add modbus checksum
        let crc = State::<MODBUS>::calculate(output_cmd.as_slice());
        output_cmd.push((crc & 0xff) as u8);
        output_cmd.push((crc >> 8) as u8);

        //start and terminate frame
        output_cmd.insert(0, FRAME_BEGIN);
        output_cmd.push(FRAME_END);

        debug!(
            "{} sending function_code={:04x} data={:04x} crc=0x{:04X} frame={:02X?}",
            self.display_name, function_code, data, crc, output_cmd
        );
        if let Err(e) = device.write_all(&output_cmd).await {
            error!("{} write error: {:?}", self.display_name, e);
            return Ok((out, device));
        }
        let now = Instant::now();

        let retval = device.read_exact(&mut buffer);
        match timeout(Duration::from_secs_f32(2.5), retval).await {
            Ok(res) => match res {
                Ok(_) => {
                    let elapsed = now.elapsed();
                    match Remeha::verify_input_data(buffer.clone()) {
                        Ok(_) => {
                            self.poll_ok = self.poll_ok + 1;
                            debug!(
                                "{} got reply [⏱️ {} ms]: {:02X?}, ok: {}, errors: {}",
                                self.display_name,
                                (elapsed.as_secs() * 1_000)
                                    + (elapsed.subsec_nanos() / 1_000_000) as u64,
                                &buffer,
                                self.poll_ok,
                                self.poll_errors
                            );
                            out = Some(buffer)
                        }
                        Err(e) => {
                            self.poll_errors = self.poll_errors + 1;
                            error!("{} data verify failed: {}", self.display_name, e);
                        }
                    }
                }
                Err(e) => {
                    error!("{} file read error: {}", self.display_name, e);
                }
            },
            Err(e) => {
                error!("{} response timeout: {}", self.display_name, e);
            }
        }

        Ok((out, device))
    }

    fn get_device_path(&self) -> Result<String> {
        //get the tty device name, like 'ttyACM0'
        let dev_name = Skymax::get_first_dir(self.device_path.clone())?;

        //create the full /dev/ path with obtained filename
        let mut full_path = "/dev/".to_string();
        full_path.push_str(&dev_name);

        Ok(full_path)
    }

    fn setup_fd(fd: RawFd) -> io::Result<()> {
        let mut termios = Termios::from_fd(fd)?;
        cfmakeraw(&mut termios);
        tcsetattr(fd, TCSANOW, &termios)?;
        tcflush(fd, TCIOFLUSH)?;
        Ok(())
    }

    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{} Starting task", self.display_name);
        let mut poll_interval = Instant::now();
        let mut stats_interval = Instant::now();
        let mut terminated = false;
        let mut remeha_state: Option<RemehaState> = None;

        loop {
            if terminated || worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            //obtain device path from sysfs
            let device_path = match self.get_device_path() {
                Ok(path) => path,
                Err(e) => {
                    error!(
                        "{} unable to obtain device path: {:?}",
                        self.display_name, e
                    );
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            info!(
                "{} opening device: {:?}, obtained from physical path: {:?}",
                self.display_name, device_path, self.device_path
            );
            let mut options = OpenOptions::new();
            let future = options.read(true).write(true).open(&device_path);
            match timeout(Duration::from_secs(5), future).await {
                Ok(res) => {
                    match res {
                        Ok(f) => {
                            info!(
                                "{} device opened, poll interval: {}s",
                                self.display_name, REMEHA_POLL_INTERVAL_SECS
                            );

                            //call cfmakeraw on a fd termios struct
                            //to enable raw mode
                            if let Err(e) = Remeha::setup_fd(f.as_raw_fd()) {
                                error!(
                                    "{} error calling cfmakeraw() on fd: {:?}",
                                    self.display_name, e
                                );
                                tokio::time::sleep(Duration::from_secs(10)).await;
                                continue;
                            }

                            //create a AsyncFd object on file
                            match AsyncFile::new(f) {
                                Err(e) => {
                                    error!("{} error creating AsyncFd: {:?}", self.display_name, e);
                                    tokio::time::sleep(Duration::from_secs(10)).await;
                                    continue;
                                }
                                Ok(mut file) => {
                                    loop {
                                        if worker_cancel_flag.load(Ordering::SeqCst) {
                                            debug!(
                                                "{} Got terminate signal from main",
                                                self.display_name
                                            );
                                            terminated = true;
                                        }

                                        if terminated
                                            || stats_interval.elapsed()
                                                > Duration::from_secs_f32(
                                                    REMEHA_STATS_DUMP_INTERVAL_SECS,
                                                )
                                        {
                                            stats_interval = Instant::now();
                                            info!(
                                                "{} 📊 boiler query statistics: ok: {}, errors: {}",
                                                self.display_name, self.poll_ok, self.poll_errors
                                            );

                                            if terminated {
                                                break;
                                            }
                                        }

                                        if poll_interval.elapsed()
                                            > Duration::from_secs_f32(REMEHA_POLL_INTERVAL_SECS)
                                        {
                                            poll_interval = Instant::now();

                                            //query for sample data
                                            let (buffer, new_handle) =
                                                self.query_boiler(file, 0x105, 0x201, 74).await?;
                                            file = new_handle;
                                            match buffer {
                                                Some(mut data) => {
                                                    //remove protocol overhead bytes:
                                                    data.drain(0..=6);

                                                    //parse data
                                                    let sample = SampleData::new(data);
                                                    debug!("{} {}", self.display_name, sample);

                                                    //write data to influxdb if configured
                                                    match &self.influxdb_url {
                                                        Some(url) => {
                                                            // By calling compat on the async function, everything inside it is able
                                                            // to use Tokio 0.2 features.
                                                            let _ = sample
                                                                .save_to_influxdb(
                                                                    url,
                                                                    &self.display_name,
                                                                )
                                                                .compat()
                                                                .await;
                                                        }
                                                        None => (),
                                                    }

                                                    remeha_state = Some(match remeha_state {
                                                        Some(mut current_state) => {
                                                            if current_state.set_new_status(
                                                                &self.display_name,
                                                                sample.status_code,
                                                                sample.substatus_code,
                                                                sample.failure_code,
                                                                sample.error_code,
                                                            ) && (sample.failure_code != 255
                                                                || sample.error_code != 255)
                                                            {
                                                                // run a shell script when mode has changed
                                                                // and we have failure or error
                                                                match &self.state_change_script {
                                                                    Some(command) => {
                                                                        let mut cmd = command
                                                                            .to_string()
                                                                            .clone();
                                                                        cmd = str::replace(
                                                                            &cmd,
                                                                            "%state%",
                                                                            &format!(
                                                                                "{}{}",
                                                                                {
                                                                                    if sample.failure_code
                                                                                != 255
                                                                            {
                                                                                format!("\nFailure/Locking: {}: {}",
                                                                                        sample.failure_code,
                                                                                        SampleData::get_failure_code_description(sample.failure_code),
                                                                                )
                                                                            } else {
                                                                                "".to_string()
                                                                            }
                                                                                },
                                                                                {
                                                                                    if sample
                                                                                        .error_code
                                                                                        != 255
                                                                                    {
                                                                                        format!("\nError/Blocking: {}: {}",
                                                                                        sample.error_code,
                                                                                        SampleData::get_error_code_description(sample.error_code),
                                                                                )
                                                                                    } else {
                                                                                        "".to_string()
                                                                                    }
                                                                                },
                                                                            ),
                                                                        );
                                                                        thread::spawn(move || {
                                                                            StateMachine::run_shell_command(
                                                                        cmd,
                                                                    )
                                                                        });
                                                                    }
                                                                    _ => (),
                                                                };
                                                            }
                                                            current_state
                                                        }
                                                        None => {
                                                            let new_state = RemehaState {
                                                                status_code: sample.status_code,
                                                                substatus_code: sample
                                                                    .substatus_code,
                                                                failure_code: sample.failure_code,
                                                                error_code: sample.error_code,
                                                            };
                                                            new_state
                                                                .show_status(&self.display_name);
                                                            new_state
                                                        }
                                                    });
                                                }
                                                None => {
                                                    break;
                                                }
                                            }
                                        }

                                        tokio::time::sleep(Duration::from_millis(30)).await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("{} error opening device: {:?}", self.display_name, e);
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    error!("{} file open timeout: {}", self.display_name, e);
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        info!("{} task stopped", self.display_name);
        Ok(())
    }
}
