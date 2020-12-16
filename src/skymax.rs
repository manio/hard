use crc16::*;
use futures::io::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::prelude::*;
use tokio::time::timeout;

pub const SKYMAX_POLL_INTERVAL_SECS: f32 = 10.0; //secs between polling

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Skymax {
    pub name: String,
    pub device_path: String,
    pub poll_ok: u64,
    pub poll_errors: u64,
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
                                    //todo
                                    info!("{}: got QPIGS result: {}", self.name, data);
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
                                Some(data) => {
                                    //todo
                                    info!("{}: got QMOD result: {}", self.name, data);
                                }
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
