//use std::fmt::Error;
use serde::Deserialize;
use std::error::Error;
use clap::ValueEnum;
use tokio::sync::RwLockReadGuard;
use nix::unistd::Pid;
use btleplug::api::CharPropFlags;
use futures::stream::StreamExt;
use uuid::Uuid;
use std::str::FromStr;
use std::{
    collections::HashMap,
    sync::Arc,
};
use std::collections::HashSet;
use tokio::sync::RwLock;
use std::fmt::Debug;
use std::time::Duration;
use tokio::{fs, time, signal};

use serde_yaml;
use clap::{Parser, Subcommand};

use btleplug::api::{Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::api::CentralEvent::ServicesAdvertisement;
use btleplug::platform::{Manager, PeripheralId};
use pretty_env_logger::env_logger::Target;


use chrono::{DateTime, TimeDelta, Utc};
use nix::sys::signal as nix_signal;

use std::fs::File;
use std::io::{Read, Write};


#[derive(Debug, Parser)]
#[command(about = "BLE inspection tool", long_about = None)]
struct Cli {
    #[arg(long, default_value="10")]
    scan_secs:u64,

    #[arg(long)]
    uuid_file:Option<String>,
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
enum Command{
    Log,
    MeasureInterval,
    Monitor{file:Option<String>},
    Load{file:String},
    Scan,
    SendRequest{name: String, nth: usize},
    AnalyzeEvent{file: String, message_type: MessageType}

}

#[derive(Deserialize, Clone, Debug, PartialEq)]
enum Cmd {
    Str{cmd: String},
    Hex{cmd: Vec<u8>}
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct Request{
    device: String,
    cmd: Cmd,
}

impl Request {
    fn normalize(&self) -> Option<Self> {
        match &self.cmd {
            Cmd::Str{cmd} => {
                if let Some(hex) = cmd
                    .split_whitespace()
                    .map(|hex|u8::from_str_radix(hex, 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .ok() {
                    let mut r =self.clone();
                    r.cmd = Cmd::Hex{cmd:hex};

                    Some(r)
                } else {
                    None
                }
            }
            _ => Some(self.clone())
        }
    }
    fn to_bytes(&self) -> Vec<u8> {
        match &self.cmd {
            Cmd::Hex{cmd} => {
                cmd.clone()
            }
            _ => {Vec::new()}
        }
    }
}


#[derive(Deserialize, Clone, Debug, PartialEq)]
struct TargetUuid<T>{
    service_uuid: T,
    peripheral_uuids: Vec<T>,
    requests: HashMap<String, Vec<Request>>
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
            requests: self.requests.clone()
        })
    }
}

async fn read_uuid_file(file_path: Option<&str>) -> Result<Option<TargetUuid<Uuid>>, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(file_path) = file_path {
        let file_content = fs::read_to_string(file_path).await?;
        let target_uuid : Result<Option<TargetUuid<Uuid>>, _> = serde_yaml::from_str(&file_content)
            .and_then(|target:TargetUuid<String>| Ok(target.into_uuid()))?
            .map(Some);

        return Ok(target_uuid?);
    }
    Ok(None)
}


fn create_scan_filter(target_uuid: &TargetUuid<Uuid>) -> ScanFilter{
    ScanFilter{services: vec![target_uuid.service_uuid]}
}

async fn scan(manager:&Manager, scan_secs: u64, target_uuid:&Option<TargetUuid<Uuid>> ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }
    let scan_filter = match target_uuid {
        Some(tuid) => {
            create_scan_filter(tuid)
        }
        None => ScanFilter::default()
    };
    for adapter in adapter_list {
        adapter
            .start_scan(scan_filter.clone())
            .await
            .expect("Can't scan BLE adapter for devices");
        time::sleep(Duration::from_secs(scan_secs)).await;
        let peripherals = adapter.peripherals().await?;
        if peripherals.is_empty() {
            eprintln!("->>> BLE peripheral devices are not found");
        } else {
            for peripheral in peripherals.iter() {
                let properties = peripheral.properties().await?;
                let local_name = properties
                    .unwrap()
                    .local_name
                    .unwrap_or(String::from("(peripheral name unknown)"));

                println!("{:?}:{}", peripheral, local_name);
            }
        }
    }
    Ok(())
}

