//! Import the supported native paginated-history projection for planned conversations only.
use super::{
    attachments,
    error::{ErrorCode, RehomeError},
    history_export::{requires_projection, validate_schema, TABLES},
    models::{PlannedSession, ReferenceRewrite},
    session_stream::{visit_jsonl, MAX_RECORD_BYTES},
};
use rusqlite::{
    params_from_iter, types::Value as SqlValue, Connection, OpenFlags, TransactionBehavior,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

const MAX_ORDINAL_REFERENCES: usize = 1_000_000;

#[derive(Deserialize)]
struct HistoryRow {
    table: String,
    thread_id: Uuid,
    columns: Map<String, Value>,
}

struct Positions {
    ranges: BTreeMap<i64, (i64, i64)>,
    next_byte: i64,
    next_ordinal: i64,
    committed: bool,
    requires_turns: bool,
    requires_realtime: bool,
}

pub(crate) fn validate_database(database: &Path) -> Result<(), RehomeError> {
    super::bridge::reject_hard_linked_sqlite(database)?;
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(failed)?;
    validate_schema(&connection)
}

/// Validate and spool bounded rows before changing SQL; failures roll back the whole projection.
pub(crate) fn merge(
    database: &Path,
    payload: impl Read,
    sessions: &[PlannedSession],
    rewrites: &[ReferenceRewrite],
) -> Result<(), RehomeError> {
    super::bridge::reject_hard_linked_sqlite(database)?;
    let planned = sessions
        .iter()
        .map(|session| (session.source_task_id, session))
        .collect::<HashMap<_, _>>();
    if planned.len() != sessions.len() {
        return Err(failed("duplicate planned history owners"));
    }
    let mut connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(failed)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(failed)?;
    connection
        .pragma_update(None, "trusted_schema", false)
        .map_err(failed)?;
    validate_schema(&connection)?;
    let mut spool = tempfile::tempfile().map_err(failed)?;
    let mut required: BTreeMap<Uuid, BTreeSet<i64>> = BTreeMap::new();
    let mut counts: BTreeMap<(Uuid, String), usize> = BTreeMap::new();
    let mut ordinal_count = 0;
    visit_jsonl(payload, |bytes, value| {
        let row = validate_row(value)?;
        if !planned.contains_key(&row.thread_id) {
            return Ok(());
        }
        let ordinals = required.entry(row.thread_id).or_default();
        for field in [
            "rollout_ordinal",
            "rollout_end_ordinal",
            "updated_at_ordinal",
        ] {
            if let Some(value) = row.columns.get(field).filter(|value| !value.is_null()) {
                let ordinal = value.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
                    failed("history ordinal must be a nonnegative SQLite integer")
                })?;
                if ordinals.insert(ordinal) {
                    ordinal_count += 1;
                }
            }
        }
        if ordinal_count > MAX_ORDINAL_REFERENCES {
            return Err(failed("history ordinal metadata exceeds the parser limit"));
        }
        *counts.entry((row.thread_id, row.table)).or_default() += 1;
        spool.write_all(bytes).map_err(failed)?;
        spool.write_all(b"\n").map_err(failed)?;
        Ok(())
    })?;
    let mut positions = HashMap::new();
    for session in sessions {
        let needed = required.remove(&session.source_task_id).unwrap_or_default();
        let offsets = rollout_positions(&session.target, &needed)?;
        if (offsets.requires_turns
            && counts
                .get(&(session.source_task_id, "thread_turns".into()))
                .copied()
                .unwrap_or(0)
                == 0)
            || (offsets.requires_realtime
                && counts
                    .get(&(session.source_task_id, "thread_realtime_items".into()))
                    .copied()
                    .unwrap_or(0)
                    == 0)
            || (offsets.committed
                && counts
                    .get(&(
                        session.source_task_id,
                        "thread_history_projection_state".into(),
                    ))
                    .copied()
                    .unwrap_or(0)
                    != 1)
        {
            return Err(failed(
                "committed conversation has an incomplete paginated-history projection",
            ));
        }
        if counts
            .get(&(
                session.source_task_id,
                "thread_history_projection_state".into(),
            ))
            .copied()
            .unwrap_or(0)
            > 1
        {
            return Err(failed("duplicate history projection state"));
        }
        positions.insert(session.source_task_id, offsets);
    }
    spool.seek(SeekFrom::Start(0)).map_err(failed)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(failed)?;
    for session in sessions {
        // The cleanup trigger on projection state may remove realtime rows too.
        for (table, _) in TABLES {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE thread_id=?"),
                    [session.target_task_id.to_string()],
                )
                .map_err(failed)?;
        }
    }
    visit_jsonl(spool, |_, value| {
        let mut row = validate_row(value)?;
        let session = planned
            .get(&row.thread_id)
            .ok_or_else(|| failed("spooled history has an unplanned owner"))?;
        let offsets = &positions[&row.thread_id];
        if row.table == "thread_turns" {
            let start = ordinal(&row.columns, "rollout_ordinal")?;
            row.columns.insert(
                "rollout_byte_offset".into(),
                offsets
                    .ranges
                    .get(&start)
                    .ok_or_else(|| {
                        failed("turn start ordinal is absent from the migrated rollout")
                    })?
                    .0
                    .into(),
            );
            if let Some(end) = row
                .columns
                .get("rollout_end_ordinal")
                .filter(|value| !value.is_null())
            {
                let end = end
                    .as_i64()
                    .ok_or_else(|| failed("invalid turn end ordinal"))?;
                if end < start {
                    return Err(failed("turn end precedes its start"));
                }
                row.columns.insert(
                    "rollout_end_byte_offset".into(),
                    offsets
                        .ranges
                        .get(&end)
                        .ok_or_else(|| {
                            failed("turn end ordinal is absent from the migrated rollout")
                        })?
                        .1
                        .into(),
                );
            } else {
                row.columns
                    .insert("rollout_end_byte_offset".into(), Value::Null);
            }
        }
        if row.table == "thread_history_projection_state" {
            if ordinal(&row.columns, "next_rollout_ordinal")? != offsets.next_ordinal {
                return Err(failed(
                    "history projection is incomplete for the migrated rollout",
                ));
            }
            row.columns
                .insert("next_rollout_byte_offset".into(), offsets.next_byte.into());
            row.columns
                .insert("next_rollout_ordinal".into(), offsets.next_ordinal.into());
        }
        if let Some(item) = row.columns.get_mut("item_json") {
            let text = item
                .as_str()
                .ok_or_else(|| failed("history item_json must be a string"))?;
            let mut parsed: Value = serde_json::from_str(text).map_err(failed)?;
            let before = parsed.clone();
            let selected = rewrites
                .iter()
                .filter(|rewrite| rewrite.source_task_id == session.source_task_id)
                .collect::<Vec<_>>();
            attachments::rewrite_record(&mut parsed, &selected);
            if parsed != before {
                *item = serde_json::to_string(&parsed).map_err(failed)?.into();
            }
        }
        row.columns.insert(
            "thread_id".into(),
            session.target_task_id.to_string().into(),
        );
        let columns = table_columns(&row.table)?;
        let values = columns
            .iter()
            .map(|column| sql_value(&row.columns[*column]))
            .collect::<Result<Vec<_>, _>>()?;
        let placeholders = vec!["?"; columns.len()].join(",");
        transaction
            .execute(
                &format!(
                    "INSERT INTO {} ({}) VALUES ({placeholders})",
                    row.table,
                    columns.join(",")
                ),
                params_from_iter(values),
            )
            .map_err(failed)?;
        Ok(())
    })?;
    transaction.commit().map_err(failed)
}

