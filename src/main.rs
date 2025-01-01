use uuid::Uuid;
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

use btleplug::api::{Central, CentralState, Characteristic, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::{Manager, PeripheralId};
use pretty_env_logger::env_logger::Target;
use serde::Deserialize;

#[derive(Parser)]
#[command(about = "BLE inspection tool", long_about = None)]
struct Cli {
    #[arg(long)]
    uuid_file:Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command{
    Scan,
}

#[derive(Deserialize, Debug, PartialEq)]
struct TargetUuid<T>{
    service_uuid: T,
    peripheral_uuids: Vec<T>,
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

async fn scan(manager:&Manager, target_uuid:&Option<TargetUuid<Uuid>> ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        time::sleep(Duration::from_secs(10)).await;
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


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>>{
    pretty_env_logger::init();
    let cli = Cli::parse();
    let target_uuid = read_uuid_file(cli.uuid_file.as_deref()).await?;
    let manager = Manager::new().await?;

    let app_task = tokio::spawn(async move {
        match &cli.command {
            Command::Scan => {
                scan(&manager, &target_uuid).await
            }
        };
    });

    // Wait for either the signal handler to complete, or the app task to complete
    tokio::select! {
        //_ = signal_handler => {
        //    println!("Terminating program due to SIGINT...");
        //}
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
        };

        // Assert the deserialized struct matches the expected value
        assert_eq!(result, expected);
    }
}