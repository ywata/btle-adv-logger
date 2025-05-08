mod datastore;
mod ds_sqlite;

//use std::fmt::Error;
use crate::ds_sqlite::SqliteAdStore;
use btleplug::api::CharPropFlags;
use clap::ValueEnum;
use futures::stream::StreamExt;
use nix::unistd::Pid;
use serde::Deserialize;
use std::collections::HashSet;
use std::error::Error;
use std::fmt::Debug;
use std::str::FromStr;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::{fs, signal, time};
use uuid::Uuid;

use clap::{Parser, Subcommand};
use serde_yaml;

use btleplug::api::CentralEvent::ServicesAdvertisement;
use btleplug::api::{
    Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral, ScanFilter,
    WriteType,
};
use btleplug::platform::{Manager, PeripheralId};
use pretty_env_logger::env_logger::Target;

use chrono::{DateTime, TimeDelta, Utc};
use nix::sys::signal as nix_signal;

use std::fs::File;
use std::io::{Read, Write};

use datastore::{AdStore, AdStoreError};

use rusqlite::{params, Connection, Result};

use tokio::sync::mpsc;

use log::{debug, error, info, trace, warn};

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

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct TargetUuid<T> {
    service_uuid: T,
    peripheral_uuids: Vec<T>,
    requests: HashMap<String, Vec<Request>>,
}

impl TargetUuid<String> {
    fn into_uuid(self) -> Result<TargetUuid<Uuid>, uuid::Error> {
        Ok(TargetUuid {
            service_uuid: Uuid::parse_str(&self.service_uuid)?,
            peripheral_uuids: self
                .peripheral_uuids
                .into_iter()
                .map(|s| Uuid::parse_str(&s))
                .collect::<Result<Vec<Uuid>, uuid::Error>>()?,
            requests: self.requests.clone(),
        })
    }
}

async fn read_uuid_file(
    file_path: Option<&str>,
) -> Result<Option<TargetUuid<Uuid>>, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(file_path) = file_path {
        let file_content = fs::read_to_string(file_path).await?;
        let target_uuid: Result<Option<TargetUuid<Uuid>>, _> = serde_yaml::from_str(&file_content)
            .and_then(|target: TargetUuid<String>| Ok(target.into_uuid()))?
            .map(Some);

        return Ok(target_uuid?);
    }
    Ok(None)
}

fn create_scan_filter(target_uuid: &TargetUuid<Uuid>) -> ScanFilter {
    ScanFilter {
        services: vec![target_uuid.service_uuid],
    }
}

fn target_uuid_contains(target_uuids: &Option<TargetUuid<Uuid>>, id: &PeripheralId) -> bool {
    if let Some(target_uuids) = target_uuids {
        target_uuids
            .peripheral_uuids
            .contains(&Uuid::from_str(&id.to_string()).unwrap())
    } else {
        true
    }
}

fn get_peripheral_id(
    event: &CentralEvent,
    target_uuids: Option<TargetUuid<Uuid>>,
) -> Option<PeripheralId> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement {
            id,
            manufacturer_data,
        } => {
            if target_uuid_contains(&target_uuids, &id) {
                return Some(id.clone());
            }
        }
        CentralEvent::ServicesAdvertisement { id, services } => {
            if target_uuid_contains(&target_uuids, &id) {
                return Some(id.clone());
            }
        }
        CentralEvent::ServiceDataAdvertisement { id, service_data } => {
            if target_uuid_contains(&target_uuids, &id) {
                return Some(id.clone());
            }
        }
        CentralEvent::DeviceDiscovered(id)
        | CentralEvent::DeviceConnected(id)
        | CentralEvent::DeviceDisconnected(id)
        | CentralEvent::DeviceUpdated(id) => {
            if target_uuid_contains(&target_uuids, &id) {
                return Some(id.clone());
            }
        }
        CentralEvent::StateUpdate(centralState) => {}
    }
    None
}

fn get_message_type(event: &CentralEvent) -> Option<MessageType> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { id, .. } => {
            Some(MessageType::ManufacturerDataAdvertisement)
        }
        CentralEvent::ServicesAdvertisement { id, .. } => Some(MessageType::ServiceAdvertisement),
        CentralEvent::ServiceDataAdvertisement { id, .. } => {
            Some(MessageType::ServiceDataAdvertisement)
        }
        _ => None,
    }
}

async fn monitor(
    manager: &Manager,
    target_uuid: &Option<TargetUuid<Uuid>>,
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }

    let scan_filter = match target_uuid {
        Some(tuid) => create_scan_filter(tuid),
        None => ScanFilter::default(),
    };
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
    let target_uuid = read_uuid_file(cli.uuid_file.as_deref()).await?;
    let manager = Manager::new().await?;

    match cli.command {
        Command::Monitor { ref file } => {
            let event_records = Arc::new(RwLock::new(Vec::new()));
            let ad_store = Arc::new(SqliteAdStore::new(file)?);

            // Initialize the database
            ad_store.init()?;

            // Run monitor and save_events concurrently
            tokio::try_join!(
                monitor(&manager, &target_uuid, event_records.clone()),
                save_events(event_records, ad_store)
            )?;
        }

        Command::Load { file } => {}
        Command::InitDb { file } => {
            let ad_store = Arc::new(Box::new(SqliteAdStore::new(&file)?));
            ad_store.init()?;
        }
    }

    Ok(())
}

#[cfg(test)] // This ensures the test code is only included in test builds
mod tests {
    use super::*; // Import the TargetUuid struct
    use serde_yaml;

    #[test]
    fn deserialize_target_uuid() {
        let yaml_data = r#"
service_uuid: "550e8400-e29b-41d4-a716-446655440000"
peripheral_uuids:
  - "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
  - "123e4567-e89b-12d3-a456-426614174000"
requests: {}
"#;

        // Attempt to deserialize the YAML into the TargetUuid<String> struct
        let result: TargetUuid<String> =
            serde_yaml::from_str(yaml_data).expect("Failed to deserialize YAML");
        println!("{:?}", result);

        // Define the expected struct value for comparison
        let expected = TargetUuid {
            service_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            peripheral_uuids: vec![
                "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
                "123e4567-e89b-12d3-a456-426614174000".to_string(),
            ],
            requests: HashMap::from([]),
        };

        // Assert the deserialized struct matches the expected value
        assert_eq!(result, expected);
    }
}