fn table_columns(table: &str) -> Result<&'static [&'static str], RehomeError> {
    TABLES
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, columns)| *columns)
        .ok_or_else(|| failed("unsupported history table"))
}

fn validate_row(value: &Value) -> Result<HistoryRow, RehomeError> {
    let row: HistoryRow = serde_json::from_value(value.clone()).map_err(failed)?;
    let columns = table_columns(&row.table)?;
    if row.columns.len() != columns.len()
        || columns
            .iter()
            .any(|column| !row.columns.contains_key(*column))
    {
        return Err(failed(
            "history row columns do not match the supported schema",
        ));
    }
    if row.columns.get("thread_id").and_then(Value::as_str)
        != Some(row.thread_id.to_string().as_str())
    {
        return Err(failed("history row owner does not match its columns"));
    }
    for value in row.columns.values() {
        sql_value(value)?;
    }
    Ok(row)
}

fn ordinal(columns: &Map<String, Value>, field: &str) -> Result<i64, RehomeError> {
    columns
        .get(field)
        .and_then(Value::as_i64)
        .filter(|ordinal| *ordinal >= 0)
        .ok_or_else(|| failed("invalid history ordinal"))
}

fn sql_value(value: &Value) -> Result<SqlValue, RehomeError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(text) => Ok(SqlValue::Text(text.clone())),
        Value::Number(number) => number
            .as_i64()
            .map(SqlValue::Integer)
            .ok_or_else(|| failed("history value is outside SQLite integer range")),
        _ => Err(failed(
            "history SQL values must be strings, integers or null",
        )),
    }
}

