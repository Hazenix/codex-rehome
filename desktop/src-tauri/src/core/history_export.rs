//! Export selected paginated-history projections without starting Codex or copying its database.
use super::{
    discovery::StateDatabaseSnapshot,
    error::{ErrorCode, RehomeError},
    models::ConversationEntry,
    session::session_metadata_from_value,
    session_stream::{visit_jsonl, MAX_RECORD_BYTES},
};
use rusqlite::{types::ValueRef, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub(crate) const HISTORY_SOURCE: &str = "codex/metadata/paginated-history.jsonl";

pub(super) const TABLES: &[(&str, &[&str])] = &[
    (
        "thread_turns",
        &[
            "thread_id",
            "turn_id",
            "rollout_ordinal",
            "status",
            "error_json",
            "started_at",
            "completed_at",
            "duration_ms",
            "first_user_item_id",
            "final_agent_item_id",
            "rollout_byte_offset",
            "rollout_end_ordinal",
            "rollout_end_byte_offset",
            "root_turn_id",
        ],
    ),
    (
        "thread_items",
        &[
            "thread_id",
            "turn_id",
            "item_id",
            "rollout_ordinal",
            "created_at_ms",
            "item_json",
            "item_type",
            "updated_at_ordinal",
            "started_at_ms",
            "completed_at_ms",
        ],
    ),
    (
        "thread_history_projection_state",
        &[
            "thread_id",
            "next_rollout_byte_offset",
            "next_rollout_ordinal",
        ],
    ),
    (
        "thread_realtime_items",
        &[
            "thread_id",
            "item_id",
            "rollout_ordinal",
            "created_at_ms",
            "item_type",
            "item_json",
        ],
    ),
];

fn invalid(error: impl std::fmt::Display) -> RehomeError {
    RehomeError::new(
        ErrorCode::PackageInvalid,
        format!("Codex paginated history: {error}"),
    )
}

struct SelectedHistory {
    id: Uuid,
    rollout_path: PathBuf,
    has_local_turn: bool,
    has_realtime: bool,
}

/// Match the records materialized by Codex's canonical history projectors.
pub(super) fn requires_projection(record: &Value) -> bool {
    match record.get("type").and_then(Value::as_str) {
        Some("realtime_item") => record.get("payload").is_some_and(Value::is_object),
        Some("event_msg") => {
            let payload = &record["payload"];
            match payload.get("type").and_then(Value::as_str) {
                Some(
                    "task_started" | "turn_started" | "task_complete" | "turn_complete"
                    | "item_completed",
                ) => true,
                Some("turn_aborted") => payload.get("turn_id").and_then(Value::as_str).is_some(),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Stage only projections belonging to selected paginated conversations, including archives.
pub(crate) fn stage(
    home: &Path,
    staging: &Path,
    chats: &[ConversationEntry],
) -> Result<Option<String>, RehomeError> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for chat in chats {
        if !seen.insert(chat.task_id) {
            return Err(invalid("selected conversation identities are duplicated"));
        }
        let path = safe_staged_rollout(staging, &chat.archive_path)?;
        let mut paginated = false;
        let mut has_local_turn = false;
        let mut has_realtime = false;
        visit_jsonl(File::open(&path).map_err(invalid)?, |_, value| {
            if let Some(metadata) = (value.get("type").and_then(Value::as_str)
                == Some("session_meta"))
            .then(|| session_metadata_from_value(value.clone()))
            .flatten()
            {
                if metadata.task_id != chat.task_id {
                    return Err(invalid(
                        "selected rollout identity does not match its conversation",
                    ));
                }
                paginated = metadata.fields.get("history_mode").and_then(Value::as_str)
                    == Some("paginated");
            }
            if requires_projection(value) {
                if value.get("type").and_then(Value::as_str) == Some("realtime_item") {
                    has_realtime = true;
                } else {
                    has_local_turn = true;
                }
            }
            Ok(())
        })?;
        if paginated {
            selected.push(SelectedHistory {
                id: chat.task_id,
                rollout_path: path,
                has_local_turn,
                has_realtime,
            });
        }
    }
    if selected.is_empty() {
        return Ok(None);
    }

    let database = home.join("thread_history_1.sqlite");
    if !database.is_file() {
        return Err(invalid("thread_history_1.sqlite is missing for selected paginated conversations; preserve the source history and rebuild it in Codex before exporting"));
    }
    let snapshot = StateDatabaseSnapshot::create(&database).map_err(invalid)?;
    let db = Connection::open_with_flags(
        snapshot.database_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(invalid)?;
    validate_schema(&db)?;

    let destination = staging.join(HISTORY_SOURCE);
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("history package path has no parent"))?;
    fs::create_dir_all(parent).map_err(invalid)?;
    let output = NamedTempFile::new_in(parent).map_err(invalid)?;
    let mut writer = HistoryWriter {
        output: BufWriter::new(output.reopen().map_err(invalid)?),
        hash: Sha256::new(),
        record_bytes: 0,
    };
    for chat in selected {
        validate_projection(&db, &chat)?;
        for (table, columns) in TABLES {
            let query = format!(
                "SELECT {} FROM {table} WHERE thread_id=?1 ORDER BY rowid",
                columns.join(",")
            );
            let mut statement = db.prepare(&query).map_err(invalid)?;
            let mut rows = statement.query([chat.id.to_string()]).map_err(invalid)?;
            while let Some(row) = rows.next().map_err(invalid)? {
                let mut values = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    values.insert(
                        (*column).to_owned(),
                        sql_value(row.get_ref(index).map_err(invalid)?)?,
                    );
                }
                let record =
                    json!({"table":table,"thread_id":chat.id.to_string(),"columns":values});
                writer.record_bytes = 0;
                serde_json::to_writer(&mut writer, &record).map_err(invalid)?;
                writer.write_all(b"\n").map_err(invalid)?;
            }
        }
    }
    writer.flush().map_err(invalid)?;
    let hash = format!("{:x}", writer.hash.clone().finalize());
    drop(writer);
    output.as_file().sync_all().map_err(invalid)?;
    output.persist_noclobber(&destination).map_err(invalid)?;
    Ok(Some(hash))
}

fn safe_staged_rollout(staging: &Path, archive: &str) -> Result<PathBuf, RehomeError> {
    let path = Path::new(archive);
    if !(archive.starts_with("codex/sessions/") || archive.starts_with("codex/archived_sessions/"))
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(invalid("conversation rollout has an unsafe package path"));
    }
    Ok(staging.join(path))
}

pub(super) fn validate_schema(db: &Connection) -> Result<(), RehomeError> {
    for (table, expected) in TABLES {
        let mut statement = db
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(invalid)?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(invalid)?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(invalid)?;
        if !columns.contains("thread_id") {
            return Err(invalid(format!("unsupported {table} schema: thread_id is required; rollout_id-only history migration is not supported")));
        }
        if columns.len() != expected.len()
            || expected.iter().any(|column| !columns.contains(*column))
        {
            return Err(invalid(format!("unsupported {table} schema: source history columns differ from the supported thread_id layout; update ReHome before exporting")));
        }
    }
    Ok(())
}

fn validate_projection(db: &Connection, chat: &SelectedHistory) -> Result<(), RehomeError> {
    let projection: Option<(i64, i64)> = db.query_row(
        "SELECT next_rollout_byte_offset,next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id=?1",
        [chat.id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional().map_err(invalid)?;
    let rollout_bytes = fs::metadata(&chat.rollout_path).map_err(invalid)?.len();
    if let Some((offset, ordinal)) = projection {
        if offset < 0 || ordinal < 0 || offset as u64 > rollout_bytes {
            return Err(invalid(format!(
                "projection offset does not match selected rollout {}",
                chat.id
            )));
        }
        if (chat.has_local_turn || chat.has_realtime) && offset as u64 != rollout_bytes {
            return Err(invalid(format!("projection is incomplete for persisted conversation {}; finish reading its history in the source Codex before exporting", chat.id)));
        }
    } else if chat.has_local_turn || chat.has_realtime {
        return Err(invalid(format!("projection is missing for persisted conversation {}; preserve the source history and repair it in Codex before exporting", chat.id)));
    }
    if chat.has_local_turn {
        let turns: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM thread_turns WHERE thread_id=?1",
                [chat.id.to_string()],
                |row| row.get(0),
            )
            .map_err(invalid)?;
        if turns == 0 {
            return Err(invalid(format!(
                "turn projection is missing for persisted conversation {}",
                chat.id
            )));
        }
    }
    if chat.has_realtime {
        let items: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM thread_realtime_items WHERE thread_id=?1",
                [chat.id.to_string()],
                |row| row.get(0),
            )
            .map_err(invalid)?;
        if items == 0 {
            return Err(invalid(format!(
                "realtime projection is missing for persisted conversation {}",
                chat.id
            )));
        }
    }
    Ok(())
}

fn sql_value(value: ValueRef<'_>) -> Result<Value, RehomeError> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => Value::Number(
            serde_json::Number::from_f64(value)
                .ok_or_else(|| invalid("non-finite history field"))?,
        ),
        ValueRef::Text(value) => {
            if value.len() > MAX_RECORD_BYTES {
                return Err(invalid("history field exceeds the per-record limit"));
            }
            Value::String(std::str::from_utf8(value).map_err(invalid)?.to_owned())
        }
        ValueRef::Blob(_) => {
            return Err(invalid(
                "binary fields are not supported in paginated-history projections",
            ))
        }
    })
}

