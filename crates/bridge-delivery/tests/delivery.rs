mod common;
use common::Fixture;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};
#[test]
fn byte_complete_artifact_supports_binary_delete_modes_and_symlink() {
    let f = Fixture::new();
    f.changes();
    let before = bridge_git::take_snapshot(f.project.workspace())
        .unwrap()
        .to_json()
        .unwrap();
    let report = bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    assert_eq!(report["status"], "built");
    let artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
    assert_eq!(artifact.entries.len(), 5);
    for e in &artifact.entries {
        if let Some(hash) = &e.blob_sha256 {
            assert_eq!(
                Some(fs::read(f.dest().join("blobs").join(hash)).unwrap().len() as u64),
                e.size
            );
        }
    }
    assert!(artifact.entries.iter().any(|e| e.op == "mode_change"));
    assert!(artifact.entries.iter().any(|e| e.op == "delete"));
    assert!(artifact.entries.iter().any(|e| e.kind == "symlink"));
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id).unwrap()["status"],
        "validated"
    );
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace())
            .unwrap()
            .to_json()
            .unwrap(),
        before
    );
}
#[test]
fn preflight_refuses_dirty_drift_tamper_and_outside_scope_without_target_writes() {
    let f = Fixture::new();
    f.changes();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    fs::write(f.project.workspace().join("outside"), "dirty").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "dirty_main_workspace"
    );
    fs::remove_file(f.project.workspace().join("outside")).unwrap();
    fs::write(f.checkout.join("src/a"), "drift").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "artifact_drift"
    );
    f.changes_without_link();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    let artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
    let hash = artifact
        .entries
        .iter()
        .find_map(|e| e.blob_sha256.as_ref())
        .unwrap();
    fs::write(f.dest().join("blobs").join(hash), "corrupt").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "artifact_corrupt"
    );
    fs::write(f.checkout.join("outside"), "outside scope").unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "out_of_scope_changes"
    );
    assert_eq!(
        fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"base\0binary"
    );
}
impl Fixture {
    fn changes_without_link(&self) {
        fs::write(self.checkout.join("src/a"), b"result\xff\0").unwrap();
    }
}
#[test]
fn accepted_guard_and_writer_reservation_gate_precede_artifact_writes() {
    let f = Fixture::new();
    f.changes();
    let s = f.layout.open().unwrap();
    s.connection()
        .execute("UPDATE tasks SET status='awaiting_review'", [])
        .unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "not_accepted"
    );
    assert!(!f.dest().exists());
    s.connection()
        .execute("UPDATE tasks SET status='accepted'", [])
        .unwrap();
    s.connection().execute("INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,'proj','[\"src/\"]','stamp',1)",[f.id.to_string()]).unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "active_writers"
    );
    assert!(!f.dest().exists());
}
#[test]
fn apply_and_every_durable_boundary_resume_preserve_index_and_head() {
    let phases = std::iter::once("after_journal".to_owned())
        .chain((0..5).flat_map(|i| [format!("after_op:{i}"), format!("after_applied:{i}")]))
        .chain(["before_post_verify".into(), "after_post_verify".into()]);
    for phase in phases {
        let f = Fixture::new();
        f.changes();
        bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
        let head = bridge_git::head(f.project.workspace()).unwrap();
        let index = bridge_git::index_fingerprint(f.project.workspace()).unwrap();
        let index_bytes = fs::read(f.project.workspace().join(".git/index")).unwrap();
        assert_eq!(
            bridge_delivery::apply_with_fault(&f.layout, &f.project, f.id, |at| at == phase)
                .unwrap_err()
                .code,
            "simulated_crash",
            "{phase}"
        );
        let state = f
            .layout
            .open()
            .unwrap()
            .get_worktree(f.id, f.project.id())
            .unwrap()
            .unwrap();
        assert_eq!(
            state.delivery_state,
            Some(bridge_storage::WorktreeDeliveryState::Applying)
        );
        assert!(state.delivered_at.is_none());
        assert_eq!(
            bridge_delivery::dry_run(&f.layout, &f.project, f.id).unwrap()["status"],
            "validated",
            "{phase}"
        );
        assert_eq!(
            bridge_delivery::apply(&f.layout, &f.project, f.id).unwrap()["status"],
            "delivered",
            "{phase}"
        );
        assert_eq!(bridge_git::head(f.project.workspace()).unwrap(), head);
        assert_eq!(
            bridge_git::index_fingerprint(f.project.workspace()).unwrap(),
            index
        );
        assert_eq!(
            fs::read(f.project.workspace().join(".git/index")).unwrap(),
            index_bytes
        );
        assert_eq!(
            fs::read(f.project.workspace().join("src/a")).unwrap(),
            b"result\xff\0"
        );
        assert!(!f.project.workspace().join("src/remove").exists());
        assert_eq!(
            fs::read_link(f.project.workspace().join("src/link")).unwrap(),
            PathBuf::from("new")
        );
        assert_eq!(
            fs::metadata(f.project.workspace().join("src/executable"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        assert_eq!(
            bridge_delivery::apply(&f.layout, &f.project, f.id)
                .unwrap_err()
                .code,
            "already_delivered"
        );
    }
}
#[test]
fn third_state_resume_refuses_before_more_writes_and_reverted_marker_reapplies() {
    let f = Fixture::new();
    f.changes();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    assert!(
        bridge_delivery::apply_with_fault(&f.layout, &f.project, f.id, |at| at
            == "after_applied:0")
        .is_err()
    );
    fs::write(f.project.workspace().join("src/a"), "third state").unwrap();
    let before = bridge_git::take_snapshot(f.project.workspace())
        .unwrap()
        .to_json()
        .unwrap();
    for apply in [false, true] {
        let error = if apply {
            bridge_delivery::apply(&f.layout, &f.project, f.id)
        } else {
            bridge_delivery::dry_run(&f.layout, &f.project, f.id)
        }
        .unwrap_err();
        assert_eq!(error.code, "needs_manual_recovery");
        assert_eq!(
            bridge_git::take_snapshot(f.project.workspace())
                .unwrap()
                .to_json()
                .unwrap(),
            before
        );
    }
    // The persisted applied marker is advisory; exact base state may be safely reapplied.
    fs::write(f.project.workspace().join("src/a"), b"base\0binary").unwrap();
    assert_eq!(
        bridge_delivery::apply(&f.layout, &f.project, f.id).unwrap()["status"],
        "delivered"
    );
}
#[test]
fn apply_preflight_requires_artifact_and_rejects_symlink_parent_without_writes() {
    let f = Fixture::new();
    f.changes();
    assert_eq!(
        bridge_delivery::apply(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "artifact_missing"
    );
    assert_eq!(
        fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"base\0binary"
    );
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    let saved = f.root.join("saved");
    fs::rename(f.project.workspace().join("src"), &saved).unwrap();
    symlink(&saved, f.project.workspace().join("src")).unwrap();
    assert!(bridge_delivery::apply(&f.layout, &f.project, f.id).is_err());
    assert_eq!(fs::read(saved.join("a")).unwrap(), b"base\0binary");
}
