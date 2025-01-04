use std::num::ParseIntError;
use btleplug::api::CharPropFlags;
use futures::stream::StreamExt;
use uuid::Uuid;
use std::str::FromStr;
use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::RwLock;
use std::error::Error;
use std::fmt::Debug;
use std::time::Duration;
use tokio::{fs, time, signal};

use serde_yaml;
use clap::{Parser, Subcommand};

use btleplug::api::{Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::{Manager, PeripheralId};
use pretty_env_logger::env_logger::Target;
use serde::Deserialize;

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

#[derive(Subcommand, Debug)]
enum Command{
    Scan,
    Monitor,
    SendRequest{name: String, nth: usize},
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

                    return Some(r)
                } else {
                    return None;
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



async fn monitor(manager:&Manager, scan_secs: u64, target_uuid:&Option<TargetUuid<Uuid>>,
                 events_map: Arc<RwLock<HashMap<PeripheralId, Vec<CentralEvent>>>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        match event {
            CentralEvent::DeviceDiscovered(ref id) => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::DeviceConnected(ref id) => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::DeviceDisconnected(ref id)=> {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::ManufacturerDataAdvertisement {ref id, ref manufacturer_data} => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::DeviceUpdated(ref id) => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::DeviceDisconnected(ref id) => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::ServiceDataAdvertisement {ref id, ref service_data} => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);

            }
            CentralEvent::ServicesAdvertisement{ref id, ref services} => {
                let mut events_lock = events_map.write().await;
                let entry = events_lock.entry(id.clone()).or_insert_with(Vec::new);
                entry.push(event);
            }
            CentralEvent::StateUpdate(ref state) => {
                println!("{:?}", &event);
            }

        }
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
            let result = peripheral.subscribe(&notify).await?;
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
                    println!("{} is ignored skipping", local_name);;
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

                        peripheral.unsubscribe(&notify_char).await;



                        peripheral.disconnect().await?;
                    }
                } else {

                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>>{
    pretty_env_logger::init();
    let cli = Cli::parse();
    let target_uuid = read_uuid_file(cli.uuid_file.as_deref()).await?;
    let manager = Manager::new().await?;
    let events_map: Arc<RwLock<HashMap<PeripheralId, Vec<CentralEvent>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let map_clone = Arc::clone(&events_map);

    let target_uuid_cloned = target_uuid.clone();
    let signal_handler = tokio::spawn(async move {
        // Wait for Ctrl+C (SIGINT)
        signal::ctrl_c().await.expect("Failed to listen for SIGINT");
        let events = map_clone.read().await;
        for (id, event_list) in events.iter() {
            for event in event_list {
                match event {
                    CentralEvent::ManufacturerDataAdvertisement { id,../* manufacturer_data*/ }
                    | CentralEvent::ServicesAdvertisement {id,..}
                    | CentralEvent::ServiceDataAdvertisement { id,../*, service_data */}
                    | CentralEvent::DeviceDiscovered(id)
                    | CentralEvent::DeviceConnected(id)
                    | CentralEvent::DeviceDisconnected(id)
                    | CentralEvent::DeviceUpdated(id)
                    => {
                        if target_uuid_contains(&target_uuid_cloned, &id) {
                            println!("    {:?}", event);
                        }
                    }
                    CentralEvent::StateUpdate(state) => { }
                }
            }
        }
    });



    let app_task = tokio::spawn(async move {
        match &cli.command {
            Command::Scan => {
                scan(&manager, cli.scan_secs, &target_uuid).await
            }
            Command::Monitor => {
                monitor(&manager, cli.scan_secs, &target_uuid, events_map).await
            }
            Command::SendRequest { name, nth } => {
                println!("{:?}", &cli.command);
                if let Some(target_uuid) = target_uuid {
                    send_request(&manager, cli.scan_secs, &target_uuid, name, *nth).await;
                } else {
                    eprintln!("SendRequest Error");
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