use makina_core::roles::{DEVELOPER_SYSTEM_PROMPT, REVIEWER_SYSTEM_PROMPT};

#[test]
fn worker_roles_reserve_all_coordinator_status_documents() {
    for prompt in [DEVELOPER_SYSTEM_PROMPT, REVIEWER_SYSTEM_PROMPT] {
        assert!(prompt.contains("docs/plans/STATUS.md"));
        assert!(prompt.contains("STATUS.md"));
        assert!(prompt.contains("tasks/*.md"));
        assert!(prompt.contains("coordinator"));
    }
}
