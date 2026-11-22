#[allow(dead_code)]
mod common;

use rehome_desktop_lib::core::{
    bridge::import_sqlite_threads,
    discovery::discover_codex,
    models::{CreatePackageRequest, PlannedSession, SessionAction},
    package::{create_package, inspect_package},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{error::Error, path::Path};
use uuid::Uuid;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const DEFERRED: &str = "22222222-2222-4222-8222-222222222222";
const SECTION: &str = "33333333-3333-4333-8333-333333333333";
const LOCAL_SECTION: &str = "44444444-4444-4444-8444-444444444444";

fn database(path: &Path) -> Result<Connection, Box<dyn Error>> {
    let db = Connection::open(path)?;
    db.execute_batch("PRAGMA foreign_keys=ON;
        CREATE TABLE thread_sections(id TEXT PRIMARY KEY,name TEXT NOT NULL,appearance TEXT);
        CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,name TEXT,is_pinned INTEGER NOT NULL DEFAULT 0,
          thread_section_id TEXT REFERENCES thread_sections(id),section_position INTEGER,section_entered_at_ms INTEGER,
          archived INTEGER,archived_at INTEGER,rollout_path TEXT,creator_user_id TEXT,creator_account_id TEXT,
          thread_source TEXT,originator TEXT,created_at_ms INTEGER,updated_at_ms INTEGER,recency_at INTEGER,recency_at_ms INTEGER);
        INSERT INTO thread_sections VALUES('44444444-4444-4444-8444-444444444444','Local only',NULL);
        INSERT INTO threads(id,title,thread_section_id,section_position) VALUES('22222222-2222-4222-8222-222222222222','Local conversation','44444444-4444-4444-8444-444444444444',100);"
    )?;
    Ok(db)
}

fn session(id: &str, target: &Path) -> PlannedSession {
    PlannedSession {
        package_source: format!("codex/sessions/{id}.jsonl"),
        target: target.to_owned(),
        source_task_id: Uuid::parse_str(id).unwrap(),
        target_task_id: Uuid::parse_str(id).unwrap(),
        title: "Derived preview".into(),
        source_content_hash: "hash".into(),
        expected_final_content_hash: "hash".into(),
        action: SessionAction::Import,
    }
}

fn metadata() -> Value {
    json!({"id":THREAD,"title":"Derived preview","name":"手动改名","is_pinned":1,
      "thread_section_id":SECTION,"section_position":2000000,"section_entered_at_ms":1700000000000i64,
      "archived":1,"archived_at":1700000000,
      "creator_user_id":"synthetic-user","creator_account_id":"synthetic-account",
      "thread_source":"cli","originator":"synthetic-test","created_at_ms":1000,"updated_at_ms":2000,
      "recency_at":2,"recency_at_ms":2000,
      "_rehome_section":{"id":SECTION,"name":"开发记录","appearance":"{\"icon\":\"code\"}"}})
}

#[test]
fn manual_name_is_visible_in_discovery_and_package_preview() -> Result<(), Box<dyn Error>> {
    let fixture = common::synthetic_codex_fixture()?;
    let db = Connection::open(&fixture.state_db_path)?;
    db.execute_batch("ALTER TABLE threads ADD COLUMN name TEXT;")?;
    db.execute(
        "UPDATE threads SET name=?1 WHERE id=?2",
        ("手动显示名", THREAD),
    )?;
    drop(db);
    let inventory = discover_codex(Some(fixture.codex_home.clone()))?;
    assert_eq!(inventory.conversations[0].title, "手动显示名");
    let path = fixture.root.join("names.rehome");
    create_package(CreatePackageRequest {
        codex_home: fixture.codex_home,
        project_paths: vec![],
        conversation_ids: vec![Uuid::parse_str(THREAD)?],
        output_path: path.clone(),
        source_device_id: Uuid::nil(),
        skill_paths: vec![],
        plugin_paths: vec![],
        generated_image_paths: vec![],
    })?;
    assert_eq!(
        inspect_package(&path)?.manifest.conversations[0].title,
        "手动显示名"
    );
    Ok(())
}

