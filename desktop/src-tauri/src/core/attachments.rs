//! Selected, file-backed attachments only; never follow arbitrary transcript paths.
use crate::core::{
    discovery::StateDatabaseSnapshot,
    error::{ErrorCode, RehomeError},
    exclusions::is_forbidden,
    models::{ConversationEntry, PlannedSession, ReferenceRewrite, ReferenceRewriteKind},
    package::VerifiedPayload,
    paths::normalize_entry,
    session_stream::visit_jsonl,
    stable_fs::PinnedParent,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const SOURCE: &str = "codex/metadata/attachments.json";
pub(crate) const DESCRIPTOR: &str = "_rehome_file_attachments";

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct AttachmentManifest {
    pub files: Vec<AttachmentFile>,
    #[serde(default)]
    records: Vec<AttachmentRecord>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AttachmentFile {
    pub thread_id: Uuid,
    pub source_path: String,
    pub archive_path: String,
    pub content_hash: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct AttachmentRecord {
    source_thread_id: Uuid,
    id: String,
    attachment_type: String,
    identity_key: String,
    payload: Value,
    created_at: i64,
}

/// Snapshot referenced managed files. Text, tool output and external files are never scanned.
pub(crate) fn collect(
    home: &Path,
    database: Option<&Path>,
    conversations: &[ConversationEntry],
    staging: &Path,
) -> Result<AttachmentManifest, RehomeError> {
    let mut manifest = AttachmentManifest::default();
    let mut references = BTreeSet::new();
    for conversation in conversations {
        visit_jsonl(
            fs::File::open(staging.join(&conversation.archive_path)).map_err(failed)?,
            |_, record| {
                for path in record_paths(record) {
                    if managed_relative(home, &path)?.is_some() {
                        references.insert((conversation.task_id, path));
                    }
                }
                Ok(())
            },
        )?;
    }
    if let Some(database) = database {
        let snapshot = StateDatabaseSnapshot::create(database).map_err(failed)?;
        let connection =
            Connection::open_with_flags(snapshot.database_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(failed)?;
        let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_attachments')", [], |row| row.get(0)).map_err(failed)?;
        if exists {
            let mut statement = connection.prepare("SELECT id, attachment_type, identity_key, payload, created_at FROM thread_attachments WHERE thread_id = ? ORDER BY created_at, id").map_err(failed)?;
            for conversation in conversations {
                let mut rows = statement
                    .query([conversation.task_id.to_string()])
                    .map_err(failed)?;
                while let Some(row) = rows.next().map_err(failed)? {
                    let attachment_type: String = row.get(1).map_err(failed)?;
                    if !file_type(&attachment_type) {
                        continue;
                    }
                    let payload_text: String = row.get(3).map_err(failed)?;
                    let payload: Value = serde_json::from_str(&payload_text).map_err(failed)?;
                    let Some(path) = file_payload_path(&payload) else {
                        continue;
                    };
                    if managed_relative(home, path)?.is_none() {
                        continue;
                    }
                    validate_path_aliases(&payload, path)?;
                    references.insert((conversation.task_id, path.to_owned()));
                    // Only known file descriptors travel, not arbitrary client-defined metadata.
                    let mut portable = Map::new();
                    for key in [
                        "path",
                        "file_path",
                        "local_path",
                        "name",
                        "filename",
                        "mime_type",
                        "size_bytes",
                    ] {
                        if let Some(value) = payload.get(key).filter(|value| {
                            value.is_string()
                                || value.is_number()
                                || value.is_boolean()
                                || value.is_null()
                        }) {
                            portable.insert(key.into(), value.clone());
                        }
                    }
                    manifest.records.push(AttachmentRecord {
                        source_thread_id: conversation.task_id,
                        id: row.get(0).map_err(failed)?,
                        attachment_type,
                        identity_key: row.get(2).map_err(failed)?,
                        payload: portable.into(),
                        created_at: row.get(4).map_err(failed)?,
                    });
                }
            }
        }
    }
    if references.len() > 100_000 {
        return Err(invalid("too many selected attachment references"));
    }
    let mut snapshots: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (thread_id, source_path) in references {
        let (archive_path, content_hash) = match snapshots.get(&source_path) {
            Some(value) => value.clone(),
            None => {
                let value = snapshot_file(home, &source_path, staging)?;
                snapshots.insert(source_path.clone(), value.clone());
                value
            }
        };
        manifest.files.push(AttachmentFile {
            thread_id,
            source_path,
            archive_path,
            content_hash,
        });
    }
    Ok(manifest)
}

fn managed_relative(home: &Path, text: &str) -> Result<Option<PathBuf>, RehomeError> {
    if text.is_empty() || text.contains('\0') || text.contains("://") {
        return Ok(None);
    }
    let path = Path::new(text);
    let Ok(relative) = path.strip_prefix(home) else {
        return Ok(None);
    };
    let mut parts = relative.components();
    if !matches!(parts.next(), Some(Component::Normal(name)) if name == "attachments" || name == "generated_images")
    {
        return Ok(None);
    }
    if parts.clone().next().is_none()
        || parts.any(|part| !matches!(part, Component::Normal(_)))
        || is_forbidden(relative)
    {
        return Err(invalid("selected attachment has an unsafe managed path"));
    }
    Ok(Some(relative.to_owned()))
}

fn snapshot_file(home: &Path, text: &str, staging: &Path) -> Result<(String, String), RehomeError> {
    let relative = managed_relative(home, text)?
        .ok_or_else(|| invalid("attachment is outside managed storage"))?;
    let mut path = home.to_owned();
    for part in relative.components() {
        path.push(part);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            invalid(format!(
                "selected managed attachment is missing or unreadable: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("selected attachment crosses a symbolic link"));
        }
    }
    let metadata = fs::metadata(&path).map_err(failed)?;
    if !metadata.is_file() {
        return Err(invalid("selected attachment is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid("selected attachment is hard linked"));
        }
    }
    let canonical_home = home.canonicalize().map_err(failed)?;
    if !path
        .canonicalize()
        .map_err(failed)?
        .starts_with(&canonical_home)
    {
        return Err(invalid("selected attachment escapes managed storage"));
    }
    let parent = PinnedParent::open(
        path.parent()
            .ok_or_else(|| invalid("attachment has no parent"))?,
    )
    .map_err(failed)?;
    let mut source = parent
        .open_file(
            path.file_name()
                .ok_or_else(|| invalid("attachment has no filename"))?,
        )
        .map_err(failed)?;
    let before = source.metadata().map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.nlink() != 1 || before.dev() != metadata.dev() || before.ino() != metadata.ino() {
            return Err(invalid(
                "selected attachment changed identity while being exported",
            ));
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(staging).map_err(failed)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(failed)?;
        if count == 0 {
            break;
        }
        temporary.write_all(&buffer[..count]).map_err(failed)?;
        hash.update(&buffer[..count]);
    }
    let after = source.metadata().map_err(failed)?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(invalid(
            "selected attachment changed while being exported; close Codex and retry",
        ));
    }
    let hash = format!("{:x}", hash.finalize());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("attachment filename is not UTF-8"))?;
    let archive = normalize_entry(Path::new(&format!(
        "codex/attachments/rehome/{hash}/{name}"
    )))?;
    let destination = staging.join(&archive);
    fs::create_dir_all(destination.parent().unwrap()).map_err(failed)?;
    if !destination.exists() {
        temporary
            .persist(&destination)
            .map_err(|error| failed(error.error))?;
    }
    Ok((archive, hash))
}

pub(crate) fn decorate_threads(
    bytes: &[u8],
    manifest: &AttachmentManifest,
) -> Result<Vec<u8>, RehomeError> {
    if manifest.records.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut rows: Vec<Value> = serde_json::from_slice(bytes).map_err(failed)?;
    for row in &mut rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<Uuid>().ok())
        else {
            continue;
        };
        let records = manifest
            .records
            .iter()
            .filter(|record| record.source_thread_id == id)
            .collect::<Vec<_>>();
        if !records.is_empty() {
            row[DESCRIPTOR] = serde_json::to_value(records).map_err(failed)?;
        }
    }
    serde_json::to_vec(&rows).map_err(failed)
}

pub(crate) fn parse(bytes: &[u8]) -> Result<AttachmentManifest, RehomeError> {
    let manifest: AttachmentManifest = serde_json::from_slice(bytes).map_err(failed)?;
    if manifest.files.len() > 100_000 || manifest.records.len() > 100_000 {
        return Err(invalid("attachment metadata exceeds its entry limit"));
    }
    for file in &manifest.files {
        let path = normalize_entry(Path::new(&file.archive_path))?;
        let expected = format!("codex/attachments/rehome/{}/", file.content_hash);
        if file.content_hash.len() != 64
            || !file
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !path.starts_with(&expected)
            || path[expected.len()..].contains('/')
            || is_forbidden(Path::new(&path))
        {
            return Err(invalid(
                "attachment metadata contains an unsafe package path",
            ));
        }
    }
    Ok(manifest)
}

pub(crate) fn rewrites(
    bytes: &[u8],
    conversation: &ConversationEntry,
    home: &Path,
) -> Result<Vec<ReferenceRewrite>, RehomeError> {
    parse(bytes)?
        .files
        .into_iter()
        .filter(|file| file.thread_id == conversation.task_id)
        .map(|file| {
            let target = home.join(file.archive_path.strip_prefix("codex/").unwrap());
            Ok(ReferenceRewrite {
                source_task_id: conversation.task_id,
                package_source: conversation.archive_path.clone(),
                kind: ReferenceRewriteKind::AttachmentPath,
                from: file.source_path,
                to: target
                    .to_str()
                    .ok_or_else(|| invalid("target attachment path is not UTF-8"))?
                    .into(),
            })
        })
        .collect()
}

pub(crate) fn planned_files(
    bytes: &[u8],
    sessions: &[PlannedSession],
    payloads: &BTreeMap<String, VerifiedPayload>,
) -> Result<BTreeSet<String>, RehomeError> {
    let selected = sessions
        .iter()
        .map(|session| session.source_task_id)
        .collect::<BTreeSet<_>>();
    let manifest = parse(bytes)?;
    let mut paths = BTreeSet::new();
    for file in manifest.files {
        let payload = payloads
            .get(&file.archive_path)
            .ok_or_else(|| invalid("selected attachment payload is missing"))?;
        if payload.content_hash != file.content_hash {
            return Err(invalid(
                "attachment content hash does not match the package",
            ));
        }
        if selected.contains(&file.thread_id) {
            paths.insert(file.archive_path);
        }
    }
    Ok(paths)
}

fn file_type(kind: &str) -> bool {
    matches!(
        kind,
        "file" | "image" | "audio" | "document" | "local_file" | "local_image" | "local_audio"
    )
}
fn file_payload_path(value: &Value) -> Option<&str> {
    ["path", "file_path", "local_path"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
}

fn validate_path_aliases(payload: &Value, path: &str) -> Result<(), RehomeError> {
    for key in ["path", "file_path", "local_path"] {
        if payload
            .get(key)
            .is_some_and(|value| value.as_str() != Some(path))
        {
            return Err(invalid(
                "file attachment contains inconsistent path aliases",
            ));
        }
    }
    Ok(())
}

fn visit_file_fields(value: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    if let Some(values) = value.as_array_mut() {
        for value in values {
            visit_file_fields(value, visit);
        }
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if matches!(
        object.get("type").and_then(Value::as_str),
        Some("local_image" | "local_audio" | "local_file" | "localImage" | "localAudio")
    ) {
        if let Some(path) = object.get_mut("path") {
            visit(path);
        }
    }
    if object.get("type").and_then(Value::as_str) == Some("user_message") {
        if let Some(images) = object.get_mut("local_images").and_then(Value::as_array_mut) {
            for image in images {
                visit(image);
            }
        }
    }
    for key in ["payload", "content", "items", "item"] {
        if let Some(value) = object.get_mut(key) {
            visit_file_fields(value, visit);
        }
    }
}

fn record_paths(value: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    visit_file_fields(&mut value.clone(), &mut |value| {
        if let Some(path) = value.as_str() {
            paths.push(path.to_owned());
        }
    });
    paths
}

pub(crate) fn rewrite_record(value: &mut Value, rewrites: &[&ReferenceRewrite]) {
    visit_file_fields(value, &mut |value| {
        if let Some(rewrite) = rewrites.iter().find(|rewrite| {
            rewrite.kind == ReferenceRewriteKind::AttachmentPath
                && value.as_str() == Some(rewrite.from.as_str())
        }) {
            *value = Value::String(rewrite.to.clone());
        }
    });
}

pub(crate) fn import_records(
    connection: &Connection,
    row: &Map<String, Value>,
    rewrites: &[ReferenceRewrite],
) -> Result<(), RehomeError> {
    let Some(records) = row.get(DESCRIPTOR) else {
        return Ok(());
    };
    let records: Vec<AttachmentRecord> = serde_json::from_value(records.clone()).map_err(failed)?;
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_attachments')", [], |row| row.get(0)).map_err(failed)?;
    if !exists {
        return Err(invalid(
            "target Codex does not support file attachment records",
        ));
    }
    let target_id = row
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("attachment owner is missing"))?;
    for mut record in records {
        if target_id != record.source_thread_id.to_string()
            && !rewrites.iter().any(|rewrite| {
                rewrite.kind == ReferenceRewriteKind::ConversationId
                    && rewrite.source_task_id == record.source_thread_id
                    && rewrite.to == target_id
            })
        {
            return Err(invalid(
                "file attachment owner does not match its planned conversation",
            ));
        }
        if !file_type(&record.attachment_type) {
            return Err(invalid("unsupported attachment category in package"));
        }
        let fields = record
            .payload
            .as_object()
            .ok_or_else(|| invalid("file attachment payload must be an object"))?;
        if fields.iter().any(|(key, value)| {
            !matches!(
                key.as_str(),
                "path"
                    | "file_path"
                    | "local_path"
                    | "name"
                    | "filename"
                    | "mime_type"
                    | "size_bytes"
            ) || value.is_array()
                || value.is_object()
        }) {
            return Err(invalid(
                "file attachment payload contains unsupported fields",
            ));
        }
        let old_path = file_payload_path(&record.payload)
            .ok_or_else(|| invalid("file attachment has no path"))?
            .to_owned();
        validate_path_aliases(&record.payload, &old_path)?;
        let rewrite = rewrites
            .iter()
            .find(|rewrite| {
                rewrite.kind == ReferenceRewriteKind::AttachmentPath
                    && rewrite.source_task_id == record.source_thread_id
                    && rewrite.from == old_path
            })
            .ok_or_else(|| invalid("file attachment has no verified destination mapping"))?;
        for key in ["path", "file_path", "local_path"] {
            if record.payload.get(key).and_then(Value::as_str) == Some(old_path.as_str()) {
                record.payload[key] = Value::String(rewrite.to.clone());
            }
        }
        if record.identity_key == old_path {
            record.identity_key = rewrite.to.clone();
        }
        let payload = serde_json::to_string(&record.payload).map_err(failed)?;
        let existing: Option<String> = connection.query_row("SELECT payload FROM thread_attachments WHERE thread_id=? AND attachment_type=? AND identity_key=?", [target_id, &record.attachment_type, &record.identity_key], |row| row.get(0)).optional().map_err(failed)?;
        if let Some(existing) = existing {
            if serde_json::from_str::<Value>(&existing).map_err(failed)? != record.payload {
                return Err(invalid(
                    "target file attachment was changed; its record was preserved",
                ));
            }
            continue;
        }
        let id = Uuid::new_v5(
            &target_id.parse::<Uuid>().map_err(failed)?,
            record.id.as_bytes(),
        )
        .to_string();
        connection.execute("INSERT INTO thread_attachments (id,thread_id,attachment_type,identity_key,payload,created_at) VALUES (?,?,?,?,?,?)", rusqlite::params![id, target_id, record.attachment_type, record.identity_key, payload, record.created_at]).map_err(failed)?;
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> RehomeError {
    RehomeError::new(ErrorCode::PackageInvalid, message)
}
fn failed(error: impl std::fmt::Display) -> RehomeError {
    invalid(format!("file attachment: {error}"))
}
