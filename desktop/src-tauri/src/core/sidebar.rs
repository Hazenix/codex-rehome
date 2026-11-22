//! Selected conversations carry their sidebar state through the existing DB transaction.
use super::error::{ErrorCode, RehomeError};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use uuid::Uuid;

pub(crate) const DESCRIPTOR: &str = "_rehome_section";
const PINNED_SECTION: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";
const THREAD_FIELDS: &[&str] = &[
    "name",
    "is_pinned",
    "thread_section_id",
    "section_position",
    "section_entered_at_ms",
];

fn invalid(message: impl Into<String>) -> RehomeError {
    RehomeError::new(ErrorCode::PackageInvalid, message)
}

fn failure(error: impl std::fmt::Display) -> RehomeError {
    RehomeError::new(
        ErrorCode::RestoreFailed,
        format!("Codex sidebar metadata: {error}"),
    )
}

fn columns(db: &Connection, table: &str) -> Result<HashSet<String>, RehomeError> {
    let mut statement = db
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(failure)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(failure)?;
    names.collect::<Result<HashSet<_>, _>>().map_err(failure)
}

/// Export only a section referenced by a selected thread, never the whole sidebar.
pub(crate) fn descriptor(db: &Connection, id: &str) -> Result<Value, RehomeError> {
    let available = columns(db, "thread_sections")?;
    if !available.contains("id") || !available.contains("name") {
        return Err(failure("source thread section table is unavailable"));
    }
    let appearance = if available.contains("appearance") {
        ", appearance"
    } else {
        ""
    };
    let mut statement = db
        .prepare(&format!(
            "SELECT name{appearance} FROM thread_sections WHERE id=?1"
        ))
        .map_err(failure)?;
    let section = statement
        .query_row([id], |row| {
            let mut value = json!({"id":id,"name":row.get::<_, String>(0)?});
            if available.contains("appearance") {
                value["appearance"] = row
                    .get::<_, Option<String>>(1)?
                    .map(Value::String)
                    .unwrap_or(Value::Null);
            }
            Ok(value)
        })
        .optional()
        .map_err(failure)?
        .ok_or_else(|| failure("selected conversation references a missing thread section"))?;
    validate_descriptor(&section, id)?;
    Ok(section)
}

fn validate_descriptor<'a>(
    value: &'a Value,
    expected_id: &str,
) -> Result<&'a Map<String, Value>, RehomeError> {
    let section = value
        .as_object()
        .ok_or_else(|| invalid("invalid thread section descriptor"))?;
    let id = section
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| *id == expected_id && Uuid::parse_str(id).is_ok())
        .ok_or_else(|| {
            invalid("thread section descriptor identity does not match its conversation")
        })?;
    let name = section
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 16 * 1024)
        .ok_or_else(|| invalid("invalid thread section name"))?;
    if id == PINNED_SECTION && name != "Pinned" {
        return Err(invalid("built-in pinned thread section cannot be renamed"));
    }
    if let Some(appearance) = section.get("appearance") {
        if !appearance.is_null()
            && !appearance.as_str().is_some_and(|value| {
                value.len() <= 16 * 1024 && serde_json::from_str::<Value>(value).is_ok()
            })
        {
            return Err(invalid("invalid thread section appearance"));
        }
    }
    Ok(section)
}

fn validate_thread_fields(row: &Map<String, Value>) -> Result<(), RehomeError> {
    for field in THREAD_FIELDS {
        let Some(value) = row.get(*field) else {
            continue;
        };
        let valid = match *field {
            "name" => value.is_null() || value.as_str().is_some_and(|name| name.len() <= 16 * 1024),
            "is_pinned" => value.as_i64().is_some_and(|value| value == 0 || value == 1),
            "thread_section_id" => {
                value.is_null() || value.as_str().is_some_and(|id| Uuid::parse_str(id).is_ok())
            }
            _ => value.is_null() || value.as_i64().is_some(),
        };
        if !valid {
            return Err(invalid(format!("invalid sidebar field {field}")));
        }
    }
    Ok(())
}

fn absent_default(field: &str, value: &Value) -> bool {
    value.is_null() || (field == "is_pinned" && value == &json!(0))
}

