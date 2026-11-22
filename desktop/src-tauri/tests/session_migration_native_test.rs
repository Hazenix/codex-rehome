use rehome_desktop_lib::core::{
    discovery::discover_codex,
    models::{
        CreatePackageRequest, PackageMode, RestoreOptions, SessionAction, SourceOs, TargetInventory,
    },
    package::{create_package_with_project_references, inspect_package},
    planner::{build_restore_plan_with_options, RestorePlanningOptions},
    restore::apply_restore,
};
use serde_json::Value;
use std::{collections::HashMap, error::Error, fs, path::PathBuf};
use uuid::Uuid;

/// Builds a package using the same native engine's stores on both sides. The
/// caller creates synthetic conversations and verifies app-server readback.
#[test]
#[ignore = "requires an isolated synthetic Codex engine fixture"]
fn migrate_new_codex_state_into_an_existing_personal_project() -> Result<(), Box<dyn Error>> {
    let root = fs::canonicalize(PathBuf::from(
        std::env::var_os("REHOME_NATIVE_FIXTURE").ok_or("missing fixture root")?,
    ))?;
    assert!(root.starts_with(fs::canonicalize(std::env::temp_dir())?));
    let receipt: Value = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
    let source_home = root.join("source/.codex");
    let target_home = root.join("target/.codex");
    let target_project_folder = root.join("target/already-cloned");
    let mut source = discover_codex(Some(source_home.clone()))?;
    let archived = source
        .conversations
        .iter()
        .find(|chat| Some(chat.task_id.to_string().as_str()) == receipt["archived"].as_str())
        .expect("source archive missing");
    let archived_path = source_home.join(archived.archive_path.strip_prefix("codex/").unwrap());
    let compressed_path = archived_path.with_extension("jsonl.zst");
    let compressed = zstd::stream::encode_all(fs::read(&archived_path)?.as_slice(), 3)?;
    fs::write(&compressed_path, &compressed)?;
    fs::remove_file(archived_path)?;
    source = discover_codex(Some(source_home.clone()))?;
    let references = source
        .projects
        .into_iter()
        .filter(|project| project.source_path == root.join("source/code").to_string_lossy())
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 1);
    let output = root.join("only-conversations.rehome");
    create_package_with_project_references(
        CreatePackageRequest {
            codex_home: source_home,
            project_paths: vec![],
            conversation_ids: source
                .conversations
                .iter()
                .map(|chat| chat.task_id)
                .collect(),
            output_path: output.clone(),
            source_device_id: Uuid::nil(),
            skill_paths: vec![],
            plugin_paths: vec![],
            generated_image_paths: vec![],
        },
        &references,
    )?;
    assert_eq!(fs::read(&compressed_path)?, compressed);
    let preview = inspect_package(&output)?;
    assert_eq!(preview.manifest.mode, PackageMode::SessionsOnly);
    let expected_chats = if receipt.get("fork").is_some() { 5 } else { 4 };
    assert_eq!(preview.manifest.counts.conversations, expected_chats);
    assert_eq!(preview.manifest.counts.project_files, 0);
    for attempt in 0..2 {
        let inventory = discover_codex(Some(target_home.clone()))?;
        let target = TargetInventory {
            codex_home: target_home.clone(),
            target_os: SourceOs::Macos,
            target_arch: std::env::consts::ARCH.into(),
            counts: inventory.counts,
            projects: inventory.projects,
            conversations: inventory.conversations,
        };
        let plan = build_restore_plan_with_options(
            &preview,
            &target,
            &root.join("unused-project-root"),
            None,
            &RestorePlanningOptions {
                project_destinations: HashMap::from([(
                    references[0].project_id,
                    target_project_folder.clone(),
                )]),
                defer_conflicting_conversations: true,
            },
        )?;
        assert_eq!(plan.sessions.len() as u64, expected_chats);
        assert!(plan.deferred_conversations.is_empty());
        let expected_action = if attempt == 0 {
            SessionAction::Import
        } else {
            SessionAction::Skip
        };
        assert!(plan
            .sessions
            .iter()
            .all(|session| session.action == expected_action));
        let report = apply_restore(
            plan,
            RestoreOptions {
                codex_closed_confirmed: true,
                backup_root: root.join("migration-backups"),
                register_projects: false,
            },
        )?;
        assert!(report.verification.sessions_valid);
        assert!(report.verification.sqlite_threads_valid);
    }
    assert_eq!(
        fs::read(target_home.join("config.toml"))?,
        fs::read(root.join("target-config-before"))?
    );
    assert!(!target_project_folder
        .join("project-content-not-for-transfer.txt")
        .exists());
    Ok(())
}
