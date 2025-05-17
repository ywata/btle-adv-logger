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
use std::collections::HashSet;
use tokio::sync::RwLock;
use tokio::{fs, time};

use clap::{Parser, Subcommand};
use serde_yaml;

use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
use btleplug::platform::{Manager, PeripheralId};

use chrono::{DateTime, TimeDelta, Utc};
use datastore::{AdStore, AdStoreError};
use log::logger;

use rusqlite::Result;
use tokio::sync::watch;

#[derive(Debug, Parser)]
#[command(about = "BLE inspection tool", long_about = None)]
struct Cli {
    #[arg(long, default_value = "10")]
    scan_duration_sec: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
enum MessageType {
    ManufacturerDataAdvertisement,
    ServiceAdvertisement,
    ServiceDataAdvertisement,
    DeviceDiscovered,
    DeviceConnected,
    DeviceDisconnected,
    DeviceUpdated,
    StateUpdate,
}

#[derive(Subcommand, Clone, Debug)]
enum Command {
    Monitor {
        file: String,
        #[arg(long)]
        filter: Option<String>,
        /// Minimum interval in seconds between recording events from the same peripheral
        #[arg(long, default_value_t = 600)]
        interval: u32,
    },
    CaptureId {
        #[arg(long, default_value = "10")]
        duration_sec: u64,
    },
    InitDb {
        file: String,
    },
    Load {
        file: String,
    },
}


trait ValidationParser<S, T> {
    fn parse(&self, data: S) -> Result<T, String>;
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CaptureConfig {
    #[serde(default)]
    peripheral_id: String,
    duration_sec: u64,
}

impl ValidationParser<CaptureConfig, CaptureConfig> for CaptureConfig {
    fn parse(&self, config: CaptureConfig) -> Result<Self, String> {
        if config.peripheral_id.is_empty() {
            return Err("Missing peripheral ID".to_string());
        }
        Ok(config)
    }
}


// Filter configuration structure
#[derive(Debug, Deserialize, Default)]
struct FilterConfig {
    // List of peripheral IDs to include (if empty, include all)
    #[serde(default)]
    capture_config: Vec<CaptureConfig>,
    include_peripheral_ids: Vec<String>,
}

impl ValidationParser<&String, FilterConfig> for FilterConfig {
    fn parse(&self, config: &String) -> Result<Self, String> {
        let parsed_config: FilterConfig = serde_yaml::from_str(&config)
            .map_err(|e| format!("Failed to parse filter config: {}", e))?;
        for capture in &parsed_config.capture_config {
            let parsed_capture = capture.parse(capture.clone())?;
        }
        Ok(parsed_config)
    }
}

impl FilterConfig {
    // Load filter configuration from a YAML file
    async fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string(path).await?;
        let config: FilterConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    // Create a filter function based on this configuration
    fn create_filter(
        &self,
    ) -> impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static {
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

fn get_message_type(event: &CentralEvent) -> MessageType {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { .. } => {
            MessageType::ManufacturerDataAdvertisement
        }
        CentralEvent::ServicesAdvertisement { .. } => MessageType::ServiceAdvertisement,
        CentralEvent::ServiceDataAdvertisement { .. } => MessageType::ServiceDataAdvertisement,
        CentralEvent::DeviceDiscovered(_) => MessageType::DeviceDiscovered,
        CentralEvent::DeviceConnected(_) => MessageType::DeviceConnected,
        CentralEvent::DeviceDisconnected(_) => MessageType::DeviceDisconnected,
        CentralEvent::DeviceUpdated(_) => MessageType::DeviceUpdated,
        CentralEvent::StateUpdate(_) => MessageType::StateUpdate,
    }
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
    _ad_store: Arc<dyn AdStore<'_, (DateTime<Utc>, CentralEvent)>>,
    filter: impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static,
    mut stop_rx: watch::Receiver<bool>,
    interval_sec: HashMap<PeripheralId, u32>,
) -> Result<Vec<(DateTime<Utc>, CentralEvent)>, Box<AdStoreError>> {
    let mut interval = time::interval(Duration::from_secs(1));
    let mut results = Vec::new();
    let mut seen_message_types: HashMap<PeripheralId, HashSet<MessageType>> = HashMap::new();
    let mut last_seen_timestamp: HashMap<PeripheralId, DateTime<Utc>> = HashMap::new();
    
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Process events
                let mut records_lock = event_records.write().await;
                
                while !records_lock.is_empty() {
                    // Remove the first element (index 0)
                    let event = records_lock.remove(0);

                    if filter(&event) {
                        process_event(
                            &event, 
                            &mut results, 
                            &mut seen_message_types, 
                            &mut last_seen_timestamp, 
                            &interval_sec
                        );
                    }
                }
            }
            Ok(stop) = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    log::info!("Received stop signal");
                    break;
                }
            }
        }
    }
    log::info!("Finished saving events");
    Ok(results)
}