struct HistoryWriter {
    output: BufWriter<File>,
    hash: Sha256,
    record_bytes: usize,
}

impl Write for HistoryWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.record_bytes.saturating_add(bytes.len()) > MAX_RECORD_BYTES {
            return Err(io::Error::other(
                "paginated-history JSON record exceeds the 128 MiB per-record limit",
            ));
        }
        let written = self.output.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.record_bytes += written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Vec<ConversationEntry>) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let staging = root.path().join("stage");
        fs::create_dir_all(&home).unwrap();
        let id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let archive = format!("codex/archived_sessions/{id}.jsonl");
        let path = staging.join(&archive);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":id,"history_mode":"paginated"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}})
        );
        fs::write(&path, &bytes).unwrap();
        let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
        for (table, columns) in TABLES {
            let fields = columns
                .iter()
                .map(|column| {
                    format!(
                        "{column} {}",
                        if column.contains("ordinal") || column.contains("offset") {
                            "INTEGER"
                        } else {
                            "TEXT"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            db.execute_batch(&format!("CREATE TABLE {table}({fields});"))
                .unwrap();
        }
        db.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1,?2,2)",
            (id.to_string(), bytes.len() as i64),
        )
        .unwrap();
        db.execute("INSERT INTO thread_turns(thread_id,turn_id,rollout_ordinal,status) VALUES (?1,'turn-1',1,'completed')",[id.to_string()]).unwrap();
        db.execute("INSERT INTO thread_items(thread_id,turn_id,item_id,rollout_ordinal,item_json) VALUES (?1,'turn-1','message-1',1,'{\"text\":\"你好\"}')",[id.to_string()]).unwrap();
        db.execute("INSERT INTO thread_turns(thread_id,turn_id,rollout_ordinal,status) VALUES ('22222222-2222-4222-8222-222222222222','private-turn',1,'completed')",[]).unwrap();
        drop(db);
        let chats = vec![ConversationEntry {
            task_id: id,
            project_id: None,
            title: "Archive".into(),
            updated_at: String::new(),
            content_hash: String::new(),
            archive_path: archive,
            classification: None,
        }];
        (root, home, staging, chats)
    }

    #[test]
    fn streams_only_selected_archive_projection_and_hashes_exact_bytes() {
        let (_root, home, staging, chats) = fixture();
        let hash = stage(&home, &staging, &chats).unwrap().unwrap();
        let bytes = fs::read(staging.join(HISTORY_SOURCE)).unwrap();
        assert_eq!(hash, format!("{:x}", Sha256::digest(&bytes)));
        let rows = BufReader::new(bytes.as_slice())
            .lines()
            .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .all(|row| row["thread_id"] == chats[0].task_id.to_string()));
        assert!(!String::from_utf8(bytes).unwrap().contains("private-turn"));
    }

    #[test]
    fn refuses_missing_or_unknown_projection_instead_of_exporting_empty_history() {
        let (_root, home, staging, chats) = fixture();
        let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
        db.execute("DELETE FROM thread_history_projection_state", [])
            .unwrap();
        drop(db);
        assert!(stage(&home, &staging, &chats)
            .unwrap_err()
            .message
            .contains("projection is missing"));
        let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
        db.execute_batch("ALTER TABLE thread_turns ADD COLUMN unknown_future_field TEXT;")
            .unwrap();
        drop(db);
        assert!(stage(&home, &staging, &chats)
            .unwrap_err()
            .message
            .contains("unsupported thread_turns schema"));
    }

    #[test]
    fn interrupted_or_started_turns_also_require_a_projection() {
        for event in [
            "task_started",
            "turn_started",
            "turn_complete",
            "turn_aborted",
            "item_completed",
        ] {
            let (_root, home, staging, chats) = fixture();
            let rollout = staging.join(&chats[0].archive_path);
            let bytes = fs::read_to_string(&rollout)
                .unwrap()
                .replace("task_complete", event);
            fs::write(&rollout, bytes).unwrap();
            let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
            db.execute("DELETE FROM thread_history_projection_state", [])
                .unwrap();
            drop(db);
            let error = stage(&home, &staging, &chats).unwrap_err();
            assert!(
                error.message.contains("projection is missing"),
                "{event}: {}",
                error.message
            );
        }
    }

    #[test]
    fn realtime_only_history_requires_its_items_but_not_a_turn() {
        let (_root, home, staging, chats) = fixture();
        let path = staging.join(&chats[0].archive_path);
        let metadata = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let event = json!({"type":"realtime_item","payload":{"id":"realtime-1","realtime_session_id":"session-1","type":"realtime_session_started"}});
        let bytes = format!("{metadata}\n{event}\n");
        fs::write(&path, &bytes).unwrap();
        let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
        let id = chats[0].task_id.to_string();
        db.execute("DELETE FROM thread_turns WHERE thread_id=?1", [&id])
            .unwrap();
        db.execute("DELETE FROM thread_items WHERE thread_id=?1", [&id])
            .unwrap();
        db.execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=?1 WHERE thread_id=?2", (bytes.len() as i64,&id)).unwrap();
        db.execute("INSERT INTO thread_realtime_items(thread_id,item_id,rollout_ordinal,item_type,item_json) VALUES (?1,'realtime-1',1,'realtime_session_started','{}')", [&id]).unwrap();
        drop(db);
        stage(&home, &staging, &chats).unwrap().unwrap();
        let exported = fs::read_to_string(staging.join(HISTORY_SOURCE)).unwrap();
        assert!(exported.contains("thread_realtime_items"));
        assert!(!exported.contains("thread_turns"));
    }
}
