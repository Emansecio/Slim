use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::schema_v2::{DurableSessionHeader, DURABLE_SCHEMA_VERSION};
use super::{SessionHeader, CURRENT_SCHEMA_VERSION};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionFormat {
    LegacyV1,
    DurableV2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInspection {
    pub format: SessionFormat,
    pub session_id: String,
}

pub fn inspect_session(path: impl AsRef<Path>) -> io::Result<SessionInspection> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(invalid_data("session file is empty"));
    }

    let value: Value = serde_json::from_str(line.trim_end()).map_err(invalid_data_from)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_data("session header must be an object"))?;
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(invalid_data("session header type must be session"));
    }
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_data("session header schema_version is invalid"))?;

    match schema_version {
        value if value == u64::from(CURRENT_SCHEMA_VERSION) => {
            let header: SessionHeader =
                serde_json::from_value(value_to_json(object)).map_err(invalid_data_from)?;
            if header.schema_version != CURRENT_SCHEMA_VERSION {
                return Err(invalid_data("legacy session schema_version is invalid"));
            }
            Ok(SessionInspection {
                format: SessionFormat::LegacyV1,
                session_id: header.id,
            })
        }
        value if value == u64::from(DURABLE_SCHEMA_VERSION) => {
            let header: DurableSessionHeader =
                serde_json::from_value(value_to_json(object)).map_err(invalid_data_from)?;
            if header.schema_version() != DURABLE_SCHEMA_VERSION {
                return Err(invalid_data("durable session schema_version is invalid"));
            }
            Ok(SessionInspection {
                format: SessionFormat::DurableV2,
                session_id: header.id,
            })
        }
        _ => Err(invalid_data("unknown session schema_version")),
    }
}

fn value_to_json(object: &serde_json::Map<String, Value>) -> Value {
    Value::Object(object.clone())
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_data_from(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