fn process_event(
    event: &(DateTime<Utc>, CentralEvent),
    results: &mut Vec<(DateTime<Utc>, CentralEvent)>,
    seen_message_types: &mut HashMap<PeripheralId, HashSet<MessageType>>,
    last_seen_timestamp: &mut HashMap<PeripheralId, DateTime<Utc>>,
    interval_sec: &HashMap<PeripheralId, u32>,
) {
    if let Some(peripheral_id) = get_peripheral_id(&event.1) {
        let message_type = get_message_type(&event.1);
        let previous_timestamp = last_seen_timestamp
            .get(&peripheral_id)
            .unwrap_or(&DateTime::UNIX_EPOCH)
            .clone();


        // Get the interval setting for this peripheral, default to 0 if not specified
        let ignore_interval_sec = interval_sec
            .get(&peripheral_id)
            .unwrap_or(&0); // If not defined, record all events
        // insert empty seen_message_types for a peripheral iff
        //  - the first message of the peripheral
        //  - the message type is not already seen
        //  - the time interval is expired
        // in these cases, we will record the event
        //println!("seen_message_types: {:?}", seen_message_types);
        if !seen_message_types.contains_key(&peripheral_id) {
            log::trace!("A {:?} : {:?} {:?} {:?}",
                     event.1,
                     !seen_message_types.contains_key(&peripheral_id),
                     !seen_message_types.get(&peripheral_id).is_some_and(|x|x.contains(&message_type)),
                     event.0 - previous_timestamp >= TimeDelta::seconds(*ignore_interval_sec as i64));
            last_seen_timestamp.entry(peripheral_id.clone()).or_insert(event.0);
        }else if !seen_message_types.get(&peripheral_id).is_some_and(|x|x.contains(&message_type)) {
            log::trace!("B {:?} : {:?} {:?} {:?}",
                     event.1,
                     !seen_message_types.contains_key(&peripheral_id),
                     !seen_message_types.get(&peripheral_id).is_some_and(|x|x.contains(&message_type)),
                     event.0 - previous_timestamp >= TimeDelta::seconds(*ignore_interval_sec as i64));
        } else if event.0 - previous_timestamp >= TimeDelta::seconds(*ignore_interval_sec as i64) {
            log::trace!("C {:?} : {:?} {:?} {:?}",
                     event.1,
                     !seen_message_types.contains_key(&peripheral_id),
                     !seen_message_types.get(&peripheral_id).is_some_and(|x|x.contains(&message_type)),
                     event.0 - previous_timestamp >= TimeDelta::seconds(*ignore_interval_sec as i64));
            log::trace!("  : {:?} {:?} {:?}",
                     event.0,
                     previous_timestamp,
                     event.0 - previous_timestamp);
            seen_message_types.insert(peripheral_id.clone(), HashSet::new());
            last_seen_timestamp.insert(peripheral_id.clone(),event.0);
        } else {
            return
        }
        seen_message_types.entry(peripheral_id.clone()).or_insert(HashSet::new()).insert(message_type);
        results.push((event.0, event.1.clone()));
    }
    // We ignore events without a peripheral ID
}

