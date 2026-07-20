use makina_core::api::{Event, RunId, RunStatus};
use makina_core::repository_lease::{RepositoryLeaseOperation, RepositoryLeaseOwner};

#[test]
fn waiting_status_and_event_round_trip_with_owner_identity() {
    let owner = RepositoryLeaseOwner {
        plan_dir: "docs/plans/0048-Test".into(),
        run_uid: "01TEST".into(),
        operation: RepositoryLeaseOperation::Run,
    };
    let status = RunStatus::WaitingForRepository {
        owner: Some(owner.clone()),
    };
    let encoded = serde_json::to_string(&status).unwrap();
    let decoded: RunStatus = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, status);
    let event = Event::RepositoryLeaseWaiting {
        run: RunId(7),
        owner: Some(owner),
    };
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(encoded.contains("repository_lease_waiting"));
    assert!(encoded.contains("01TEST"));
}
