use evdev::Key::*;
use evdev::KEY;
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
    pub fn worker(&self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);

        info!("{}: opening event path: {:?}", self.name, self.event_path);
        let mut d = evdev::Device::open(&self.event_path).unwrap();
        info!("{}: device {:?} opened", self.name, d.name());

        let mut tag_id: String = "".to_string();
        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                debug!("Got terminate signal from main");
                break;
            }

            for ev in d.events_no_sync().unwrap() {
                /* ev.value=1 is for key_down */
                if ev._type == KEY.number::<u16>() && ev.value == 1 {
                    debug!("{}: got event: {:?}", self.name, ev);
                    let mut tag_complete = false;

                    //fixme - fix somehow the following ugly code
                    //cannot do it using 'match' because 'as u16'
                    //cannot be used inside the match statement
                    let mut val = ' ';
                    if ev.code == KEY_0 as u16 {
                        val = '0';
                    } else if ev.code == KEY_1 as u16 {
                        val = '1';
                    } else if ev.code == KEY_2 as u16 {
                        val = '2';
                    } else if ev.code == KEY_3 as u16 {
                        val = '3';
                    } else if ev.code == KEY_4 as u16 {
                        val = '4';
                    } else if ev.code == KEY_5 as u16 {
                        val = '5';
                    } else if ev.code == KEY_6 as u16 {
                        val = '6';
                    } else if ev.code == KEY_7 as u16 {
                        val = '7';
                    } else if ev.code == KEY_8 as u16 {
                        val = '8';
                    } else if ev.code == KEY_9 as u16 {
                        val = '9';
                    } else if ev.code == KEY_ENTER as u16 {
                        tag_complete = true;
                    }

                    if tag_complete {
                        match tag_id.parse::<u32>() {
                            Ok(tag) => {
                                info!("{}: got complete tag ID: {}", self.name, tag);

                                //fixme: don't unwrap and keep read ID locally for a short while
                                let mut rfid_pending_tags = self.rfid_pending_tags.write().unwrap();
                                rfid_pending_tags.push(tag);
                            }
                            Err(e) => {
                                error!("{}: error parsing tag ID {:?}: {:?}", self.name, tag_id, e);
                            }
                        }
                        tag_id.clear();
                    } else {
                        tag_id.push(val);
                    }
                }
            }

            thread::sleep(Duration::from_millis(30));
        }
        info!("{}: Stopping thread", self.name);
    }
}
