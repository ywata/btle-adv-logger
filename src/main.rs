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
use datastore::{AdStore, AdStoreError};
use log::logger;

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
    Monitor { 
        file: String,
        #[arg(long)]
        filter: Option<String>,
    },
    CaptureId {
        #[arg(long, default_value = "10")]
        duration_sec: u64,
    },
    InitDb { file: String },
    Load { file: String },
}

enum EventType {
    ManufacturerDataAdvertisement,
    ServicesAdvertisement,
    ServiceDataAdvertisement,
    DeviceDiscovered,
    DeviceConnected,
    DeviceDisconnected,
    DeviceUpdated,
    StateUpdate,
}

fn get_event_type(event: &CentralEvent) -> EventType {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { .. } => EventType::ManufacturerDataAdvertisement,
        CentralEvent::ServicesAdvertisement { .. } => EventType::ServicesAdvertisement,
        CentralEvent::ServiceDataAdvertisement { .. } => EventType::ServiceDataAdvertisement,
        CentralEvent::DeviceDiscovered(_) => EventType::DeviceDiscovered,
        CentralEvent::DeviceConnected(_) => EventType::DeviceConnected,
        CentralEvent::DeviceDisconnected(_) => EventType::DeviceDisconnected,
        CentralEvent::DeviceUpdated(_) => EventType::DeviceUpdated,
        CentralEvent::StateUpdate(_) => EventType::StateUpdate,
    }
}

// Filter configuration structure
#[derive(Debug, Deserialize, Default)]
struct FilterConfig {
    // List of peripheral IDs to include (if empty, include all)
    #[serde(default)]
    include_peripheral_ids: Vec<String>,
}

impl FilterConfig {
    // Load filter configuration from a YAML file
    async fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string(path).await?;
        let config: FilterConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }
    
    // Create a filter function based on this configuration
    fn create_filter(&self) -> impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static {
        // Clone the configuration data for the closure
        let include_ids = self.include_peripheral_ids.clone();
        
        move |event: &(DateTime<Utc>, CentralEvent)| {
            if let Some(peripheral_id) = get_peripheral_id(&event.1) {
                let id_str = format!("{:?}", peripheral_id);
                
                // If include list is empty, include all
                // Otherwise, only include if in the include list
                if include_ids.is_empty() {
                    return true;
                } else {
                    return include_ids.iter().any(|included| id_str.contains(included));
                }
            }
            
            // For events without a peripheral ID, include by default
            true
        }
    }
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
    ad_store: Arc<dyn AdStore<'_, (DateTime<Utc>, CentralEvent)>>,
    filter: impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<AdStoreError>> {
    let mut interval = time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let mut records_lock = event_records.write().await;
                while let Some(event) = records_lock.pop() {
                    if filter(&event) {
                        log::debug!("Saving event: {:?}", &event);
                        ad_store.store_event(&event)?;
                    } else {
                        log::debug!("Filtered out event: {:?}", &event);
                    }
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

async fn report_peripheral(
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut interval = time::interval(Duration::from_secs(1));
    let mut seen_peripherals = std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let mut records_lock = event_records.write().await;
                while let Some(event) = records_lock.pop() {
                    if let Some(peripheral_id) = get_peripheral_id(&event.1) {
                        log::debug!("Peripheral ID: {:?}", peripheral_id);
                        // Store the event in the peripheral advertisements map
                        peripheral_advertisements
                            .entry(peripheral_id.clone())
                            .or_insert_with(Vec::new)
                            .push(event.1.clone());
                    } else {
                        log::debug!("No Peripheral ID found for event: {:?}", &event);

                    }
                }
            }
            _ = stop_rx.changed() => {
                log::info!("Stopping report_peripheral");
                if *stop_rx.borrow() {
                    break;
                }
            }
        }
    }
    
    // print the collected advertisement data for each peripheral
    for (peripheral_id, events) in peripheral_advertisements.iter() {
        println!("{:?}", peripheral_id);
        for event in events {
            println!("  {:?}", event);
        }
    }

    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pretty_env_logger::init();
    let cli = Cli::parse();
    let manager = Manager::new().await?;

    match cli.command {
        Command::Monitor { ref file, ref filter } => {
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

            // Load filter configuration or use default (empty lists = no filtering)
            let filter_config = match filter {
                Some(filter_file) => FilterConfig::from_file(filter_file).await?,
                None => FilterConfig::default(),
            };
            
            let filter_fn = filter_config.create_filter();

            tokio::try_join!(
                monitor(&manager, event_records.clone(), stop_rx.clone()),
                save_events(event_records, ad_store, filter_fn, stop_rx)
            )?;
        }
        
        Command::CaptureId { duration_sec } => {
            println!("Starting device capture for {} seconds...", duration_sec);
            let event_records = Arc::new(RwLock::new(Vec::new()));
            
            // Create a stop channel that will automatically trigger after the specified duration
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            
            // Spawn a timer task to stop the capture after the specified duration
            let stop_tx_clone = stop_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(duration_sec)).await;
                log::info!("Capture duration reached, stopping...");
                let _ = stop_tx_clone.send(true);
            });
            
            // Also handle Ctrl-C for manual interruption
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    log::info!("Ctrl-C received, stopping capture...");
                    let _ = stop_tx.send(true);
                }
            });
            
            tokio::try_join!(
                monitor(&manager, event_records.clone(), stop_rx.clone()),
                report_peripheral(event_records, stop_rx)
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
