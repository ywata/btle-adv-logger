use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AdStoreError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

pub trait AdStore<'a, T>: Send + Sync
where
    T: Serialize + Deserialize<'a>,
{
    fn init(&self) -> Result<(), AdStoreError>;
    fn store_event(&self, event: &T) -> Result<(), AdStoreError>;
    fn load_event(&self) -> Result<Vec<T>, AdStoreError>;
}
