#[allow(dead_code)]
mod common;

use common::{synthetic_codex_fixture, THREAD_ID};
use rehome_desktop_lib::core::{
    discovery::discover_codex,
    models::*,
    package::{create_package_with_project_references, inspect_package},
    planner::{build_restore_plan_with_options, RestorePlanningOptions},
    restore::apply_restore_with_registrar,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{collections::HashMap, error::Error, fs};
use uuid::Uuid;

const PROJECT_SCHEMA: &str = "CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,metadata TEXT NOT NULL DEFAULT '{}',position INTEGER NOT NULL,created_at_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL);CREATE TABLE project_roots(project_id TEXT NOT NULL,position INTEGER NOT NULL,path TEXT NOT NULL,PRIMARY KEY(project_id,position));ALTER TABLE threads ADD COLUMN project_id TEXT;";

#[test]
fn sessions_only_maps_to_existing_project_without_copying_its_files_or_launching_codex(
) -> Result<(), Box<dyn Error>> {
    let source = synthetic_codex_fixture()?;
    let source_root = source.project_path.to_string_lossy().to_string();
    fs::write(
        source.codex_home.join(".codex-global-state.json"),
        serde_json::to_vec(&json!({"electron-saved-workspace-roots":[source_root]}))?,
    )?;
    let db = Connection::open(&source.state_db_path)?;
    db.execute_batch(PROJECT_SCHEMA)?;
    db.execute(
        "INSERT INTO projects VALUES('source-project','Source project','{}',0,0,0)",
        [],
    )?;
    db.execute(
        "INSERT INTO project_roots VALUES('source-project',0,?1)",
        [&source_root],
    )?;
    db.execute(
        "UPDATE threads SET cwd=?1,rollout_path=?2,project_id='source-project'",
        params![source_root, source.session_path.to_string_lossy()],
    )?;
    drop(db);
    let inventory = discover_codex(Some(source.codex_home.clone()))?;
    let project = inventory
        .projects
        .iter()
        .find(|project| project.source_path == source_root)
        .unwrap()
        .clone();
    let output = source.root.join("conversations.rehome");
    let report = create_package_with_project_references(
        CreatePackageRequest {
            codex_home: source.codex_home.clone(),
            project_paths: vec![],
            conversation_ids: vec![Uuid::parse_str(THREAD_ID)?],
            output_path: output.clone(),
            source_device_id: Uuid::nil(),
            skill_paths: vec![],
            plugin_paths: vec![],
            generated_image_paths: vec![],
        },
        std::slice::from_ref(&project),
    )?;
    assert_eq!(report.counts.project_files, 0);
    let package = inspect_package(&output)?;
    assert_eq!(package.manifest.mode, PackageMode::SessionsOnly);
    assert!(!package
        .entries
        .iter()
        .any(|entry| entry.contains("/files/")));
    assert!(!package
        .entries
        .iter()
        .any(|entry| entry.ends_with("README.md")));

    let target_home = source.root.join("personal/.codex");
    let existing_project = source.root.join("personal/already-cloned-project");
    fs::create_dir_all(&target_home)?;
    fs::create_dir_all(&existing_project)?;
    fs::write(
        existing_project.join("README.md"),
        "Target files must remain unchanged",
    )?;
    let schema: String = Connection::open(&source.state_db_path)?.query_row(
        "SELECT sql FROM sqlite_master WHERE name='threads'",
        [],
        |row| row.get(0),
    )?;
    let target_db_path = target_home.join("state_5.sqlite");
    let db = Connection::open(&target_db_path)?;
    db.execute_batch(&schema)?;
    db.execute_batch(PROJECT_SCHEMA.split("ALTER TABLE").next().unwrap())?;
    db.execute(
        "INSERT INTO projects VALUES('existing-native-project','My existing project','{}',0,0,0)",
        [],
    )?;
    db.execute(
        "INSERT INTO project_roots VALUES('existing-native-project',0,?1)",
        [existing_project.to_string_lossy().as_ref()],
    )?;
    drop(db);
    fs::write(target_home.join("session_index.jsonl"), "")?;
    let target = TargetInventory {
        codex_home: target_home.clone(),
        target_os: if cfg!(windows) {
            SourceOs::Windows
        } else {
            SourceOs::Macos
        },
        target_arch: "test".into(),
        counts: ContentCounts::default(),
        projects: vec![],
        conversations: vec![],
    };
    let unused_root = source.root.join("unused-project-copy-root");
    assert!(build_restore_plan_with_options(
        &package,
        &target,
        &unused_root,
        None,
        &RestorePlanningOptions::default()
    )
    .is_err());
    let options = RestorePlanningOptions {
        project_destinations: HashMap::from([(project.project_id, existing_project.clone())]),
        defer_conflicting_conversations: true,
    };
    let plan = build_restore_plan_with_options(&package, &target, &unused_root, None, &options)?;
    assert!(!plan
        .operations
        .iter()
        .any(|operation| operation.target.starts_with(&existing_project)));
    assert!(plan
        .reference_rewrites
        .iter()
        .any(|rewrite| rewrite.to == existing_project.to_string_lossy()));
    let report = apply_restore_with_registrar(
        plan,
        RestoreOptions {
            codex_closed_confirmed: true,
            backup_root: source.root.join("backups"),
            register_projects: true,
        },
        |_, _| panic!("session-only import must not launch Codex"),
    )?;
    assert!(report.registrations.is_empty());
    assert!(report.verification.path_mapping_valid);
    assert!(report.verification.sqlite_threads_valid);
    assert_eq!(
        fs::read_to_string(existing_project.join("README.md"))?,
        "Target files must remain unchanged"
    );
    assert!(!unused_root.exists());
    let db = Connection::open(&target_db_path)?;
    let (cwd, project_id): (String, String) = db.query_row(
        "SELECT cwd,project_id FROM threads WHERE id=?1",
        [THREAD_ID],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(cwd, existing_project.to_string_lossy());
    assert_eq!(project_id, "existing-native-project");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM projects", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    let metadata: String = db.query_row(
        "SELECT metadata FROM projects WHERE id='existing-native-project'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(serde_json::from_str::<Value>(&metadata)?, json!({}));
    Ok(())
}