async fn report_peripheral(
    event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<Vec<(DateTime<Utc>, CentralEvent)>, Box<dyn std::error::Error + Send + Sync>> {
    let mut interval = time::interval(Duration::from_secs(1));
    let mut peripheral_events: std::collections::HashMap<PeripheralId, Vec<CentralEvent>> =
        std::collections::HashMap::new();
    let mut results = Vec::new();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let mut records_lock = event_records.write().await;
                while let Some(event) = records_lock.pop() {
                    if let Some(peripheral_id) = get_peripheral_id(&event.1) {
                        if !peripheral_events.contains_key(&peripheral_id) {
                            let mut v = Vec::new();
                            peripheral_events.insert(peripheral_id.clone(), v);
                        }
                        if let Some(v) = peripheral_events.get_mut(&peripheral_id) {
                            v.push(event.1.clone());
                            results.push(event);
                        }

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

    // Print summary
    println!(
        "\nCapture complete. Found {} unique devices:",
        peripheral_events.len()
    );
    for p_map in peripheral_events {
        println!("{:?}", p_map.0);
        for event in p_map.1 {
            println!("  - {:?}", event);
        }
    }

    Ok(results)
}

fn create_accept_all_filter() -> impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static {
    |_| true
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pretty_env_logger::init();
    let cli = Cli::parse();
    let manager = Manager::new().await?;

    match cli.command {
        Command::Monitor {
            ref file,
            ref filter,
            interval,
        } => {
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
                Some(filter_file) => FilterConfig::from_file(filter_file).await?
                    .parse(filter_file)
                    .map_err(|e| format!("Failed to parse filter config: {}", e))?,
                None => FilterConfig::default(),
            };

            let filter_fn = filter_config.create_filter();

            tokio::try_join!(
                monitor(&manager, event_records.clone(), stop_rx.clone()),
                save_events(event_records, ad_store, filter_fn, stop_rx, HashMap::new())
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
            let _events = ad_store.load_event();
        }
        Command::InitDb { file } => {
            let ad_store = Arc::new(Box::new(SqliteAdStore::new(&file)?));
            ad_store.init()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use btleplug::api::{BDAddr, CentralEvent, Characteristic, Peripheral as _};
    use uuid::Uuid;
    use chrono::{DateTime, Duration, Utc};
    use serde::{Deserialize, Serialize};
    use tokio::sync::{watch, RwLock};
    use std::sync::Arc;
    use crate::datastore::{AdStore, AdStoreError};

    // Mock implementation of AdStore for testing
    struct MockAdStore {
        stored_events: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    }

    impl MockAdStore {
        fn new() -> Self {
            Self {
                stored_events: Arc::new(RwLock::new(Vec::new())),
            }
        }

        async fn get_stored_events(&self) -> Vec<(DateTime<Utc>, CentralEvent)> {
            self.stored_events.read().await.clone()
        }
    }

    impl<'a> AdStore<'a, (DateTime<Utc>, CentralEvent)> for MockAdStore {
        fn init(&self) -> Result<(), AdStoreError> {
            Ok(())
        }

        fn store_event(&self, event: &(DateTime<Utc>, CentralEvent)) -> Result<(), AdStoreError> {
            let event_clone = event.clone();
            tokio::spawn({
                let stored_events = self.stored_events.clone();
                async move {
                    stored_events.write().await.push(event_clone);
                }
            });
            Ok(())
        }

        fn load_event(&self) -> Result<Vec<(DateTime<Utc>, CentralEvent)>, AdStoreError> {
            // Not needed for this test
            Ok(Vec::new())
        }
    }

    // Helper function to create a test peripheral ID
    fn create_test_peripheral_id(addr: &str) -> btleplug::platform::PeripheralId {
        let addr_clean = addr.replace(':', "");

        // Create a UUID using the MAC address as part of the UUID
        // This ensures consistent IDs for the same MAC address
        let uuid_string = format!("{}-0000-1000-8000-00805f9b34fb", &addr_clean[0..8]);
        let uuid = Uuid::parse_str(&uuid_string).unwrap_or_else(|_| {
            // Fallback to a default UUID if parsing fails
            Uuid::from_u128(0x00000000000000000000000000000000)
        });
        
        // Create the PeripheralId from the UUID
        btleplug::platform::PeripheralId::from(uuid)
    }

    // Helper function to create test events
    fn create_service_data_event(peripheral_id: &btleplug::platform::PeripheralId, time: DateTime<Utc>) -> (DateTime<Utc>, CentralEvent) {
        let uuid = Uuid::from_u128(0x1234);
        (
            time,
            CentralEvent::ServiceDataAdvertisement { 
                id: peripheral_id.clone(), 
                service_data: HashMap::from([(uuid, vec![1, 2, 3, 4])]) 
            }
        )
    }

    fn create_manufacturer_data_event(peripheral_id: &btleplug::platform::PeripheralId, time: DateTime<Utc>) -> (DateTime<Utc>, CentralEvent) {
        (
            time,
            CentralEvent::ManufacturerDataAdvertisement { 
                id: peripheral_id.clone(), 
                manufacturer_data: HashMap::from([(0x004C, vec![1, 2, 3, 4])]) 
            }
        )
    }

    fn create_service_adv_event(peripheral_id: &btleplug::platform::PeripheralId, time: DateTime<Utc>) -> (DateTime<Utc>, CentralEvent) {
        let uuid = Uuid::from_u128(0x5678);
        (
            time,
            CentralEvent::ServicesAdvertisement {
                id: peripheral_id.clone(), 
                services: Vec::from([uuid])
            }
        )
    }
    fn create_state_update_event(_:PeripheralId, time: DateTime<Utc>) -> (DateTime<Utc>, CentralEvent) {
        (
            time,
            CentralEvent::StateUpdate(btleplug::api::CentralState::PoweredOn)
        )
    }

    // Helper function to create a simple filter that accepts all events
    fn create_accept_all_filter() -> impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static {
        |_| true
    }
    fn create_accept_specified_peripheral_id_filter(peripheral_ids: Vec<PeripheralId>) -> impl Fn(&(DateTime<Utc>, CentralEvent)) -> bool + Send + Sync + 'static {
        move |event: &(DateTime<Utc>, CentralEvent)| {
            if let Some(peripheral_id) = get_peripheral_id(&event.1) {
                let id_str = format!("{:?}", peripheral_id);
                return peripheral_ids.iter().any(|included| id_str.contains(&included.to_string()));
            }
            false
        }
    }
    fn create_test_events(now: DateTime<Utc>, vec: Vec<(PeripheralId, MessageType, u32)>) -> Vec<(DateTime<Utc>, CentralEvent)> {
        let mut events : Vec<(DateTime<Utc>, CentralEvent)> = Vec::new();
        let mut next = now;;
        for (id, message_type, diff) in vec {
            let peripheral_id = create_test_peripheral_id(&id.to_string());
            let next = now + Duration::seconds(diff as i64);
            let event = match message_type {
                MessageType::ManufacturerDataAdvertisement => create_manufacturer_data_event(&peripheral_id, next),
                MessageType::ServiceAdvertisement => create_service_adv_event(&peripheral_id, next),
                MessageType::ServiceDataAdvertisement => create_service_data_event(&peripheral_id, next),
                MessageType::StateUpdate => create_state_update_event(peripheral_id, next),
                _ => panic!("Unsupported message type"),
            };
            events.push(event);


        }
        events
    }
    #[tokio::test]
    async fn test_save_events_stop_signal() {

        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Create a test event
        let now = Utc::now();
        let peripheral_id = create_test_peripheral_id("00:11:22:33:44:55");
        let event = create_service_data_event(&peripheral_id, now);

        // Add the event to the records
        {
            let mut records = event_records.write().await;
            records.push(event);
        }

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            save_task,
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "save_events should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "save_events should complete without errors");
    }
    #[tokio::test]
    async fn test_save_events_filter() {

        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a test event
        let now = Utc::now();
        let peripheral_id1 = create_test_peripheral_id("00:00:00:01:00:00");
        let peripheral_id2 = create_test_peripheral_id("00:00:00:02:00:00");
        
        // Create a filter that only accepts events from peripheral_id2
        let filter = create_accept_specified_peripheral_id_filter(vec![peripheral_id2.clone()]);

        let events = vec![
            (peripheral_id1.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id2.clone(), MessageType::ServiceAdvertisement, 0),
        ];

        // Add the event to the records
        let mut records = event_records.write().await;
        for event in create_test_events(now, events) {
            records.push(event);
        }
        drop(records); // Release the lock

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let result = match tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            save_task,
        ).await {
            Ok(task_result) => task_result.unwrap().unwrap(),
            Err(_) => panic!("Test timed out waiting for save_events to complete"),
        };
        
        // Assert: Only events from peripheral_id2 should be returned
        assert_eq!(result.len(), 1, "Filtered event count should be 1");
        
        // Verify the returned event is from peripheral_id2
        if let Some(peripheral_id) = get_peripheral_id(&result[0].1) {
            assert_eq!(peripheral_id, peripheral_id2, "Event should be from peripheral_id2");
        } else {
            panic!("Event should have a peripheral ID");
        }
    }
    #[tokio::test]
    async fn test_save_events_collect_one_events_for_each_message_type() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Create a test event
        let now = Utc::now();
        let peripheral_id = create_test_peripheral_id("00:00:00:01:00:00");
        let events = vec![
            (peripheral_id.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 1),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 2),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 3),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 4),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 13),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 15),
        ];
        let intervals : HashMap<PeripheralId, u32> = HashMap::from([
            (peripheral_id.clone(), 20),
        ]);


        // Add the event to the records
        {
            let mut records = event_records.write().await;
            for event in create_test_events(now, events) {
                records.push(event);
            }
        }

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            intervals
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            save_task,
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "save_events should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "save_events should complete without errors");
        assert_eq!(result.unwrap().len(), 3, "collect one event for each message type");
        // Verify the stored events in the mock store
        let stored_events = ad_store.get_stored_events().await;
        assert_eq!(stored_events.len(), 0, "all the events processed");
    }
    #[tokio::test]
    async fn test_save_events_collect_one_events_for_each_message_type_per_periphel() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Create a test event
        let now = Utc::now();
        let peripheral_id1 = create_test_peripheral_id("00:00:00:01:00:00");
        let peripheral_id2 = create_test_peripheral_id("00:00:00:02:00:00");
        let events = vec![
            (peripheral_id1.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 1),
            (peripheral_id1.clone(), MessageType::ServiceDataAdvertisement, 2),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 3),

            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 4),
            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 13),
            (peripheral_id2.clone(), MessageType::ManufacturerDataAdvertisement, 15),
        ];
        let intervals : HashMap<PeripheralId, u32> = HashMap::from([
            (peripheral_id1.clone(), 20),
            (peripheral_id2.clone(), 20),
        ]);


        // Add the event to the records
        {
            let mut records = event_records.write().await;
            for event in create_test_events(now, events) {
                records.push(event);
            }
        }

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            intervals
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            save_task,
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "save_events should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "save_events should complete without errors");
        assert_eq!(result.unwrap().len(), 5, "collect one event for each message type");
        // Verify the stored events in the mock store
        let stored_events = ad_store.get_stored_events().await;
        assert_eq!(stored_events.len(), 0, "all the events processed");
    }
    #[tokio::test]
    async fn test_save_events_collect_events_interval_wise() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Create a test event
        let now = Utc::now();
        let peripheral_id1 = create_test_peripheral_id("00:00:00:01:00:00");
        let peripheral_id2 = create_test_peripheral_id("00:00:00:02:00:00");
        // events are ordered by increasing time
        let mut events = vec![
            // peripheral_id1: 3
            (peripheral_id1.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 1),
            (peripheral_id1.clone(), MessageType::ServiceDataAdvertisement, 2),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 3),

            // later than 10 secs: 2
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 11),
            (peripheral_id1.clone(), MessageType::ServiceDataAdvertisement, 12),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 13),

            // peripheral_id2: 2