fn rollout_positions(path: &Path, required: &BTreeSet<i64>) -> Result<Positions, RehomeError> {
    let mut reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut line = Vec::new();
    let mut result = Positions {
        ranges: BTreeMap::new(),
        next_byte: 0,
        next_ordinal: 0,
        committed: false,
        requires_turns: false,
        requires_realtime: false,
    };
    let mut previous = None;
    loop {
        line.clear();
        let count = Read::by_ref(&mut reader)
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(failed)?;
        if count == 0 {
            break;
        }
        if count > MAX_RECORD_BYTES {
            return Err(failed("migrated rollout record exceeds the parser limit"));
        }
        let end = result
            .next_byte
            .checked_add(i64::try_from(count).map_err(failed)?)
            .ok_or_else(|| failed("rollout byte offset overflow"))?;
        let bytes = if result.next_byte == 0 {
            line.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&line)
        } else {
            &line
        };
        let record: Value = serde_json::from_slice(bytes).map_err(failed)?;
        let ordinal = record
            .get("ordinal")
            .and_then(Value::as_i64)
            .filter(|ordinal| *ordinal >= 0)
            .ok_or_else(|| failed("paginated rollout has a missing or invalid ordinal"))?;
        if previous.is_some_and(|value| ordinal <= value) {
            return Err(failed("paginated rollout ordinals are not increasing"));
        }
        previous = Some(ordinal);
        if required.contains(&ordinal) {
            result.ranges.insert(ordinal, (result.next_byte, end));
        }
        result.next_byte = end;
        result.next_ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| failed("rollout ordinal overflow"))?;
        if requires_projection(&record) {
            result.committed = true;
            if record.get("type").and_then(Value::as_str) == Some("realtime_item") {
                result.requires_realtime = true;
            } else {
                result.requires_turns = true;
            }
        }
    }
    if result.ranges.len() != required.len() {
        return Err(failed(
            "history references an ordinal absent from the migrated rollout",
        ));
    }
    Ok(result)
}

