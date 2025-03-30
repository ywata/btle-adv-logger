use serde::ser::StdError;
use rusqlite::{params, Connection, Result};
use std::sync::Mutex;
use crate::AdStore;

pub struct SqliteAdStore {
    conn: Mutex<Connection>,
}

impl SqliteAdStore {
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        Ok(SqliteAdStore { conn: Mutex::new(conn) })
    }
}

impl AdStore for SqliteAdStore {
fn init(&self, db_path: &str) -> Result<(), Box<dyn StdError + 'static>> {
    let conn = self.conn.lock().unwrap();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ADVERTISEMENT (
            id INTEGER PRIMARY KEY,
            type TEXT NOT NULL,
            data TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

    fn save_event(&self, event_type: &str, event_data: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ADVERTISEMENT (type, data) VALUES (?1, ?2)",
            params![event_type, event_data],
        )?;
        Ok(())
    }
}