fn target_uuid_contains(target_uuids: &Option<TargetUuid<Uuid>>, id: &PeripheralId ) -> bool {
    if let Some(target_uuids) = target_uuids {
        target_uuids.peripheral_uuids.contains(&Uuid::from_str(&id.to_string()).unwrap())
    } else {
        true
    }
}


fn get_peripheral_id(event: &CentralEvent, target_uuids: Option<TargetUuid<Uuid>>) -> Option<PeripheralId> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::ServicesAdvertisement { id, .. }
        | CentralEvent::ServiceDataAdvertisement { id, .. }
        | CentralEvent::DeviceDiscovered(id)
        | CentralEvent::DeviceConnected(id)
        | CentralEvent::DeviceDisconnected(id)
        | CentralEvent::DeviceUpdated(id) => {
            if target_uuid_contains(&target_uuids, &id) {
                return Some(id.clone());
            }
        },
        _ => {}
    }
    None
}

fn get_message_type(event: &CentralEvent) -> Option<MessageType> {
    match event {
        CentralEvent::ManufacturerDataAdvertisement { id, .. } => {
            Some(MessageType::ManufacturerDataAdvertisement)
        }
        | CentralEvent::ServicesAdvertisement { id, .. } => {
            Some(MessageType::ServiceAdvertisement)
        }
        | CentralEvent::ServiceDataAdvertisement { id, .. } => {
            Some(MessageType::ServiceDataAdvertisement)
        }
        _ => {None}
    }
}



async fn monitor(manager:&Manager, target_uuid:&Option<TargetUuid<Uuid>>,
                 event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }

    let scan_filter = match target_uuid {
        Some(tuid) => {
            create_scan_filter(tuid)
        }
        None => ScanFilter::default()
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


async fn subscribe(peripheral:&impl Peripheral) -> Result<(Characteristic, Characteristic), Box<dyn Error+ Send + Sync>> {
    let mut notify_char = None;
    let mut write_char = None;
    for characteristic in peripheral.characteristics() {
        println!("Checking characteristic {:?}", characteristic);
        if characteristic.properties.contains(CharPropFlags::NOTIFY) {
            notify_char = Some(characteristic.clone());
            continue;
        }
        if characteristic.properties.contains(CharPropFlags::WRITE) {
            write_char = Some(characteristic);
            continue;
        }
    }
    match (notify_char, write_char) {
        (Some(notify), Some(write)) => {
            let _result = peripheral.subscribe(&notify).await?;
            Ok((write, notify))
        }
        _ => Err("subscribe failed".into())

    }

}

async fn send_request(manager:&Manager, scan_secs: u64,
                      target_uuid:&TargetUuid<Uuid>,
                      name:&String, nth: usize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {

    let request;
    if let Some(normalized_request) = target_uuid.requests.get(name)
        .and_then(|vec| vec.get(nth))
        .and_then(|req| req.normalize()) {
        request = normalized_request;
        println!("    {:?}", request);
    } else{
        eprintln!("request error: {:?}", target_uuid.requests);
        return Err("send_request argument".into());
    }


    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No adapters found");
    }

    let scan_filter =  create_scan_filter(target_uuid);
    for adapter in adapter_list {
        adapter
            .start_scan(scan_filter.clone())
            .await
            .expect("Can't scan BLE adapter for devices");
        time::sleep(Duration::from_secs(scan_secs)).await;
        let peripherals = adapter.peripherals().await?;
        if peripherals.is_empty() {
            eprintln!("->>> BLE peripheral devices are not found");
        } else {
            println!("search for peripherals:");
            for peripheral in peripherals.iter() {
                let properties = peripheral.properties().await?;
                let local_name = properties
                    .unwrap()
                    .local_name
                    .unwrap_or(String::from("(peripheral name unknown)"));
                if local_name != *name {
                    println!("{} is ignored skipping", local_name);
                    continue;
                }
                let is_connected = peripheral.is_connected().await?;
                println!(
                    "Peripheral {:?} is connected: {:?}",
                    &local_name, is_connected
                );
                if !is_connected {
                    if let Err(err) = peripheral.connect().await {
                        eprintln!("Error connecting to peripheral, skipping: {}", err);
                        continue;
                    }
                }
                let is_connected = peripheral.is_connected().await?;
                println!(
                    "Now connected ({:?}) to peripheral {:?}.",
                    is_connected, &local_name
                );
                if is_connected {
                    peripheral.discover_services().await?;
                    if let Ok((write_char, notify_char)) = subscribe(peripheral).await {
                        let bytes = request.to_bytes();
                        let write_result = peripheral.write(&write_char, &bytes, WriteType::WithResponse).await;
                        println!("{:?}", write_result);
                        println!("waiting for notification:");
                        while let Some(data) = peripheral.notifications().await?.next().await {
                            println!("Received notification: {:?}", data.value);
                            break;
                        }

                        let _ = peripheral.unsubscribe(&notify_char).await;



                        peripheral.disconnect().await?;
                    }
                } else {

                }
            }
        }
    }

    Ok(())
}


async fn spawn_killer(wait_secs: u64) {
    let pid = Pid::this();
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    match nix_signal::kill(pid, nix_signal::Signal::SIGINT) {
        Ok(_) => {}
        Err(err) => {
            eprintln!("Failed to send SIGINT:{}", err)
        }
    }

}

use tokio::task::JoinHandle;

fn report_event_records(events: RwLockReadGuard<Vec<(DateTime<Utc>, CentralEvent)>>, target_uuid_cloned:Option<TargetUuid<Uuid>>) {
    for (_date_time, event) in events.iter() {
        match event {
            CentralEvent::ManufacturerDataAdvertisement { id, .. }
            | CentralEvent::ServicesAdvertisement { id, .. }
            | CentralEvent::ServiceDataAdvertisement { id, .. }
            | CentralEvent::DeviceDiscovered(id)
            | CentralEvent::DeviceConnected(id)
            | CentralEvent::DeviceDisconnected(id)
            | CentralEvent::DeviceUpdated(id) => {
                if target_uuid_contains(&target_uuid_cloned, &id) {
                    println!("    {:?}", event);
                }
            }
            CentralEvent::StateUpdate(_state) => {
            }
        }
    }
}

fn log_event_records(events: RwLockReadGuard<Vec<(DateTime<Utc>, CentralEvent)>>, target_uuid_cloned:Option<TargetUuid<Uuid>>) {
    let mut reported : HashSet<PeripheralId> = HashSet::new();
    for (date_time, event) in events.iter() {
        match event {
            CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                if target_uuid_contains(&target_uuid_cloned, &id) && !reported.contains(&id){
                    reported.insert(id.clone());
                    for (key, value) in service_data.into_iter() {
                        println!("{:?} {}:{:?} {:?}", date_time, &id.to_string(), key, value);
                    }
                }
            }
            _ => {}

        }
    }
}

