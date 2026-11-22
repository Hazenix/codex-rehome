use rehome_desktop_lib::core::{
    discovery::discover_codex,
    models::{
        CreatePackageRequest, PackageMode, PackagePreview, RestoreOptions, RestorePlan,
        SessionAction, SourceOs, TargetInventory,
    },
    package::{create_package_with_project_references, inspect_package},
    planner::{build_restore_plan_with_options, RestorePlanningOptions},
    restore::apply_restore,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Builds a package using the same native engine's stores on both sides. The
/// caller creates synthetic conversations and verifies app-server readback.
#[test]
#[ignore = "requires an isolated synthetic Codex engine fixture"]
fn migrate_new_codex_state_into_an_existing_personal_project() -> Result<(), Box<dyn Error>> {
    let root = fixture_root()?;
    let receipt: Value = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
    let source_home = root.join("source/.codex");
    let target_home = root.join("target/.codex");
    let target_project_folder = root.join("target/already-cloned");
    let source = discover_codex(Some(source_home.clone()))?;
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
    let (preview, options) = export_native_package(&root, "only-conversations.rehome")?;
    assert_eq!(fs::read(&compressed_path)?, compressed);
    let expected_chats = if receipt.get("fork").is_some() { 5 } else { 4 };
    assert_eq!(preview.manifest.counts.conversations, expected_chats);
    for attempt in 0..2 {
        let plan = plan_native_import(&root, &preview, &options)?;
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
        apply_native_import(&root, plan)?;
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

#[test]
#[ignore = "requires a rehearsed native fixture with an advanced source conversation"]
fn imports_advanced_source_and_dependent_fork_as_copies() -> Result<(), Box<dyn Error>> {
    let root = fixture_root()?;
    let receipt: Value = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
    let source_parent: Uuid = receipt["second"].as_str().unwrap().parse()?;
    let source_fork: Uuid = receipt["fork"].as_str().unwrap().parse()?;
    let (preview, mut options) = export_native_package(&root, "updated-conversations.rehome")?;

    let deferred = plan_native_import(&root, &preview, &options)?;
    assert_eq!(deferred.deferred_conversations.len(), 2);
    assert!(deferred
        .deferred_conversations
        .iter()
        .any(|item| item.task_id == source_parent && item.required_source_id.is_none()));
    assert!(deferred
        .deferred_conversations
        .iter()
        .any(|item| item.task_id == source_fork && item.required_source_id == Some(source_parent)));
    options.defer_conflicting_conversations = false;
    let copies = plan_native_import(&root, &preview, &options)?;
    assert!(copies.deferred_conversations.is_empty());
    let branch_ids = copies
        .sessions
        .iter()
        .filter(|session| session.action == SessionAction::ImportAsBranch)
        .map(|session| (session.source_task_id, session.target_task_id))
        .collect::<HashMap<_, _>>();
    assert_eq!(branch_ids.len(), 2);
    assert_ne!(branch_ids[&source_parent], source_parent);
    assert_ne!(branch_ids[&source_fork], source_fork);
    apply_native_import(&root, copies)?;

    let repeated = plan_native_import(&root, &preview, &options)?;
    assert!(repeated
        .sessions
        .iter()
        .all(|session| session.action == SessionAction::Skip));
    for session in &repeated.sessions {
        if let Some(expected_id) = branch_ids.get(&session.source_task_id) {
            assert_eq!(*expected_id, session.target_task_id);
        }
    }
    apply_native_import(&root, repeated)?;
    fs::write(root.join("copy-ids.json"), serde_json::to_vec(&branch_ids)?)?;
    Ok(())
}

fn fixture_root() -> Result<PathBuf, Box<dyn Error>> {
    let root = fs::canonicalize(PathBuf::from(
        std::env::var_os("REHOME_NATIVE_FIXTURE").ok_or("missing fixture root")?,
    ))?;
    assert!(root.starts_with(fs::canonicalize(std::env::temp_dir())?));
    Ok(root)
}

fn export_native_package(
    root: &Path,
    filename: &str,
) -> Result<(PackagePreview, RestorePlanningOptions), Box<dyn Error>> {
    let source_home = root.join("source/.codex");
    let source = discover_codex(Some(source_home.clone()))?;
    let references = source
        .projects
        .into_iter()
        .filter(|project| project.source_path == root.join("source/code").to_string_lossy())
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 1);
    let output = root.join(filename);
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
    let preview = inspect_package(&output)?;
    assert_eq!(preview.manifest.mode, PackageMode::SessionsOnly);
    assert_eq!(preview.manifest.counts.project_files, 0);
    Ok((
        preview,
        RestorePlanningOptions {
            project_destinations: HashMap::from([(
                references[0].project_id,
                root.join("target/already-cloned"),
            )]),
            defer_conflicting_conversations: true,
        },
    ))
}

fn plan_native_import(
    root: &Path,
    preview: &PackagePreview,
    options: &RestorePlanningOptions,
) -> Result<RestorePlan, Box<dyn Error>> {
    let inventory = discover_codex(Some(root.join("target/.codex")))?;
    Ok(build_restore_plan_with_options(
        preview,
        &TargetInventory {
            codex_home: inventory.codex_home,
            target_os: SourceOs::Macos,
            target_arch: std::env::consts::ARCH.into(),
            counts: inventory.counts,
            projects: inventory.projects,
            conversations: inventory.conversations,
        },
        &root.join("unused-project-root"),
        None,
        options,
    )?)
}

fn apply_native_import(root: &Path, plan: RestorePlan) -> Result<(), Box<dyn Error>> {
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
    Ok(())
}