#[test]
fn selected_sidebar_and_ownership_round_trip_preserves_local_and_deferred_records(
) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state_5.sqlite");
    let db = database(&path)?;
    drop(db);
    let mut deferred = metadata();
    deferred["id"] = json!(DEFERRED);
    deferred["_rehome_section"]["id"] = json!("55555555-5555-4555-8555-555555555555");
    deferred["thread_section_id"] = deferred["_rehome_section"]["id"].clone();
    let payload = serde_json::to_vec(&json!([metadata(), deferred]))?;
    let sessions = [session(
        THREAD,
        &root.path().join("archived_sessions/thread.jsonl"),
    )];
    for _ in 0..2 {
        assert_eq!(import_sqlite_threads(&path, &payload, &sessions, &[])?, 1);
    }
    let db = Connection::open(&path)?;
    let actual = db.query_row("SELECT name,is_pinned,thread_section_id,section_position,section_entered_at_ms,archived,archived_at,creator_user_id,creator_account_id FROM threads WHERE id=?1", [THREAD], |row| {
        Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,i64>(6)?,row.get::<_,String>(7)?,row.get::<_,String>(8)?))
    })?;
    assert_eq!(
        actual,
        (
            "手动改名".into(),
            1,
            SECTION.into(),
            2000000,
            1700000000000,
            1,
            1700000000,
            "synthetic-user".into(),
            "synthetic-account".into()
        )
    );
    assert_eq!(
        db.query_row(
            "SELECT thread_section_id FROM threads WHERE id=?1",
            [DEFERRED],
            |row| row.get::<_, String>(0)
        )?,
        LOCAL_SECTION
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM thread_sections", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT name FROM thread_sections WHERE id=?1",
            [SECTION],
            |row| row.get::<_, String>(0)
        )?,
        "开发记录"
    );
    Ok(())
}

#[test]
fn old_package_preserves_sidebar_and_explicit_null_membership_clears_only_selected_thread(
) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state_5.sqlite");
    drop(database(&path)?);
    let sessions = [session(THREAD, &root.path().join("sessions/thread.jsonl"))];
    import_sqlite_threads(
        &path,
        &serde_json::to_vec(&json!([metadata()]))?,
        &sessions,
        &[],
    )?;
    import_sqlite_threads(
        &path,
        &serde_json::to_vec(&json!([{"id":THREAD,"title":"Old format"}]))?,
        &sessions,
        &[],
    )?;
    let db = Connection::open(&path)?;
    assert_eq!(
        db.query_row("SELECT name FROM threads WHERE id=?1", [THREAD], |row| {
            row.get::<_, String>(0)
        })?,
        "手动改名"
    );
    drop(db);
    import_sqlite_threads(
        &path,
        &serde_json::to_vec(
            &json!([{"id":THREAD,"thread_section_id":null,"section_position":null,"section_entered_at_ms":null,"is_pinned":0}]),
        )?,
        &sessions,
        &[],
    )?;
    let db = Connection::open(&path)?;
    assert!(db
        .query_row(
            "SELECT thread_section_id FROM threads WHERE id=?1",
            [THREAD],
            |row| row.get::<_, Option<String>>(0)
        )?
        .is_none());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM thread_sections", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    Ok(())
}

#[test]
fn failed_later_thread_rolls_back_earlier_section_and_thread() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state_5.sqlite");
    let db = database(&path)?;
    db.execute_batch(
        "CREATE TRIGGER reject_second_thread BEFORE UPDATE ON threads
      WHEN NEW.id='22222222-2222-4222-8222-222222222222'
      BEGIN SELECT RAISE(ABORT,'synthetic import failure'); END;",
    )?;
    drop(db);
    let mut second = metadata();
    second["id"] = json!(DEFERRED);
    let payload = serde_json::to_vec(&json!([metadata(), second]))?;
    let sessions = [
        session(THREAD, &root.path().join("one.jsonl")),
        session(DEFERRED, &root.path().join("two.jsonl")),
    ];
    assert!(import_sqlite_threads(&path, &payload, &sessions, &[]).is_err());
    let db = Connection::open(&path)?;
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM threads", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM thread_sections", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.query_row("SELECT title FROM threads WHERE id=?1", [DEFERRED], |row| {
            row.get::<_, String>(0)
        })?,
        "Local conversation"
    );
    Ok(())
}

#[test]
fn unsupported_sidebar_schema_errors_instead_of_losing_nondefault_state(
) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    let db = Connection::open(&path)?;
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,rollout_path TEXT);")?;
    drop(db);
    let error = import_sqlite_threads(
        &path,
        &serde_json::to_vec(&json!([metadata()]))?,
        &[session(THREAD, &root.path().join("thread.jsonl"))],
        &[],
    )
    .unwrap_err();
    assert!(error.message.contains("does not support sidebar field"));
    Ok(())
}

#[test]
fn older_section_schema_accepts_absent_appearance_without_losing_membership(
) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    let db = database(&path)?;
    db.execute_batch("ALTER TABLE thread_sections DROP COLUMN appearance;")?;
    drop(db);
    let mut row = metadata();
    row["_rehome_section"]["appearance"] = Value::Null;
    import_sqlite_threads(
        &path,
        &serde_json::to_vec(&json!([row]))?,
        &[session(THREAD, &root.path().join("thread.jsonl"))],
        &[],
    )?;
    let db = Connection::open(&path)?;
    assert_eq!(
        db.query_row(
            "SELECT thread_section_id FROM threads WHERE id=?1",
            [THREAD],
            |row| row.get::<_, String>(0)
        )?,
        SECTION
    );
    Ok(())
}
