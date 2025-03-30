use serde::Deserialize;
use clap::ValueEnum;
use std::error;

pub trait AdStore: Send + Sync {
    fn save_event(&self, event_type: &str, event_data: &str) -> rusqlite::Result<()>;
    fn init(&self, path: &str) -> Result<(), Box<dyn std::error::Error>>;
}

