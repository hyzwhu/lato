use lato_core::{
    AgentId, AgentProfile, ErrorCategory, LeaseId, Retryability, SessionId, TaskError,
    TaskErrorCode, TaskId, TaskMachine, TaskOwner, TaskStatus, ToolCapability, TurnId,
    VerificationPolicy, WorkspaceIntent,
};

#[test]
fn task_ids_reject_empty_values() {
    assert!(TaskId::parse(" ").is_err());
    assert!(AgentId::parse("").is_err());
    assert!(LeaseId::parse("\t").is_err());

    assert!(serde_json::from_str::<TaskId>(r#"" ""#).is_err());
    assert!(serde_json::from_str::<AgentId>(r#""""#).is_err());
    assert!(serde_json::from_str::<LeaseId>(r#""\n\t""#).is_err());
}

#[test]
fn task_owner_round_trips_with_explicit_tag() {
    let owner = TaskOwner::Interactive {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
    };
    let value = serde_json::to_value(&owner).unwrap();
    assert_eq!(value["type"], "interactive");
    assert_eq!(serde_json::from_value::<TaskOwner>(value).unwrap(), owner);
}

#[test]
fn terminal_task_cannot_transition_again() {
    let mut machine = TaskMachine::new(TaskStatus::Queued);
    machine.transition(TaskStatus::Preparing).unwrap();
    machine.transition(TaskStatus::Running).unwrap();
    machine.transition(TaskStatus::Verifying).unwrap();
    machine.transition(TaskStatus::Completed).unwrap();
    assert!(machine.transition(TaskStatus::Failed).is_err());
}

#[test]
fn transition_table_is_exhaustive_for_all_status_pairs() {
    const STATUSES: [TaskStatus; 10] = [
        TaskStatus::Queued,
        TaskStatus::Preparing,
        TaskStatus::Running,
        TaskStatus::WaitingForChildren,
        TaskStatus::WaitingForApproval,
        TaskStatus::Verifying,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
        TaskStatus::TimedOut,
    ];
    const ALLOWED: [(TaskStatus, TaskStatus); 30] = [
        (TaskStatus::Queued, TaskStatus::Preparing),
        (TaskStatus::Queued, TaskStatus::Failed),
        (TaskStatus::Queued, TaskStatus::Cancelled),
        (TaskStatus::Queued, TaskStatus::TimedOut),
        (TaskStatus::Preparing, TaskStatus::Running),
        (TaskStatus::Preparing, TaskStatus::Failed),
        (TaskStatus::Preparing, TaskStatus::Cancelled),
        (TaskStatus::Preparing, TaskStatus::TimedOut),
        (TaskStatus::Running, TaskStatus::WaitingForChildren),
        (TaskStatus::Running, TaskStatus::WaitingForApproval),
        (TaskStatus::Running, TaskStatus::Verifying),
        (TaskStatus::Running, TaskStatus::Failed),
        (TaskStatus::Running, TaskStatus::Cancelled),
        (TaskStatus::Running, TaskStatus::TimedOut),
        (TaskStatus::WaitingForChildren, TaskStatus::Running),
        (TaskStatus::WaitingForChildren, TaskStatus::Verifying),
        (TaskStatus::WaitingForChildren, TaskStatus::Failed),
        (TaskStatus::WaitingForChildren, TaskStatus::Cancelled),
        (TaskStatus::WaitingForChildren, TaskStatus::TimedOut),
        (TaskStatus::WaitingForApproval, TaskStatus::Running),
        (TaskStatus::WaitingForApproval, TaskStatus::Verifying),
        (TaskStatus::WaitingForApproval, TaskStatus::Failed),
        (TaskStatus::WaitingForApproval, TaskStatus::Cancelled),
        (TaskStatus::WaitingForApproval, TaskStatus::TimedOut),
        (TaskStatus::Verifying, TaskStatus::Completed),
        (TaskStatus::Verifying, TaskStatus::Failed),
        (TaskStatus::Verifying, TaskStatus::Cancelled),
        (TaskStatus::Verifying, TaskStatus::TimedOut),
        (TaskStatus::Verifying, TaskStatus::WaitingForChildren),
        (TaskStatus::Verifying, TaskStatus::WaitingForApproval),
    ];

    for from in STATUSES {
        for to in STATUSES {
            let expected = ALLOWED.contains(&(from, to));
            let mut machine = TaskMachine::new(from);
            assert_eq!(
                machine.transition(to).is_ok(),
                expected,
                "unexpected transition result for {from:?} -> {to:?}"
            );
            assert_eq!(
                machine.status(),
                if expected { to } else { from },
                "failed transition mutated state for {from:?} -> {to:?}"
            );
        }
    }
}

#[test]
fn profile_cannot_expand_parent_capabilities() {
    let profile = AgentProfile::worker();
    let parent = vec![ToolCapability::FileRead];
    let error = profile
        .effective_capabilities(&parent, Some(&[ToolCapability::FileWrite]))
        .unwrap_err();
    assert_eq!(error.code, TaskErrorCode::CapabilityExpansion);
}

#[test]
fn profile_intersects_parent_profile_and_request_in_stable_order() {
    let profile = AgentProfile::worker();
    let parent = vec![
        ToolCapability::NetworkRead,
        ToolCapability::FileWrite,
        ToolCapability::FileRead,
    ];
    assert_eq!(
        profile.effective_capabilities(&parent, None).unwrap(),
        vec![
            ToolCapability::NetworkRead,
            ToolCapability::FileWrite,
            ToolCapability::FileRead,
        ]
    );
    assert_eq!(
        profile
            .effective_capabilities(
                &parent,
                Some(&[ToolCapability::FileRead, ToolCapability::NetworkRead]),
            )
            .unwrap(),
        vec![ToolCapability::NetworkRead, ToolCapability::FileRead]
    );
}

#[test]
fn built_in_profiles_have_expected_workspace_intent() {
    assert_eq!(
        AgentProfile::explorer().workspace,
        WorkspaceIntent::SharedReadOnly
    );
    assert_eq!(
        AgentProfile::worker().workspace,
        WorkspaceIntent::IsolatedWorktree
    );
    assert_eq!(
        AgentProfile::reviewer().verification,
        VerificationPolicy::IndependentReview
    );
}

#[test]
fn task_error_maps_code_to_explicit_retryability() {
    let transient = TaskError::new(TaskErrorCode::QueueFull, "queue is full");
    let agent_error = transient.agent_error();
    assert_eq!(agent_error.category, ErrorCategory::Task);
    assert_eq!(agent_error.retryability, Retryability::AfterBackoff);

    let permanent = TaskError::new(TaskErrorCode::CapabilityExpansion, "not transient");
    assert_eq!(permanent.agent_error().retryability, Retryability::Never);
    let value = serde_json::to_value(&permanent).unwrap();
    assert_eq!(value["code"], "task.capability_expansion");
    assert!(value.get("retryability").is_none());
    assert_eq!(permanent.code, "task.capability_expansion");
}
