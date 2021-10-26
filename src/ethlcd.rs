use simplelog::*;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub const ETHLCD_TCP_PORT: u16 = 2425;
pub const ETHLCD_SET_BEEP: u8 = 0x05;
pub const ETHLCD_BEEPSTATE_ON: u8 = 0x01;
pub const ETHLCD_BEEPSTATE_OFF: u8 = 0x00;

#[derive(Debug)]
pub enum BeepMethod {
    AlarmArming,
    DoorBell,
    Confirmation,
}

pub struct EthLcd {
    pub struct_name: String,
    pub host: String,
    pub in_progress: Arc<AtomicBool>,
}

impl EthLcd {
    fn beep_sequence(
        struct_name: &String,
        hostname: &String,
        mut stream: &TcpStream,
        beep_duration_ms: u64,
        pause_duration_ms: u64,
        repetitions: u8,
        end_pause_ms: u64,
    ) {
        let mut command = vec![];
        for _ in 0..repetitions {
            command.clear();
            command.push(ETHLCD_SET_BEEP);
            command.push(ETHLCD_BEEPSTATE_ON);
            match stream.write_all(&command) {
                Err(e) => {
                    error!(
                        "{} [{}]: cannot write to socket: {:?}",
                        struct_name, hostname, e
                    );
                    return;
                }
                Ok(_) => (),
            }
            thread::sleep(Duration::from_millis(beep_duration_ms));

            command.clear();
            command.push(ETHLCD_SET_BEEP);
            command.push(ETHLCD_BEEPSTATE_OFF);
            match stream.write_all(&command) {
                Err(e) => {
                    error!(
                        "{} [{}]: cannot write to socket: {:?}",
                        struct_name, hostname, e
                    );
                    return;
                }
                Ok(_) => (),
            }
            thread::sleep(Duration::from_millis(pause_duration_ms));
        }
        thread::sleep(Duration::from_millis(end_pause_ms));
    }

    fn beep(
        struct_name: String,
        hostname: String,
        beep_method: BeepMethod,
        in_progress: Arc<AtomicBool>,
    ) {
        debug!("{} [{}]: connecting...", struct_name, hostname);
        match TcpStream::connect(format!("{}:{}", hostname, ETHLCD_TCP_PORT)) {
            Err(e) => {
                error!("{} [{}]: connection error: {:?}", struct_name, hostname, e);
            }
            Ok(stream) => {
                info!(
                    "{} [{}]: 📟 connected, sending beep commands (beep method: {:?})...",
                    struct_name, hostname, beep_method
                );
                match beep_method {
                    BeepMethod::AlarmArming => {
                        //todo
                    }
                    BeepMethod::DoorBell => {
                        for _ in 0..3 {
                            EthLcd::beep_sequence(&struct_name, &hostname, &stream, 400, 300, 1, 0);
                            for _ in 0..3 {
                                EthLcd::beep_sequence(
                                    &struct_name,
                                    &hostname,
                                    &stream,
                                    70,
                                    70,
                                    4,
                                    150,
                                );
                            }
                            EthLcd::beep_sequence(&struct_name, &hostname, &stream, 70, 270, 1, 0);
                        }
                        EthLcd::beep_sequence(&struct_name, &hostname, &stream, 400, 300, 3, 0);
                    }
                    BeepMethod::Confirmation => {
                        EthLcd::beep_sequence(&struct_name, &hostname, &stream, 70, 70, 3, 0);
                    }
                }
            }
        }
        in_progress.store(false, Ordering::SeqCst);
    }

    pub fn async_beep(&mut self, beep_method: BeepMethod) {
        let struct_name = self.struct_name.clone();
        let hostname = self.host.clone();
        let in_progress = self.in_progress.clone();
        if !self.in_progress.load(Ordering::SeqCst) {
            self.in_progress.store(true, Ordering::SeqCst);
            debug!("{} [{}]: starting beep thread...", struct_name, hostname);
            thread::spawn(move || EthLcd::beep(struct_name, hostname, beep_method, in_progress));
        } else {
            error!(
                "{} [{}]: beep in progress, {:?} beep request ignored",
                struct_name, hostname, beep_method
            );
        }
    }
}
