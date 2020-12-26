use std::io::prelude::*;
use std::io::BufReader;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const READ_INTERVAL_SECS: f32 = 1.0; //secs between reading data from TCP connection when idle

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug)]
pub enum LcdTaskCommand {
    SetLineText,
    SetCesspoolLevel,
    SetEmergencyMode,
}
#[derive(Clone)]
pub struct LcdTask {
    pub command: LcdTaskCommand,
    pub int_arg: u8,
    pub string_arg: Option<String>,
}

pub struct Lcdproc {
    pub name: String,
    pub lcdproc_host_port: String,
    pub lcd_receiver: Receiver<LcdTask>,
    pub lcd_lines: Vec<String>,
    pub level: Option<u8>,
}

impl Lcdproc {
    fn send_command(mut stream: &TcpStream, command: &str) -> std::io::Result<bool> {
        stream.write(format!("{}\n", command).as_ref())?;
        let result = Lcdproc::read_result(stream)?;
        Ok(result == "success")
    }

    fn read_result(stream: &TcpStream) -> std::io::Result<String> {
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone()?);
        reader.read_line(&mut line)?;
        Lcdproc::trim_newline(&mut line);
        Ok(line)
    }

    fn trim_newline(s: &mut String) {
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
    }

    fn set_emergency_mode(&mut self, stream: &TcpStream, enable: bool) {
        if enable {
            // blink/flash and set as main screen
            let _ = Lcdproc::send_command(&stream, "screen_set hard -backlight blink -priority 1");
        } else {
            // return to normal
            let _ = Lcdproc::send_command(&stream, "screen_set hard -backlight on -priority 100");
        }
    }

    fn refresh_screen(
        &mut self,
        stream: &TcpStream,
        line_no: Option<usize>,
    ) -> std::io::Result<()> {
        match line_no {
            Some(idx) => {
                if idx == 3 {
                    match self.level {
                        Some(lev) => {
                            Lcdproc::send_command(
                                &stream,
                                &format!("widget_set hard cesspool_bar 9 4 {}", 4 * 5 * lev),
                            )?;
                        }
                        _ => (),
                    }
                } else {
                    Lcdproc::send_command(
                        &stream,
                        &format!(
                            "widget_set hard s{} 1 {} {{{}}}",
                            idx + 1,
                            idx + 1,
                            self.lcd_lines[idx]
                        ),
                    )?;
                }
            }
            None => {
                //refresh all data
                for (idx, line) in self.lcd_lines.iter().enumerate() {
                    let _ = Lcdproc::send_command(
                        &stream,
                        &format!("widget_set hard s{} 1 {} {{{}}}", idx + 1, idx + 1, line),
                    )?;
                }
                match self.level {
                    Some(lev) => {
                        let _ = Lcdproc::send_command(
                            &stream,
                            &format!("widget_set hard cesspool_bar 9 4 {}", 4 * 5 * lev),
                        )?;
                    }
                    _ => (),
                }
            }
        }

        Ok(())
    }

    pub async fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) -> Result<()> {
        info!("{}: Starting task", self.name);
        let mut read_interval = Instant::now();
        let mut terminated = false;

        loop {
            if terminated || worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            info!("{}: connecting to {}...", self.name, self.lcdproc_host_port);
            match TcpStream::connect(&self.lcdproc_host_port) {
                Err(e) => {
                    error!(
                        "{}: {} connection error: {:?}",
                        self.name, self.lcdproc_host_port, e
                    );
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
                Ok(stream) => {
                    info!(
                        "📟 {}: connected to server: {}",
                        self.name, self.lcdproc_host_port
                    );
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    let _ = Lcdproc::send_command(&stream, "hello");
                    //configure/initialize our screen
                    let _ = Lcdproc::send_command(&stream, "client_set -name {hard_lcd}");
                    let _ = Lcdproc::send_command(&stream, "screen_add hard");
                    let _ = Lcdproc::send_command(
                        &stream,
                        "screen_set hard -priority 100 -heartbeat none",
                    );
                    let _ = Lcdproc::send_command(&stream, "widget_add hard s1 string");
                    let _ = Lcdproc::send_command(&stream, "widget_add hard s2 string");
                    let _ = Lcdproc::send_command(&stream, "widget_add hard s3 string");
                    let _ = Lcdproc::send_command(&stream, "widget_add hard cesspool_title string");
                    let _ = Lcdproc::send_command(
                        &stream,
                        "widget_set hard cesspool_title 1 4 {c-pool:}",
                    );
                    let _ = Lcdproc::send_command(&stream, "widget_add hard cesspool_bar hbar");

                    //refreshing whole screen with previous data (if any)
                    let _ = self.refresh_screen(&stream, None);

                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            debug!("{}: Got terminate signal from main", self.name);
                            terminated = true;
                            break;
                        }

                        //checking for external lcd tasks
                        //fixme: read all tasks, not a single one at a call
                        match self.lcd_receiver.try_recv() {
                            Ok(t) => {
                                debug!(
                                    "{}: received LcdTask: int_arg: {:?}, string_arg: {:?}",
                                    self.name, t.int_arg, t.string_arg
                                );
                                match t.command {
                                    LcdTaskCommand::SetLineText => {
                                        let idx = t.int_arg as usize;
                                        if self.lcd_lines.len() < idx + 1 {
                                            self.lcd_lines.resize(idx + 1, String::new());
                                        }
                                        self.lcd_lines[idx] = t.string_arg.unwrap();
                                        let _ = self.refresh_screen(&stream, Some(idx));
                                    }
                                    LcdTaskCommand::SetCesspoolLevel => {
                                        self.level = Some(t.int_arg);
                                        let _ = self.refresh_screen(&stream, Some(3));
                                    }
                                    LcdTaskCommand::SetEmergencyMode => {
                                        self.set_emergency_mode(&stream, t.int_arg == 1);
                                    }
                                }
                            }
                            _ => (),
                        }

                        // reading the input data when idle
                        if read_interval.elapsed() > Duration::from_secs_f32(READ_INTERVAL_SECS) {
                            read_interval = Instant::now();

                            match Lcdproc::read_result(&stream) {
                                Ok(_) => (),
                                Err(e) => {
                                    if e.kind() != std::io::ErrorKind::WouldBlock {
                                        error!("{}: connection read error: {:?}", self.name, e);
                                        break;
                                    }
                                }
                            };
                        }
                        thread::sleep(Duration::from_millis(30));
                    }
                }
            }
            thread::sleep(Duration::from_millis(30));
        }

        info!("{}: Stopping task", self.name);
        Ok(())
    }
}
