#[allow(dead_code)]
mod common;

use common::{synthetic_codex_fixture, SyntheticCodexFixture, THREAD_ID};
use rehome_desktop_lib::core::{
    discovery::discover_codex,
    models::{
        ChangeKind, ContentCounts, CreatePackageRequest, RestoreOptions, SourceOs, TargetInventory,
    },
    package::{create_package_with_project_references, inspect_package},
    planner::{build_restore_plan, build_restore_plan_with_options, RestorePlanningOptions},
    restore::{apply_restore, rollback},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{error::Error, fs, io::Write, path::Path};
use uuid::Uuid;

fn request(fixture: &SyntheticCodexFixture, output: &Path) -> CreatePackageRequest {
    CreatePackageRequest {
        codex_home: fixture.codex_home.clone(),
        project_paths: vec![],
        conversation_ids: vec![Uuid::parse_str(THREAD_ID).unwrap()],
        output_path: output.to_owned(),
        source_device_id: Uuid::nil(),
        skill_paths: vec![],
        plugin_paths: vec![],
        generated_image_paths: vec![],
    }
}

fn attachment_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE thread_attachments (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id), attachment_type TEXT NOT NULL, identity_key TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(thread_id,attachment_type,identity_key));")
}

fn append_record(path: &Path, value: Value) -> Result<(), Box<dyn Error>> {
    writeln!(fs::OpenOptions::new().append(true).open(path)?, "{value}")?;
    Ok(())
}

fn source_with_attachment() -> Result<(SyntheticCodexFixture, std::path::PathBuf), Box<dyn Error>> {
    let fixture = synthetic_codex_fixture()?;
    let attachment = fixture.codex_home.join("attachments").join("selected.png");
    fs::create_dir_all(attachment.parent().unwrap())?;
    fs::write(&attachment, b"synthetic selected image")?;
    // Plain text that looks like a path must remain historical text.
    append_record(
        &fixture.session_path,
        json!({"type":"event_msg","payload":{"type":"user_message","message":attachment,"local_images":[attachment]}}),
    )?;
    let connection = Connection::open(&fixture.state_db_path)?;
    attachment_schema(&connection)?;
    connection.execute("INSERT INTO thread_attachments VALUES (?,?,?,?,?,?)", rusqlite::params![Uuid::new_v4().to_string(), THREAD_ID, "image", "selected-image", json!({"path":attachment,"mime_type":"image/png","unrelated_secret":{"do_not_export":true}}).to_string(), 1])?;
    // Worktree metadata is not a portable file attachment, even if it contains a path.
    connection.execute(
        "INSERT INTO thread_attachments VALUES (?,?,?,?,?,?)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            THREAD_ID,
            "worktree",
            "project-state",
            json!({"path":attachment}).to_string(),
            2
        ],
    )?;
    fs::write(
        fixture
            .codex_home
            .join("attachments")
            .join("unselected.png"),
        b"not selected",
    )?;
    Ok((fixture, attachment))
}

#[test]
fn selected_attachments_round_trip_with_scoped_paths_and_transaction_rollback(
) -> Result<(), Box<dyn Error>> {
    let app_data = tempfile::tempdir()?;
    let previous = std::env::var_os("LOCALAPPDATA");
    std::env::set_var("LOCALAPPDATA", app_data.path());
    struct RestoreEnvironment(Option<std::ffi::OsString>);
    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("LOCALAPPDATA", value),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }
    }
    let _environment = RestoreEnvironment(previous);
    let (source, source_attachment) = source_with_attachment()?;
    let output = source.root.join("attachments.rehome");
    create_package_with_project_references(request(&source, &output), &[])?;
    let preview = inspect_package(&output)?;
    assert!(preview
        .entries
        .iter()
        .any(|path| path == "codex/metadata/attachments.json"));
    let files = preview
        .entries
        .iter()
        .filter(|path| path.starts_with("codex/attachments/") && !path.ends_with('/'))
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert!(!preview
        .entries
        .iter()
        .any(|path| path.contains("unselected.png")));

    let target = synthetic_codex_fixture()?;
    fs::remove_file(&target.session_path)?;
    let connection = Connection::open(&target.state_db_path)?;
    connection.execute("DELETE FROM threads", [])?;
    attachment_schema(&connection)?;
    drop(connection);
    let inventory = TargetInventory {
        codex_home: target.codex_home.clone(),
        target_os: if cfg!(target_os = "windows") {
            SourceOs::Windows
        } else {
            SourceOs::Macos
        },
        target_arch: std::env::consts::ARCH.into(),
        counts: ContentCounts::default(),
        projects: vec![],
        conversations: vec![],
    };
    let plan = build_restore_plan(&preview, &inventory, &target.root.join("projects-restored"))?;
    let file = plan
        .operations
        .iter()
        .find(|operation| operation.package_source.starts_with("codex/attachments/"))
        .unwrap();
    assert_eq!(file.action, ChangeKind::Add);
    let target_attachment = file.target.clone();
    let existing_attachment = target.codex_home.join("attachments/selected.png");
    fs::create_dir_all(existing_attachment.parent().unwrap())?;
    fs::write(&existing_attachment, b"target-only existing attachment")?;
    let restored_session = plan.sessions[0].target.clone();
    let report = apply_restore(
        plan,
        RestoreOptions {
            codex_closed_confirmed: true,
            backup_root: app_data.path().join("backups"),
            register_projects: false,
        },
    )?;
    assert!(
        report.verification.files_valid
            && report.verification.sessions_valid
            && report.verification.sqlite_threads_valid
    );
    assert_eq!(fs::read(&target_attachment)?, b"synthetic selected image");
    assert_eq!(
        fs::read(&existing_attachment)?,
        b"target-only existing attachment"
    );
    let rows = fs::read_to_string(&restored_session)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let user = rows
        .iter()
        .find(|row| row["payload"]["type"] == "user_message")
        .unwrap();
    assert_eq!(
        user["payload"]["local_images"][0],
        target_attachment.to_str().unwrap()
    );
    assert_eq!(
        user["payload"]["message"],
        source_attachment.to_str().unwrap()
    );
    let connection = Connection::open(&target.state_db_path)?;
    let payload: String = connection.query_row(
        "SELECT payload FROM thread_attachments WHERE thread_id=?",
        [THREAD_ID],
        |row| row.get(0),
    )?;
    let payload: Value = serde_json::from_str(&payload)?;
    assert_eq!(payload["path"], target_attachment.to_str().unwrap());
    assert!(payload.get("unrelated_secret").is_none());
    let count: u64 =
        connection.query_row("SELECT count(*) FROM thread_attachments", [], |row| {
            row.get(0)
        })?;
    assert_eq!(count, 1);
    drop(connection);
    rollback(report.transaction_id)?;
    assert!(!target_attachment.exists());
    assert!(!restored_session.exists());
    assert_eq!(fs::read(&source_attachment)?, b"synthetic selected image");
    Ok(())
}

