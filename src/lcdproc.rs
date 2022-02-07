use simplelog::*;
use std::io::{Error, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

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
    async fn send_command(stream: &mut TcpStream, command: &str) -> Result<bool> {
        stream.write(format!("{}\n", command).as_ref()).await?;
        let mut result = Lcdproc::read_result(stream, false).await?;
        if command == "hello" && result.starts_with("connect ") {
            info!("{}", result);
            return Ok(true);
        } else if result.starts_with("listen ") || result.starts_with("ignore ") {
            //we've got listen/ignore instead of success ... try read result again
            result = Lcdproc::read_result(stream, false).await?;
        }
        if result == "success" {
            Ok(true)
        } else {
            Err(Box::new(Error::new(
                ErrorKind::Other,
                "server doesn't respond with \"success\"",
            )))
        }
    }

    async fn read_result(stream: &mut TcpStream, ignore_timeout: bool) -> Result<String> {
        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        loop {
            match timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
                Ok(result) => match result {
                    Ok(_) => {
                        break;
                    }
                    Err(e) => {
                        return Err(Box::new(e));
                    }
                },
                Err(_) => {
                    if ignore_timeout {
                        return Ok(line);
                    }
                    warn!("lcdproc: LCDd server busy, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
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

    async fn set_emergency_mode(&mut self, stream: &mut TcpStream, enable: bool) -> Result<bool> {
        if enable {
            // blink/flash and set as main screen
            Lcdproc::send_command(stream, "screen_set hard -backlight blink -priority 1").await
        } else {
            // return to normal
            Lcdproc::send_command(stream, "screen_set hard -backlight on -priority 100").await
        }
    }

    async fn refresh_screen(
        &mut self,
        stream: &mut TcpStream,
        line_no: Option<usize>,
    ) -> Result<()> {
        match line_no {
            Some(idx) => {
                if idx == 3 {
                    match self.level {
                        Some(lev) => {
                            Lcdproc::send_command(
                                stream,
                                &format!("widget_set hard cesspool_bar 9 4 {}", 4 * 5 * lev),
                            )
                            .await?;
                        }
                        _ => (),
                    }
                } else {
                    Lcdproc::send_command(
                        stream,
                        &format!(
                            "widget_set hard s{} 1 {} {{{}}}",
                            idx + 1,
                            idx + 1,
                            self.lcd_lines[idx]
                        ),
                    )
                    .await?;
                }
            }
            None => {
                //refresh all data
                for (idx, line) in self.lcd_lines.iter().enumerate() {
                    Lcdproc::send_command(
                        stream,
                        &format!("widget_set hard s{} 1 {} {{{}}}", idx + 1, idx + 1, line),
                    )
                    .await?;
                }
                match self.level {
                    Some(lev) => {
                        Lcdproc::send_command(
                            stream,
                            &format!("widget_set hard cesspool_bar 9 4 {}", 4 * 5 * lev),
                        )
                        .await?;
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

            tokio::time::sleep(Duration::from_secs(12)).await;
            info!("{}: connecting to {}...", self.name, self.lcdproc_host_port);
            match TcpStream::connect(&self.lcdproc_host_port).await {
                Err(e) => {
                    error!(
                        "{}: {} connection error: {:?}",
                        self.name, self.lcdproc_host_port, e
                    );
                    continue;
                }
                Ok(mut stream) => {
                    info!(
                        "📟 {}: connected to server: {}",
                        self.name, self.lcdproc_host_port
                    );

                    debug!("{}: reading pending LcdTasks...", self.name);
                    loop {
                        match self.lcd_receiver.try_recv() {
                            Ok(t) => match t.command {
                                LcdTaskCommand::SetLineText => {
                                    let idx = t.int_arg as usize;
                                    if self.lcd_lines.len() < idx + 1 {
                                        self.lcd_lines.resize(idx + 1, String::new());
                                    }
                                    self.lcd_lines[idx] = t.string_arg.unwrap();
                                }
                                LcdTaskCommand::SetCesspoolLevel => {
                                    self.level = Some(t.int_arg);
                                }
                                _ => (),
                            },
                            _ => {
                                break;
                            }
                        }
                    }

                    if let Err(e) = Lcdproc::send_command(&mut stream, "hello").await {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    //configure/initialize our screen
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "client_set -name {hard_lcd}").await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) = Lcdproc::send_command(&mut stream, "screen_add hard").await {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) = Lcdproc::send_command(
                        &mut stream,
                        "screen_set hard -priority 100 -heartbeat none",
                    )
                    .await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "widget_add hard s1 string").await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "widget_add hard s2 string").await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "widget_add hard s3 string").await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "widget_add hard cesspool_title string")
                            .await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) = Lcdproc::send_command(
                        &mut stream,
                        "widget_set hard cesspool_title 1 4 {c-pool:}",
                    )
                    .await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }
                    if let Err(e) =
                        Lcdproc::send_command(&mut stream, "widget_add hard cesspool_bar hbar")
                            .await
                    {
                        error!("{}: write error: {:?}", self.name, e);
                        continue;
                    }

                    //refreshing whole screen with previous data (if any)
                    if let Err(e) = self.refresh_screen(&mut stream, None).await {
                        error!("{}: refresh_screen error: {:?}", self.name, e);
                        continue;
                    }

                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            debug!("{}: Got terminate signal from main", self.name);
                            terminated = true;
                            break;
                        }

                        //checking for external lcd tasks
                        //fixme: read all tasks, not a single one at a call
                        let task = self.lcd_receiver.try_recv();
                        match task {
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
                                        if let Err(e) =
                                            self.refresh_screen(&mut stream, Some(idx)).await
                                        {
                                            error!("{}: refresh_screen error: {:?}", self.name, e);
                                            break;
                                        }
                                    }
                                    LcdTaskCommand::SetCesspoolLevel => {
                                        self.level = Some(t.int_arg);
                                        if let Err(e) =
                                            self.refresh_screen(&mut stream, Some(3)).await
                                        {
                                            error!("{}: refresh_screen error: {:?}", self.name, e);
                                            break;
                                        }
                                    }
                                    LcdTaskCommand::SetEmergencyMode => {
                                        if let Err(e) = self
                                            .set_emergency_mode(&mut stream, t.int_arg == 1)
                                            .await
                                        {
                                            error!(
                                                "{}: set_emergency_mode error: {:?}",
                                                self.name, e
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            _ => (),
                        }

                        // reading the input data when idle
                        if read_interval.elapsed() > Duration::from_secs_f32(READ_INTERVAL_SECS) {
                            read_interval = Instant::now();

                            match Lcdproc::read_result(&mut stream, true).await {
                                Ok(_) => (),
                                Err(e) => {
                                    error!("{}: read error: {:?}", self.name, e);
                                    break;
                                }
                            };
                        }
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        info!("{}: task stopped", self.name);
        Ok(())
    }
}