fn measure_record_interval(events: RwLockReadGuard<Vec<(DateTime<Utc>, CentralEvent)>>, target_uuid_cloned:Option<TargetUuid<Uuid>>) {
    let mut time_records: HashMap<PeripheralId, (DateTime<Utc>, Option<TimeDelta>)>
        = HashMap::new();
    for (date_time, event) in events.iter() {
        match event {
            CentralEvent::ManufacturerDataAdvertisement { id, .. } => {
                if target_uuid_contains(&target_uuid_cloned, &id){
                    match time_records.get(id) {
                        Some((last_date_time, Some(min_diff))) => {
                            let diff = *date_time - last_date_time;

                            if *min_diff > diff {
                                time_records.insert(id.clone(), (*date_time, Some(diff)));
                            } else {
                                time_records.insert(id.clone(), (*date_time, Some(*min_diff)));
                            }
                        }
                        Some((last_date_time, None)) => {
                            let diff = *date_time - last_date_time;
                            time_records.insert(id.clone(), (*date_time, Some(diff)));
                        }
                        None => {
                            time_records.insert(id.clone(), (*date_time, None));
                        }
                    }
                }
            }
            _ => {}

        }
    }
    for (id, min_diff) in time_records {
        println!("{:?}: {:?}", id, min_diff.1);
    }
}




/// Saves the events in YAML format by converting them to the `SerializableEvent` type.
fn save_event_records(
    file_path: &str,
    events: RwLockReadGuard<Vec<(DateTime<Utc>, CentralEvent)>>
) {
    // Convert each (DateTime<Utc>, CentralEvent) to our serializable enum
    let serializable_events: Vec<CentralEvent> = events.clone()
        .into_iter()
        .map(|(dt, e)| e.clone())
        .collect();

    // Attempt to serialize and write to file
    if let Ok(mut file) = File::create(file_path) {
        if let Ok(yaml_str) = serde_yaml::to_string(&serializable_events) {
            let _ = file.write_all(yaml_str.as_bytes());
        }
    }
}



