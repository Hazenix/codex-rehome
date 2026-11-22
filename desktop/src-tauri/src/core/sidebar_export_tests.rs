use super::export_selected_threads;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::error::Error;
use uuid::Uuid;

#[test]
fn exported_sidebar_contains_only_selected_members_and_their_sections() -> Result<(), Box<dyn Error>>
{
    let root = tempfile::tempdir()?;
    let path = root.path().join("state_5.sqlite");
    let db = Connection::open(&path)?;
    db.execute_batch("CREATE TABLE thread_sections(id TEXT PRIMARY KEY,name TEXT NOT NULL,appearance TEXT);
      CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,name TEXT,is_pinned INTEGER,thread_section_id TEXT,
        section_position INTEGER,section_entered_at_ms INTEGER,archived INTEGER,archived_at INTEGER,
        creator_user_id TEXT,creator_account_id TEXT,thread_source TEXT,originator TEXT,
        created_at_ms INTEGER,updated_at_ms INTEGER,recency_at INTEGER,recency_at_ms INTEGER);
      INSERT INTO thread_sections VALUES('33333333-3333-4333-8333-333333333333','工作记录','{\"icon\":\"code\"}');
      INSERT INTO thread_sections VALUES('44444444-4444-4444-8444-444444444444','Not selected',NULL);
      INSERT INTO threads VALUES('11111111-1111-4111-8111-111111111111','Preview','用户命名',1,
        '33333333-3333-4333-8333-333333333333',2000000,1700000000000,1,1700000000,
        'synthetic-user','synthetic-account','cli','synthetic-test',1000,2000,2,2000);
      INSERT INTO threads(id,title,thread_section_id) VALUES('22222222-2222-4222-8222-222222222222','Not selected','44444444-4444-4444-8444-444444444444');")?;
    drop(db);
    let exported = export_selected_threads(
        &path,
        &[Uuid::parse_str("11111111-1111-4111-8111-111111111111")?],
    )?;
    let rows: Value = serde_json::from_slice(&exported.bytes)?;
    assert_eq!(exported.count, 1);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "用户命名");
    assert_eq!(rows[0]["archived"], 1);
    assert_eq!(rows[0]["is_pinned"], 1);
    assert_eq!(rows[0]["section_position"], 2000000);
    assert_eq!(rows[0]["creator_account_id"], "synthetic-account");
    assert_eq!(
        rows[0]["_rehome_section"],
        json!({"id":"33333333-3333-4333-8333-333333333333","name":"工作记录","appearance":"{\"icon\":\"code\"}"})
    );
    assert!(!String::from_utf8(exported.bytes)?.contains("Not selected"));
    Ok(())
}