pub(crate) fn validate_rows(rows: &[Map<String, Value>]) -> Result<(), RehomeError> {
    let mut sections = std::collections::HashMap::new();
    for row in rows {
        validate_thread_fields(row)?;
        if let Some(value) = row.get(DESCRIPTOR) {
            let id = row
                .get("thread_section_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("thread section descriptor has no matching membership"))?;
            validate_descriptor(value, id)?;
            if sections
                .insert(id, value)
                .is_some_and(|previous| previous != value)
            {
                return Err(invalid(
                    "selected conversations disagree about their shared thread section",
                ));
            }
        }
    }
    Ok(())
}

/// Keep section creation and membership changes in the caller's backed-up transaction.
pub(crate) fn prepare_import(
    db: &Connection,
    row: &mut Map<String, Value>,
) -> Result<(), RehomeError> {
    validate_thread_fields(row)?;
    let available = columns(db, "threads")?;
    for field in THREAD_FIELDS {
        if !available.contains(*field)
            && row
                .get(*field)
                .is_some_and(|value| !absent_default(field, value))
        {
            return Err(failure(format!(
                "target Codex does not support sidebar field {field}"
            )));
        }
    }
    let section_id = row.get("thread_section_id").and_then(Value::as_str);
    let Some(section_id) = section_id else {
        if row.contains_key(DESCRIPTOR) {
            return Err(invalid(
                "thread section descriptor has no matching membership",
            ));
        }
        return Ok(());
    };
    let value = row
        .get(DESCRIPTOR)
        .ok_or_else(|| invalid("thread section descriptor is missing"))?;
    let section = validate_descriptor(value, section_id)?;
    let available = columns(db, "thread_sections")?;
    if !available.contains("id") || !available.contains("name") {
        return Err(failure("target thread section table is unavailable"));
    }
    let name = section["name"].as_str().expect("validated section name");
    // 同一账号沿用 section UUID；只更新已选会话引用的组，不删除目标独有组和成员。
    if let Some(appearance) = section.get("appearance") {
        if !available.contains("appearance") && !appearance.is_null() {
            return Err(failure(
                "target Codex does not support thread section appearance",
            ));
        }
        if available.contains("appearance") {
            db.execute("INSERT INTO thread_sections(id,name,appearance) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET name=excluded.name,appearance=excluded.appearance",
                (section_id, name, appearance.as_str())).map_err(failure)?;
            return Ok(());
        }
    }
    db.execute("INSERT INTO thread_sections(id,name) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET name=excluded.name", (section_id,name)).map_err(failure)?;
    Ok(())
}

pub(crate) fn verify_restored(
    db: &Connection,
    row: &Map<String, Value>,
) -> Result<bool, RehomeError> {
    validate_thread_fields(row)?;
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("sidebar member has no thread identity"))?;
    let available = columns(db, "threads")?;
    for field in THREAD_FIELDS {
        let Some(expected) = row.get(*field) else {
            continue;
        };
        if !available.contains(*field) {
            if absent_default(field, expected) {
                continue;
            }
            return Ok(false);
        }
        let current = db
            .query_row(
                &format!("SELECT {field} FROM threads WHERE id=?1"),
                [id],
                |row| {
                    Ok(match row.get_ref(0)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(value) => json!(value),
                        rusqlite::types::ValueRef::Text(value) => {
                            Value::String(String::from_utf8_lossy(value).into_owned())
                        }
                        _ => Value::Bool(false),
                    })
                },
            )
            .optional()
            .map_err(failure)?;
        if current.as_ref() != Some(expected) {
            return Ok(false);
        }
    }
    if let Some(value) = row.get(DESCRIPTOR) {
        let id = row
            .get("thread_section_id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("thread section descriptor has no matching membership"))?;
        let expected = validate_descriptor(value, id)?;
        if !columns(db, "thread_sections")?.contains("id") {
            return Ok(false);
        }
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_sections WHERE id=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(failure)?;
        if !exists {
            return Ok(false);
        }
        let actual = descriptor(db, id)?;
        for (field, value) in expected {
            if field == "appearance" && value.is_null() && actual.get(field).is_none() {
                continue;
            }
            if actual.get(field) != Some(value) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