/// Loads the events from a YAML file, deserializing them into a `Vec<CentralEvent>`.
fn load_event_records(file_path: &str) -> Result<Vec<CentralEvent>, Box<dyn std::error::Error>> {
    // Open the file for reading
    let mut file = File::open(file_path)?;

    // Read the entire file content into a string
    let mut yaml_str = String::new();
    file.read_to_string(&mut yaml_str)?;

    // Deserialize the YAML string into a Vec<CentralEvent>
    let events: Vec<CentralEvent> = serde_yaml::from_str(&yaml_str)?;

    Ok(events)
}


async fn handle_signal(
    cmd: Command,
    clone_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>>,
    target_uuid_cloned: Option<TargetUuid<Uuid>>,  // Replace TargetUuidType with the actual type
) {
    // Wait for Ctrl+C (SIGINT)
    signal::ctrl_c().await.expect("Failed to listen for SIGINT");

    let events = clone_records.read().await;


    match cmd {
        Command::Monitor{file} => {
            if let Some(file) = file {
                save_event_records(&file, events)
            } else {
                report_event_records(events, target_uuid_cloned);
            }
        }
        Command::Log => {
            log_event_records(events, target_uuid_cloned);
        }
        Command::MeasureInterval => {
            measure_record_interval(events, target_uuid_cloned);
        }
        _ => {}
    }
}




#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>>{
    pretty_env_logger::init();
    let cli = Cli::parse();
    let target_uuid = read_uuid_file(cli.uuid_file.as_deref()).await?;
    let manager = Manager::new().await?;
    let event_records: Arc<RwLock<Vec<(DateTime<Utc>, CentralEvent)>>> =
        Arc::new(RwLock::new(Vec::new()));
    let clone_records = Arc::clone(&event_records);
    let target_uuid_cloned = target_uuid.clone();
    let signal_handler: JoinHandle<()> =
        tokio::spawn(handle_signal(cli.command.clone(), clone_records, target_uuid_cloned));

    let wait_secs = cli.scan_secs;
    let app_task = tokio::spawn(async move {
        let _ = match &cli.command {
            Command::Scan => {
                scan(&manager, cli.scan_secs, &target_uuid).await
            }
            Command::MeasureInterval| Command::Monitor{..} | Command::Log => {
                if cli.scan_secs > 0 {
                    tokio::spawn(spawn_killer(wait_secs));
                }

                monitor(&manager, &target_uuid, event_records).await
            }
            Command::SendRequest { name, nth } => {
                println!("{:?}", &cli.command);
                if let Some(target_uuid) = target_uuid {
                    let _ = send_request(&manager, cli.scan_secs, &target_uuid, name, *nth).await;
                } else {
                    eprintln!("SendRequest Error");
                }
                Ok(())
            }
            Command::Load{file} => {
                let events = load_event_records(file);
                println!("{:?}", events);
                Ok(())
            }
            Command::AnalyzeEvent {file, message_type} => {
                if let Ok(events) = load_event_records(file) {
                    let filtered_events:Vec<CentralEvent> = events.into_iter()
                        .filter_map(|event| get_peripheral_id(&event, target_uuid.clone()).map(|_| event))
                        .filter_map(|event|
                            if Some(message_type) == get_message_type(&event).as_ref() {
                                Some(event)
                            } else {
                                None
                            }
                        )
                       .collect();
                    for event in filtered_events {
                        println!("{:?}", &event);
                    }
                } else {
                    eprintln!("load_event_records() failed")
                }
                Ok(())

            }

        };
    });

    // Wait for either the signal handler to complete, or the app task to complete
    tokio::select! {
        _ = signal_handler => {
            println!("Terminating program due to SIGINT...");
        }
        res = app_task => {
            // Handle errors in the application
            if let Err(err) = res {
                eprintln!("Application error: {}", err);
            }
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
"#;

        // Attempt to deserialize the YAML into the TargetUuid<String> struct
        let result: TargetUuid<String> = serde_yaml::from_str(yaml_data).expect("Failed to deserialize YAML");
        println!("{:?}", result);

        // Define the expected struct value for comparison
        let expected = TargetUuid {
            service_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            peripheral_uuids: vec![
                "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
                "123e4567-e89b-12d3-a456-426614174000".to_string(),
            ],
            requests: HashMap::from([])
        };

        // Assert the deserialized struct matches the expected value
        assert_eq!(result, expected);
    }
}