fn failed(error: impl std::fmt::Display) -> RehomeError {
    RehomeError::new(
        ErrorCode::RestoreFailed,
        format!("Codex paginated history import: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{ReferenceRewriteKind, SessionAction};
    use serde_json::json;

    fn schema(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        for (table, columns) in TABLES {
            let fields = columns
                .iter()
                .map(|column| {
                    let sql_type = if matches!(
                        *column,
                        "thread_id"
                            | "turn_id"
                            | "item_id"
                            | "status"
                            | "error_json"
                            | "first_user_item_id"
                            | "final_agent_item_id"
                            | "root_turn_id"
                            | "item_json"
                            | "item_type"
                    ) {
                        "TEXT"
                    } else {
                        "INTEGER"
                    };
                    format!("{column} {sql_type}")
                })
                .collect::<Vec<_>>();
            connection
                .execute_batch(&format!("CREATE TABLE {table} ({})", fields.join(",")))
                .unwrap();
        }
        connection
    }

    fn row(table: &str, owner: Uuid, fields: Value) -> Value {
        let mut columns = table_columns(table)
            .unwrap()
            .iter()
            .map(|field| ((*field).into(), Value::Null))
            .collect::<Map<_, _>>();
        columns.insert("thread_id".into(), owner.to_string().into());
        for (field, value) in fields.as_object().unwrap() {
            columns.insert(field.clone(), value.clone());
        }
        json!({"table":table,"thread_id":owner,"columns":columns})
    }

    fn fixture(root: &Path) -> (PlannedSession, Vec<u8>, Vec<(i64, i64)>) {
        let owner = Uuid::new_v4();
        let records = [
            json!({"ordinal":14,"type":"session_meta","payload":{"id":owner,"history_mode":"paginated","history_base":{"end_ordinal_exclusive":14},"cwd":"a remapped target root"}}),
            json!({"ordinal":15,"type":"event_msg","payload":{"type":"task_started"}}),
            json!({"ordinal":16,"type":"event_msg","payload":{"type":"item_completed"}}),
            json!({"ordinal":17,"type":"event_msg","payload":{"type":"task_complete"}}),
        ];
        let mut bytes = Vec::new();
        let mut offsets = Vec::new();
        for record in records {
            let start = bytes.len() as i64;
            writeln!(bytes, "{record}").unwrap();
            offsets.push((start, bytes.len() as i64));
        }
        let target = root.join("rollout.jsonl");
        std::fs::write(&target, &bytes).unwrap();
        let session = PlannedSession {
            package_source: "codex/sessions/rollout.jsonl".into(),
            target,
            source_task_id: owner,
            target_task_id: Uuid::new_v4(),
            title: "Synthetic".into(),
            source_content_hash: String::new(),
            expected_final_content_hash: String::new(),
            action: SessionAction::Import,
        };
        (session, bytes, offsets)
    }

    fn payload(session: &PlannedSession, bad_item: bool) -> Vec<u8> {
        let rows = [
            row(
                "thread_turns",
                session.source_task_id,
                json!({"turn_id":"turn","rollout_ordinal":15,"status":"completed","rollout_byte_offset":999,"rollout_end_ordinal":17,"rollout_end_byte_offset":999}),
            ),
            row(
                "thread_items",
                session.source_task_id,
                json!({"turn_id":"turn","item_id":"item","rollout_ordinal":16,"created_at_ms":1,"item_type":"userMessage","updated_at_ordinal":16,"item_json":if bad_item {"bad JSON".to_owned()} else {json!({"type":"userMessage","content":[{"type":"text","text":"keep /old/image.png"},{"type":"localImage","path":"/old/image.png"}]}).to_string()}}),
            ),
            row(
                "thread_history_projection_state",
                session.source_task_id,
                json!({"next_rollout_byte_offset":999,"next_rollout_ordinal":18}),
            ),
        ];
        let mut bytes = Vec::new();
        for row in rows {
            writeln!(bytes, "{row}").unwrap();
        }
        bytes
    }

    #[test]
    fn merges_selected_fork_projection_with_local_offsets_and_scoped_image_paths() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread_history_1.sqlite");
        let connection = schema(&database);
        let unrelated = Uuid::new_v4().to_string();
        connection
            .execute(
                "INSERT INTO thread_history_projection_state VALUES (?,123,7)",
                [&unrelated],
            )
            .unwrap();
        drop(connection);
        let (session, bytes, offsets) = fixture(root.path());
        let rewrite = ReferenceRewrite {
            source_task_id: session.source_task_id,
            package_source: session.package_source.clone(),
            kind: ReferenceRewriteKind::AttachmentPath,
            from: "/old/image.png".into(),
            to: "/new/image.png".into(),
        };
        merge(
            &database,
            payload(&session, false).as_slice(),
            std::slice::from_ref(&session),
            &[rewrite],
        )
        .unwrap();
        let connection = Connection::open(&database).unwrap();
        let actual:(i64,i64)=connection.query_row("SELECT rollout_byte_offset,rollout_end_byte_offset FROM thread_turns WHERE thread_id=?",[session.target_task_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(actual, (offsets[1].0, offsets[3].1));
        let next:(i64,i64)=connection.query_row("SELECT next_rollout_byte_offset,next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id=?",[session.target_task_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(next, (bytes.len() as i64, 18));
        let item: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE thread_id=?",
                [session.target_task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let item: Value = serde_json::from_str(&item).unwrap();
        assert_eq!(item["content"][0]["text"], "keep /old/image.png");
        assert_eq!(item["content"][1]["path"], "/new/image.png");
        assert_eq!(connection.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[unrelated],|row|row.get::<_,i64>(0)).unwrap(),123);
    }

    #[test]
    fn invalid_item_rolls_back_projection_replacement() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread_history_1.sqlite");
        let connection = schema(&database);
        let (session, _, _) = fixture(root.path());
        connection
            .execute(
                "INSERT INTO thread_history_projection_state VALUES (?,123,7)",
                [session.target_task_id.to_string()],
            )
            .unwrap();
        drop(connection);
        assert!(merge(
            &database,
            payload(&session, true).as_slice(),
            std::slice::from_ref(&session),
            &[]
        )
        .is_err());
        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT next_rollout_byte_offset FROM thread_history_projection_state",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            123
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM thread_turns", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn rejects_unrecognized_columns_and_preserves_unplanned_rows() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread_history_1.sqlite");
        let connection = schema(&database);
        let owner = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO thread_history_projection_state VALUES (?,123,7)",
                [owner.to_string()],
            )
            .unwrap();
        drop(connection);
        let unplanned = row(
            "thread_history_projection_state",
            owner,
            json!({"next_rollout_byte_offset":1,"next_rollout_ordinal":1}),
        );
        merge(&database, format!("{unplanned}\n").as_bytes(), &[], &[]).unwrap();
        let mut invalid = unplanned;
        invalid["columns"]["arbitrary_sql_column"] = 1.into();
        assert!(merge(&database, format!("{invalid}\n").as_bytes(), &[], &[]).is_err());
        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT next_rollout_byte_offset FROM thread_history_projection_state",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            123
        );
    }
}