//            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 0),
//            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 13),
//            (peripheral_id2.clone(), MessageType::ManufacturerDataAdvertisement, 15),

            // later than 20 secs: 3
//            (peripheral_id2.clone(), MessageType::ServiceAdvertisement, 24),
//            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 33),
//            (peripheral_id2.clone(), MessageType::ManufacturerDataAdvertisement, 35),


        ];
        events.sort_by(|a, b| a.2.cmp(&b.2));

        let intervals : HashMap<PeripheralId, u32> = HashMap::from([
            (peripheral_id1.clone(), 10),
            (peripheral_id2.clone(), 20),
        ]);


        // Add the event to the records
        {
            let mut records = event_records.write().await;
            for event in create_test_events(now, events) {
                records.push(event);
            }
        }

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            intervals,
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            save_task,
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "save_events should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "save_events should complete without errors");
        assert_eq!(result.unwrap().len(), 5, "collect one event for each message type");
        // Verify the stored events in the mock store
        let stored_events = ad_store.get_stored_events().await;
        assert_eq!(stored_events.len(), 0, "all the events processed");
    }

    //#[tokio::test]
    async fn test_save_events_restart() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Create a test event
        let now = Utc::now();
        let peripheral_id = create_test_peripheral_id("00:00:00:01:00:00");
        let events = vec![
            (peripheral_id.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 1),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 2),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 3),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 4),
            //(peripheral_id.clone(), MessageType::ServiceAdvertisement, 5),

            // 10 secs later
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 13),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 15),

            // restart from here
            (peripheral_id.clone(), MessageType::ServiceAdvertisement, 17),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 18),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 19),
        ];


        // Add the event to the records
        {
            let mut records = event_records.write().await;
            for event in create_test_events(now, events) {
                records.push(event);
            }
            println!("records: {:?}\n", records);
        }

        // Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            save_task,
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "save_events should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "save_events should complete without errors");

        // Verify the stored events in the mock store
        let stored_events = ad_store.get_stored_events().await;
        assert_eq!(stored_events.len(), 6, "Should store one event");
    }




    // Test of repot_peripheral() In this test,
    #[tokio::test]
    async fn test_report_peripheral_stop_signal() {

        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let (stop_tx, stop_rx) = watch::channel(false);

        let now = Utc::now();

        // Create a peripheral ID
        let peripheral_id = create_test_peripheral_id("00:11:22:33:44:55");

        // Define events timing
        let events_timing = vec![
            (peripheral_id.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 1),
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 2),
        ];
        
        // Create and add events to the records
        {
            let mut records = event_records.write().await;
            let test_events = create_test_events(now, events_timing);
            for event in test_events {
                records.push(event);
            }
        }

        // Start the report_peripheral task
        let report_task = tokio::spawn(report_peripheral(
            event_records.clone(),
            stop_rx,
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            report_task
        ).await;

        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "report_peripheral should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "report_peripheral should complete without errors");

        // Verify the returned events
        let returned_events = result.unwrap();
        assert_eq!(returned_events.len(), 3, "Should return all processed events");

        // Verify that all events were processed (removed from event_records)
        let remaining_records = event_records.read().await;
        assert_eq!(remaining_records.len(), 0, "All events should be processed");
    }

    #[tokio::test]
    async fn test_report_peripheral_drop_state_update() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let (stop_tx, stop_rx) = watch::channel(false);

        let now = Utc::now();

        // Create a peripheral ID
        let peripheral_id1 = create_test_peripheral_id("00:00:00:01:00:00");
        let peripheral_dummy = create_test_peripheral_id("00:00:00:02:00:00");
        let filter = create_accept_specified_peripheral_id_filter(vec![peripheral_id1.clone()]);

        // Define events timing
        let events_timing = vec![
            (peripheral_id1.clone(), MessageType::ServiceAdvertisement, 0),
            (peripheral_dummy.clone(), MessageType::StateUpdate, 1),
            (peripheral_id1.clone(), MessageType::ServiceDataAdvertisement, 2),
        ];
        println!("1");
        // Create and add events to the records
        {
            let mut records = event_records.write().await;
            let test_events = create_test_events(now, events_timing);

            for event in test_events {
                records.push(event);
            }
        }

        // Start the report_peripheral task
        let report_task = tokio::spawn(report_peripheral(
            event_records.clone(),
            stop_rx,
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a timeout
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            report_task
        ).await;


        // Assert that the task completed within the timeout period
        assert!(timeout.is_ok(), "report_peripheral should stop when stop signal is received");

        // Get the result and check it
        let result = timeout.unwrap().unwrap();
        assert!(result.is_ok(), "report_peripheral should complete without errors");

        // Verify the returned events
        let returned_events = result.unwrap();
        assert_eq!(returned_events.len(), 2, "Should return all processed events");

        // Verify that all events were processed (removed from event_records)
        let remaining_records = event_records.read().await;
        assert_eq!(remaining_records.len(), 0, "All events should be processed");
    }

    #[tokio::test]
    async fn test_save_events_fifo_order() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create test events with different timestamps
        let now = Utc::now();
        let peripheral_id = create_test_peripheral_id("00:00:00:01:00:00");
        
        // Create events with timestamps in a specific order
        let events = vec![
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 0),  // First event (now)
            (peripheral_id.clone(), MessageType::ManufacturerDataAdvertisement, 1),  // Second event (now + 1s)
            (peripheral_id.clone(), MessageType::ServiceAdvertisement, 2),  // Third event (now + 2s)
        ];
        
        let test_events = create_test_events(now, events);
        
        // Add events to records in the same order
        let mut records = event_records.write().await;
        for event in test_events {
            records.push(event);
        }
        drop(records); // Release the lock
        
        // Create a filter that accepts all events
        let filter = create_accept_all_filter();

        // Act: Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(), // Large interval to ensure all events are processed
        ));

        // Wait a short time to ensure the task has started processing events
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete with a longer timeout
        let result = match tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            save_task,
        ).await {
            Ok(task_result) => task_result.unwrap().unwrap(),
            Err(_) => panic!("Test timed out waiting for save_events to complete"),
        };
        
        // Assert: Verify events are returned in FIFO order (same order they were added)
        assert_eq!(result.len(), 3, "Should return all 3 events");
        
        // Check that events are in the correct order by comparing timestamps
        for i in 1..result.len() {
            let prev_time = result[i-1].0;
            let curr_time = result[i].0;
            assert!(prev_time <= curr_time, "Events should be in chronological order");
        }
    }

    #[tokio::test]
    async fn test_save_events_filtering() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create test events with different peripheral IDs
        let now = Utc::now();
        let peripheral_id1 = create_test_peripheral_id("00:00:00:01:00:00");
        let peripheral_id2 = create_test_peripheral_id("00:00:00:02:00:00");
        
        // Create events for different peripherals
        let events = vec![
            (peripheral_id1.clone(), MessageType::ServiceDataAdvertisement, 0),
            (peripheral_id2.clone(), MessageType::ServiceDataAdvertisement, 1),
            (peripheral_id1.clone(), MessageType::ManufacturerDataAdvertisement, 2),
            (peripheral_id2.clone(), MessageType::ManufacturerDataAdvertisement, 3),
        ];
        
        let test_events = create_test_events(now, events);
        
        // Add events to records
        let mut records = event_records.write().await;
        for event in test_events {
            records.push(event);
        }
        drop(records);
        
        // Create a filter that only accepts events from peripheral_id1
        let filter = create_accept_specified_peripheral_id_filter(vec![peripheral_id1.clone()]);

        // Act: Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete
        let result = match tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            save_task,
        ).await {
            Ok(task_result) => task_result.unwrap().unwrap(),
            Err(_) => panic!("Test timed out waiting for save_events to complete"),
        };
        
        // Assert: Only events from peripheral_id1 should be returned
        assert_eq!(result.len(), 2, "Should return only events from peripheral_id1");
        
        // Verify all returned events are from peripheral_id1
        for event in &result {
            if let Some(peripheral_id) = get_peripheral_id(&event.1) {
                assert_eq!(peripheral_id, peripheral_id1, "All events should be from peripheral_id1");
            } else {
                panic!("Event should have a peripheral ID");
            }
        }
    }

    #[tokio::test]
    async fn test_save_events_error_handling() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);

        // Create a test event
        let now = Utc::now();
        let peripheral_id = create_test_peripheral_id("00:00:00:01:00:00");
        
        // Create events
        let events = vec![
            (peripheral_id.clone(), MessageType::ServiceDataAdvertisement, 0),
        ];
        
        let test_events = create_test_events(now, events);
        
        // Add events to records
        let mut records = event_records.write().await;
        for event in test_events {
            records.push(event);
        }
        drop(records);
        
        // Create a filter that always returns false - simulating filtering out all events
        let filter = |_: &(DateTime<Utc>, CentralEvent)| -> bool {
            false
        };

        // Act: Start the save_events task
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete
        let result = match tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            save_task,
        ).await {
            Ok(task_result) => task_result.unwrap().unwrap(),
            Err(_) => panic!("Test timed out waiting for save_events to complete"),
        };
        
        // Assert: The function should complete without errors but return no events (all filtered out)
        assert_eq!(result.len(), 0, "Should return no events when all are filtered out");
    }

    #[tokio::test]
    async fn test_save_events_empty_events() {
        // Arrange
        let event_records = Arc::new(RwLock::new(Vec::new()));
        let ad_store = Arc::new(MockAdStore::new());
        let (stop_tx, stop_rx) = watch::channel(false);
        
        // Create a filter that accepts all events
        let filter = |_: &(DateTime<Utc>, CentralEvent)| -> bool { true };

        // Start the save_events task with an empty event vector
        let save_task = tokio::spawn(save_events(
            event_records.clone(),
            ad_store.clone(),
            filter,
            stop_rx,
            HashMap::new(),
        ));

        // Wait a short time to ensure the task has started
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Signal to stop
        stop_tx.send(true).unwrap();

        // Wait for the task to complete
        let result = match tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            save_task,
        ).await {
            Ok(task_result) => task_result.unwrap().unwrap(),
            Err(_) => panic!("Test timed out waiting for save_events to complete"),
        };
        
        // Assert: No events should be returned since the vector was empty
        assert_eq!(result.len(), 0, "Should return no events when the vector is empty");
    }
}
