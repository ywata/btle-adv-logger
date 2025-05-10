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

use clap::{Parser, Subcommand};
use serde_yaml;

use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
use btleplug::platform::{Manager, PeripheralId};

use chrono::{DateTime, Utc};
use log::logger;
use datastore::{AdStore, AdStoreError};

use rusqlite::Result;
use tokio::sync::watch;

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




fn create_scan_filter() -> ScanFilter {
    ScanFilter { services: vec![] }
}

fn get_peripheral_id(event: &CentralEvent) -> Option<PeripheralId> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { id, .. } => {
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



async fn monitor(
    manager: &Manager,
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }

    let scan_filter = ScanFilter::default();
    let central = adapter_list.into_iter().nth(0).unwrap();

    central.start_scan(scan_filter).await?;
    let mut events = central.events().await?;


    loop {
        tokio::select! {
            maybe_event = events.next() => {
                if let Some(event) = maybe_event {
                    log::trace!("Event: {:?}", &event);                    
                    let mut records_lock = event_records.write().await;
                    let utc_now = Utc::now();
                    records_lock.push((utc_now, event));

                } else {
                    break;
                }
            }
            _ = stop_rx.changed() => {
                log::info!("Stopping monitor:");
                if *stop_rx.borrow() {
                    // Received stop signal
                    break;
                }
            }
        }
    }

    log::info!("Finished monitoring");
    Ok(())
}

pub async fn save_events(
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    ad_store: Arc<dyn AdStore>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<AdStoreError>> {
    let mut interval = time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let mut records_lock = event_records.write().await;
                while let Some((_, event)) = records_lock.pop() {
                    log::trace!("Saving event: {:?}", &event);
                    ad_store.store_event(&event)?;
                }
            }
            _ = stop_rx.changed() => {
                log::info!("Stopping save_events");
                if *stop_rx.borrow() {
                    break;
                }
            }
        }
    }
    log::info!("Finished saving events");
    Ok(())
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

            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    log::info!("Ctrl-C received, stopping...");
                    let _ = stop_tx.send(true);
                }
            });

            tokio::try_join!(
                monitor(&manager, event_records.clone(), stop_rx.clone()),
                save_events(event_records, ad_store, stop_rx)
            )?;
        }
        
        Command::Load { file } => {
            let ad_store = Arc::new(Box::new(SqliteAdStore::new(&file)?));
            let events = ad_store.load_event();
        }
        Command::InitDb { file } => {
            let ad_store = Arc::new(Box::new(SqliteAdStore::new(&file)?));
            ad_store.init()?;
        }
    }

    Ok(())
}
