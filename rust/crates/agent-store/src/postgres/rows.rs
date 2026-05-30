use std::str::FromStr;

use agent_session::StoredTranscriptEntry;
use anyhow::Result;
use sqlx::{postgres::PgRow, Row};

use crate::{EventFrame, EventType};

pub(super) fn row_to_event(row: PgRow) -> Result<EventFrame> {
    Ok(EventFrame {
        event_id: row.get("id"),
        session_id: row.get("session_id"),
        event: row_text::<EventType>(&row, "type")?,
        data: row.get("payload"),
    })
}

pub(super) fn row_text<T>(row: &PgRow, column: &'static str) -> Result<T>
where
    T: FromStr<Err = String>,
{
    parse_text(row.get(column))
}

fn parse_text<T>(value: String) -> Result<T>
where
    T: FromStr<Err = String>,
{
    value.parse().map_err(anyhow::Error::msg)
}

pub(super) fn row_to_stored_entry(row: &PgRow) -> Result<StoredTranscriptEntry> {
    Ok(StoredTranscriptEntry {
        id: row.get("id"),
        parent_id: row.get("parent_id"),
        timestamp_ms: row.get::<i64, _>("timestamp_ms") as u64,
        item: serde_json::from_value(row.get("item"))?,
        provider_replay: serde_json::from_value(row.get("provider_replay"))?,
    })
}
