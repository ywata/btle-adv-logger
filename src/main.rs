mod datastore;
mod ds_sqlite;

//use std::fmt::Error;
use crate::ds_sqlite::SqliteAdStore;
use clap::ValueEnum;
use futures::stream::StreamExt;
use serde::Deserialize;
use std::fmt::Debug;
use std::str::FromStr;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tokio::{fs, time};
use uuid::Uuid;

use clap::{Parser, Subcommand};
use serde_yaml;

use btleplug::api::{
    Central, CentralEvent, Manager as _, ScanFilter,
};
use btleplug::platform::{Manager, PeripheralId};

use chrono::{DateTime, Utc};


use datastore::{AdStore, AdStoreError};

use rusqlite::{Result};


#[derive(Debug, Parser)]
#[command(about = "BLE inspection tool", long_about = None)]
struct Cli {
    #[arg(long, default_value = "10")]
    scan_duration_sec: u64,

    #[arg(long)]
    uuid_file: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq, Hash, ValueEnum)]
enum MessageType {
    ManufacturerDataAdvertisement,
    ServiceAdvertisement,
    ServiceDataAdvertisement,
}

#[derive(Subcommand, Clone, Debug)]
enum Command {
    Monitor { file: String },
    InitDb { file: String },
    Load { file: String },
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
enum Cmd {
    Str { cmd: String },
    Hex { cmd: Vec<u8> },
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct Request {
    device: String,
    cmd: Cmd,
}

impl Request {
    fn normalize(&self) -> Option<Self> {
        match &self.cmd {
            Cmd::Str { cmd } => {
                if let Some(hex) = cmd
                    .split_whitespace()
                    .map(|hex| u8::from_str_radix(hex, 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .ok()
                {
                    let mut r = self.clone();
                    r.cmd = Cmd::Hex { cmd: hex };

                    Some(r)
                } else {
                    None
                }
            }
            _ => Some(self.clone()),
        }
    }
    fn to_bytes(&self) -> Vec<u8> {
        match &self.cmd {
            Cmd::Hex { cmd } => cmd.clone(),
            _ => Vec::new(),
        }
    }
}



fn create_scan_filter() -> ScanFilter {
    ScanFilter {
        services: vec![],
    }
}

fn get_peripheral_id(
    event: &CentralEvent,
) -> Option<PeripheralId> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement {
            id,
            ..
        } => {
            return Some(id.clone());
        }
        CentralEvent::ServicesAdvertisement { id, .. } => {
            return Some(id.clone());
        }
        CentralEvent::ServiceDataAdvertisement { id, .. } => {
            return Some(id.clone());
        }
        CentralEvent::DeviceDiscovered(id)
        | CentralEvent::DeviceConnected(id)
        | CentralEvent::DeviceDisconnected(id)
        | CentralEvent::DeviceUpdated(id) => {
            return Some(id.clone());
        }
        CentralEvent::StateUpdate(_central_state) => {}
    }
    None
}

fn get_message_type(event: &CentralEvent) -> Option<MessageType> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { .. } => {
            Some(MessageType::ManufacturerDataAdvertisement)
        }
        CentralEvent::ServicesAdvertisement { .. } => Some(MessageType::ServiceAdvertisement),
        CentralEvent::ServiceDataAdvertisement { .. } => {
            Some(MessageType::ServiceDataAdvertisement)
        }
        _ => None,
    }
}

async fn monitor(
    manager: &Manager,
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }

    let scan_filter = ScanFilter::default();
    let central = adapter_list.into_iter().nth(0).unwrap();

    central.start_scan(scan_filter).await?;
    let mut events = central.events().await?;
    while let Some(event) = events.next().await {
        let mut records_lock = event_records.write().await;
        let utc_now = Utc::now();
        records_lock.push((utc_now, event));
    }
    Ok(())
}

pub async fn save_events(
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    ad_store: Arc<dyn AdStore>,
) -> Result<(), Box<AdStoreError>> {
    let mut interval = time::interval(Duration::from_secs(1));

    loop {
        interval.tick().await;

        let mut records_lock = event_records.write().await;
        while let Some((_, event)) = records_lock.pop() {
            ad_store.store_event(&event)?;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pretty_env_logger::init();
    let cli = Cli::parse();
    let manager = Manager::new().await?;

    match cli.command {
        Command::Monitor { ref file } => {
            let event_records = Arc::new(RwLock::new(Vec::new()));
            let ad_store = Arc::new(SqliteAdStore::new(file)?);

            // Initialize the database
            ad_store.init()?;

            // Run monitor and save_events concurrently
            tokio::try_join!(
                monitor(&manager, event_records.clone()),
                save_events(event_records, ad_store)
            )?;
        }

        Command::Load { .. } => {}
        Command::InitDb { file } => {
            let ad_store = Arc::new(Box::new(SqliteAdStore::new(&file)?));
            ad_store.init()?;
        }
    }

    Ok(())
}