#[test]
fn selected_missing_managed_attachment_fails_without_creating_a_package(
) -> Result<(), Box<dyn Error>> {
    let fixture = synthetic_codex_fixture()?;
    let missing = fixture.codex_home.join("attachments/missing.png");
    append_record(
        &fixture.session_path,
        json!({"type":"event_msg","payload":{"type":"user_message","local_images":[missing]}}),
    )?;
    let output = fixture.root.join("missing.rehome");
    let error =
        create_package_with_project_references(request(&fixture, &output), &[]).unwrap_err();
    assert!(error.message.contains("missing or unreadable"), "{error}");
    assert!(!output.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn managed_attachment_symlinks_are_rejected_and_external_paths_are_not_read(
) -> Result<(), Box<dyn Error>> {
    let fixture = synthetic_codex_fixture()?;
    let external = fixture.root.join("external-private.png");
    fs::write(&external, b"synthetic private bytes")?;
    append_record(
        &fixture.session_path,
        json!({"type":"event_msg","payload":{"type":"user_message","local_images":[external]}}),
    )?;
    let output = fixture.root.join("external.rehome");
    create_package_with_project_references(request(&fixture, &output), &[])?;
    assert!(!inspect_package(&output)?
        .entries
        .iter()
        .any(|path| path.starts_with("codex/attachments/")));
    let link = fixture.codex_home.join("attachments/linked.png");
    fs::create_dir_all(link.parent().unwrap())?;
    std::os::unix::fs::symlink(&external, &link)?;
    append_record(
        &fixture.session_path,
        json!({"type":"event_msg","payload":{"type":"user_message","local_images":[link]}}),
    )?;
    let error = create_package_with_project_references(
        request(&fixture, &fixture.root.join("symlink.rehome")),
        &[],
    )
    .unwrap_err();
    assert!(error.message.contains("symbolic link"), "{error}");
    assert_eq!(fs::read(&external)?, b"synthetic private bytes");
    Ok(())
}

#[test]
fn deferred_conversation_cannot_import_its_attachment() -> Result<(), Box<dyn Error>> {
    let (fixture, _) = source_with_attachment()?;
    let output = fixture.root.join("deferred.rehome");
    create_package_with_project_references(request(&fixture, &output), &[])?;
    let target = synthetic_codex_fixture()?;
    append_record(
        &target.session_path,
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"target-only branch"}]}}),
    )?;
    let current = discover_codex(Some(target.codex_home.clone()))?;
    let inventory = TargetInventory {
        codex_home: target.codex_home.clone(),
        target_os: current.source_os,
        target_arch: current.source_arch,
        counts: current.counts,
        projects: vec![],
        conversations: current.conversations,
    };
    let plan = build_restore_plan_with_options(
        &inspect_package(&output)?,
        &inventory,
        &target.root.join("restored-projects"),
        None,
        &RestorePlanningOptions {
            defer_conflicting_conversations: true,
            ..Default::default()
        },
    )?;
    assert!(plan.sessions.is_empty());
    assert_eq!(plan.deferred_conversations.len(), 1);
    assert!(!plan
        .operations
        .iter()
        .any(|operation| operation.package_source.starts_with("codex/attachments/")));
    Ok(())
}

#[test]
fn inconsistent_file_attachment_aliases_are_rejected_without_reading_external_paths(
) -> Result<(), Box<dyn Error>> {
    let (fixture, attachment) = source_with_attachment()?;
    let connection = Connection::open(&fixture.state_db_path)?;
    connection.execute(
        "UPDATE thread_attachments SET payload=? WHERE attachment_type='image'",
        [json!({"path":attachment,"local_path":"/unrelated/private/file.png"}).to_string()],
    )?;
    drop(connection);
    let output = fixture.root.join("ambiguous-attachment.rehome");
    let error =
        create_package_with_project_references(request(&fixture, &output), &[]).unwrap_err();
    assert!(
        error.message.contains("inconsistent path aliases"),
        "{error}"
    );
    assert!(!output.exists());
    assert_eq!(fs::read(&attachment)?, b"synthetic selected image");
    Ok(())
}
