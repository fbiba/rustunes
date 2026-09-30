//! Real private SQLite materialization, not constructed guards or fake receipts.
use super::*;

#[test]
fn owned_materialization_retains_real_creation_custody_and_original_control() {
    let case = Case::with_wal();
    let before = case.bytes();
    let (budget, flag) = controlled();
    let deadline = budget.deadline;
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    owner.ensure_original_control(deadline, &flag).unwrap();
    owner
        .with_view(|view| {
            assert_eq!(
                view.snapshot().table("facts")?.rows.values().sum::<u32>(),
                2
            );
            assert!(!case.out("owned/database.sqlite-shm").exists());
            view.recheck()
        })
        .unwrap();
    owner.recheck().unwrap();
    assert_eq!(case.bytes(), before);
}
#[test]
fn owned_materialization_same_byte_leaf_replacement_after_return_refuses() {
    let case = Case::new();
    let (budget, _) = controlled();
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    replace_equal(
        &case.out("owned/database.sqlite"),
        &case.out("saved.sqlite"),
    );
    assert!(owner.recheck().is_err());
}
#[test]
fn owned_materialization_same_byte_root_replacement_after_return_refuses() {
    let case = Case::new();
    let (budget, _) = controlled();
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    fs::rename(case.out("owned"), case.out("saved")).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(case.out("owned"))
        .unwrap();
    fs::copy(
        case.out("saved/database.sqlite"),
        case.out("owned/database.sqlite"),
    )
    .unwrap();
    assert!(owner.recheck().is_err());
}
#[test]
fn owned_materialization_changed_source_and_new_scratch_namespace_refuse() {
    for source in [true, false] {
        let case = Case::new();
        let (budget, _) = controlled();
        let owner =
            materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
        if source {
            let mut bytes = fs::read(&case.database).unwrap();
            bytes[100] ^= 1;
            fs::write(&case.database, bytes).unwrap();
        } else {
            fs::write(case.out("owned/database.sqlite-journal"), b"unexpected").unwrap();
        }
        assert!(owner.recheck().is_err());
    }
}
#[test]
fn owned_materialization_rechecks_all_custody_after_visitor_refusal() {
    let case = Case::new();
    let (budget, _) = controlled();
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    let error = owner
        .with_view::<()>(|_| {
            replace_equal(
                &case.out("owned/database.sqlite"),
                &case.out("saved.sqlite"),
            );
            anyhow::bail!("intentional visitor refusal")
        })
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("intentional visitor refusal"));
    assert!(message.contains("SQL custody final check also failed"));
}
#[test]
fn owned_materialization_cancellation_and_foreign_control_refuse_without_reopening() {
    let case = Case::new();
    let (budget, flag) = controlled();
    let deadline = budget.deadline;
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    assert!(
        owner
            .ensure_original_control(deadline, &Arc::new(AtomicBool::new(false)))
            .is_err()
    );
    flag.store(true, Ordering::Relaxed);
    assert!(owner.recheck().is_err());
    assert!(
        owner
            .with_view::<()>(|_| panic!("cancelled owner dispatched visitor"))
            .is_err()
    );
}
#[test]
fn owned_materialization_expired_control_creates_no_output() {
    let case = Case::new();
    let budget = Budget::new_cancellable_until(Instant::now(), Arc::new(AtomicBool::new(false)));
    assert!(materialize_owned_controlled(case.capture(), &case.out("owned"), budget).is_err());
    assert!(!case.out("owned").exists());
}
#[test]
fn owned_materialization_hardlink_after_return_refuses() {
    let case = Case::new();
    let (budget, _) = controlled();
    let owner = materialize_owned_controlled(case.capture(), &case.out("owned"), budget).unwrap();
    fs::hard_link(case.out("owned/database.sqlite"), case.out("extra-link")).unwrap();
    assert!(owner.recheck().is_err());
}
