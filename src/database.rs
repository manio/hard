extern crate ini;
extern crate postgres;
extern crate postgres_openssl;

use self::ini::Ini;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::thread;
use std::time::Duration;

pub struct Database {
    pub name: String,
    pub host: Option<String>,
    pub dbname: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Database {
    fn load_db_config(&mut self) {
        let conf = Ini::load_from_file("hard.conf").expect("Cannot open config file");
        let section = conf
            .section(Some("postgres".to_owned()))
            .expect("Cannot find postgres section in config");
        self.host = section.get("host").cloned();
        self.dbname = section.get("dbname").cloned();
        self.username = section.get("username").cloned();
        self.password = section.get("password").cloned();
    }

    pub fn worker(&mut self, worker_cancel_flag: Arc<AtomicBool>) {
        info!("{}: Starting thread", self.name);

        let mut builder =
            SslConnector::builder(SslMethod::tls()).expect("SslConnector::builder error");
        builder.set_verify(SslVerifyMode::NONE); //allow self-signed certificates
        let connector = MakeTlsConnector::new(builder.build());
        let mut conn: Option<postgres::Client> = None;

        loop {
            if worker_cancel_flag.load(Ordering::SeqCst) {
                //todo: check if we have all SQL data flushed
                debug!("{}: Got terminate signal from main", self.name);
                break;
            }

            if conn.is_none() {
                debug!("{}: Loading db config...", self.name);
                self.load_db_config();

                if self.host.is_some()
                    && self.dbname.is_some()
                    && self.username.is_some()
                    && self.password.is_some()
                {
                    let connectionstring = format!(
                        "postgres://{}:{}@{}/{}?sslmode=require&application_name=hard",
                        self.username.as_ref().unwrap(),
                        self.password.as_ref().unwrap(),
                        self.host.as_ref().unwrap(),
                        self.dbname.as_ref().unwrap()
                    )
                    .to_string()
                    .clone();
                    info!("{}: Connecting to: {}", self.name, connectionstring);
                    let client = postgres::Client::connect(&connectionstring, connector.clone());
                    match client {
                        Err(e) => {
                            conn = None;
                            error!("{}: PostgreSQL connection error: {:?}", self.name, e);
                            info!("{}: Trying to reconnect...", self.name);
                        }
                        Ok(c) => {
                            conn = Some(c);
                            info!("{}: Connected successfully", self.name);
                        }
                    }
                } else {
                    debug!(
                        "{}: postgres config is not OK, check the config file",
                        self.name
                    );
                }
            }
            if conn.is_some() {
                debug!("{}: doing queries", self.name);
            }

            thread::sleep(Duration::from_secs(10));
        }
        info!("{}: Stopping thread", self.name);
    }
}
