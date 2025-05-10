use crate::datastore::AdStoreError;
use crate::AdStore;
use btleplug::api::CentralEvent;
use rusqlite::{params, Connection, Result};
use std::sync::Mutex;

pub struct SqliteAdStore {
    conn: Mutex<Connection>,
}

impl SqliteAdStore {
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        Ok(SqliteAdStore {
            conn: Mutex::new(conn),
        })
    }
}

impl AdStore<'_, CentralEvent> for SqliteAdStore {
    fn init(&self) -> Result<(), AdStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS CentralEvents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                peripheral_id TEXT NOT NULL,
                data TEXT
            );
            "#,
            [],
        )?;
        Ok(())
    }

    // To store an event using json serialization
    fn store_event(&self, event: &CentralEvent) -> Result<(), AdStoreError> {
        let conn = self.conn.lock().unwrap();
        let json_data = serde_json::to_string(event)?;
        conn.execute(
            "INSERT INTO CentralEvents (event_type, peripheral_id, data) VALUES (?1, ?2, ?3)",
            params!["", "", json_data],
        )?;
        Ok(())
    }
    fn load_event(&self) -> Result<Vec<CentralEvent>, AdStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM CentralEvents")?;
        let event_iter = stmt.query_map([], |row| {
            let json_data: String = row.get(0)?;
            // Deserialize to intermediate type
            let event: CentralEvent = serde_json::from_str(&json_data)
                .map_err(AdStoreError::SerializationError)
                .expect("Failed to deserialize event");
            log::info!("Loaded event: {:?}", event);
            Ok(event)
        })?;

        let mut events = Vec::new();
        for event_result in event_iter {
            events.push(event_result?);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {

}
