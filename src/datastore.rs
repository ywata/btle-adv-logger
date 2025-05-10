use btleplug::api::CentralEvent;
use thiserror::Error;
use serde::{Serialize, Deserialize};

#[derive(Error, Debug)]
pub enum AdStoreError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

pub trait AdStore<'a, T>: Send + Sync
where
    T: Serialize + Deserialize<'a>
{
    fn init(&self) -> Result<(), AdStoreError>;
    fn store_event(&self, event: &CentralEvent) -> Result<(), AdStoreError>;
    fn load_event(&self) -> Result<Vec<CentralEvent>, AdStoreError>;
}
