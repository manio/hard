use evdev::Key;
use simplelog::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

pub struct RfidTag {
    pub id_tag: i32,
    pub name: String,
    pub tags: Vec<String>,
    pub associated_relays: Vec<i32>,
}

pub struct Rfid {
    pub name: String,
    pub event_path: String,
    pub rfid_pending_tags: Arc<RwLock<Vec<u32>>>,
}

impl Rfid {
    pub fn push_tag_upstream(&self, tag: u32) -> bool {
        match self.rfid_pending_tags.write() {
            Ok(mut rfid_pending_tags) => {
                rfid_pending_tags.push(tag);
                true
            }
            Err(_) => false,
        }
    }
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);
        let mut terminated = false;

        loop {
            if terminated || worker_cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            info!(
                "{}: trying to open device with physical path: {:?}",
                self.name, self.event_path
            );
            let dev = evdev::enumerate().into_iter().find(|x| {
                x.physical_path().is_some()
                    && (x
                        .physical_path()
                        .as_ref()
                        .unwrap()
                        .to_string())
                        == self.event_path
            });

            match dev {
                Some(mut d) => {
                    info!("{}: device {:?} opened", self.name, d.name());
                    let mut tag_id: String = "".to_string();
                    let mut local_pending_tags: Vec<u32> = vec![];
                    loop {
                        if worker_cancel_flag.load(Ordering::SeqCst) {
                            debug!("Got terminate signal from main");
                            terminated = true;
                            break;
                        }

                        match d.fetch_events() {
                            Ok(events) => {
                                for ev in events {
                                    /* ev.value=1 is for key_down */
                                    if ev.event_type() == evdev::EventType::KEY && ev.value() == 1 {
                                        debug!("{}: got event: {:?}", self.name, ev);
                                        let mut tag_complete = false;

                                        //fixme - fix somehow the following ugly code
                                        //cannot do it using 'match' because 'as u16'
                                        //cannot be used inside the match statement
                                        let mut val = ' ';
                                        if ev.code() == Key::KEY_0.code() {
                                            val = '0';
                                        } else if ev.code() == Key::KEY_1.code() {
                                            val = '1';
                                        } else if ev.code() == Key::KEY_2.code() {
                                            val = '2';
                                        } else if ev.code() == Key::KEY_3.code() {
                                            val = '3';
                                        } else if ev.code() == Key::KEY_4.code() {
                                            val = '4';
                                        } else if ev.code() == Key::KEY_5.code() {
                                            val = '5';
                                        } else if ev.code() == Key::KEY_6.code() {
                                            val = '6';
                                        } else if ev.code() == Key::KEY_7.code() {
                                            val = '7';
                                        } else if ev.code() == Key::KEY_8.code() {
                                            val = '8';
                                        } else if ev.code() == Key::KEY_9.code() {
                                            val = '9';
                                        } else if ev.code() == Key::KEY_ENTER.code() {
                                            tag_complete = true;
                                        }

                                        if tag_complete {
                                            match tag_id.parse::<u32>() {
                                                Ok(tag) => {
                                                    info!(
                                                        "{}: 🏷️ got complete tag ID: {}",
                                                        self.name, tag
                                                    );

                                                    if !self.push_tag_upstream(tag) {
                                                        //unable to obtain a write lock, keep it locally
                                                        local_pending_tags.push(tag);
                                                    }
                                                }
                                                Err(e) => {
                                                    error!(
                                                        "{}: error parsing tag ID {:?}: {:?}",
                                                        self.name, tag_id, e
                                                    );
                                                }
                                            }
                                            tag_id.clear();
                                        } else {
                                            tag_id.push(val);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("{}: error processing events: {:?}", self.name, e);
                                break;
                            }
                        }

                        //if there was a problem to push a tag, try again now
                        match local_pending_tags.pop() {
                            Some(tag) => {
                                if !self.push_tag_upstream(tag) {
                                    //still unable to obtain a write lock, re-push
                                    local_pending_tags.push(tag);
                                } else {
                                    warn!("{}: delayed process of tag ID: {}", self.name, tag);
                                }
                            }
                            _ => {}
                        }

                        thread::sleep(Duration::from_millis(30));
                    }
                }
                None => {
                    error!("{}: device not found", self.name);
                    thread::sleep(Duration::from_secs(10));
                }
            }
        }
        info!("{}: thread stopped", self.name);
    }
}
