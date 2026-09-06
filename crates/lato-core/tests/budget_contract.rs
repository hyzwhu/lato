use lato_core::{BudgetAccount, BudgetAmount, BudgetDimension, BudgetError, BudgetLimits, TaskId};

fn amount(tokens: u64, tools: u64, children: u64) -> BudgetAmount {
    BudgetAmount {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: tokens,
        tool_calls: tools,
        cost_micros: 0,
        wall_time_ms: 0,
        retries: 0,
        child_tasks: children,
        worktrees: 0,
    }
}

#[test]
fn failed_multi_dimension_reservation_is_atomic() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 2, 1)));
    let error = account
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap_err();

    assert_eq!(error.dimension(), Some(BudgetDimension::ToolCalls));
    assert_eq!(account.reserved(), BudgetAmount::ZERO);
    assert_eq!(account.spent(), BudgetAmount::ZERO);
}

#[test]
fn settlement_returns_unused_reservation() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account
        .reserve(TaskId::from("child"), amount(80, 4, 1))
        .unwrap();

    account.settle(reservation, amount(30, 2, 1)).unwrap();

    assert_eq!(account.spent(), amount(30, 2, 1));
    assert_eq!(account.reserved(), BudgetAmount::ZERO);
    assert_eq!(account.remaining(), BudgetLimits::limited(amount(70, 8, 1)));
}

#[test]
fn duplicate_settlement_cannot_charge_twice() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap();

    account
        .settle(reservation.clone(), amount(20, 1, 1))
        .unwrap();
    let duplicate = account.settle(reservation, amount(20, 1, 1)).unwrap_err();
    assert_eq!(duplicate.dimension(), None);
    assert_eq!(account.spent(), amount(20, 1, 1));
}

#[test]
fn release_returns_the_entire_reservation_and_is_single_use() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap();

    account.release(reservation.clone()).unwrap();

    assert_eq!(account.reserved(), BudgetAmount::ZERO);
    assert_eq!(
        account.remaining(),
        BudgetLimits::limited(amount(100, 10, 2))
    );
    let duplicate = account.release(reservation).unwrap_err();
    assert_eq!(duplicate.dimension(), None);
}

#[test]
fn unlimited_account_has_no_remaining_ceiling_but_still_tracks_values() {
    let mut account = BudgetAccount::new(BudgetLimits::unlimited());
    let reservation = account
        .reserve(TaskId::from("child"), amount(10_000, 500, 20))
        .unwrap();

    assert_eq!(account.remaining(), BudgetLimits::unlimited());
    account.settle(reservation, amount(9_000, 450, 18)).unwrap();
    assert_eq!(account.spent(), amount(9_000, 450, 18));
}

#[test]
fn checked_arithmetic_reports_the_first_overflow_dimension() {
    let lhs = BudgetAmount {
        input_tokens: u64::MAX,
        ..BudgetAmount::ZERO
    };
    let rhs = BudgetAmount {
        input_tokens: 1,
        ..BudgetAmount::ZERO
    };

    let error = lhs.checked_add(rhs).unwrap_err();

    assert_eq!(error.dimension(), Some(BudgetDimension::InputTokens));
    assert!(matches!(error, BudgetError::ArithmeticOverflow { .. }));
}

#[test]
fn usage_regression_is_rejected_without_changing_spend() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let previous = amount(40, 4, 0);
    account
        .apply_cumulative_usage(BudgetAmount::ZERO, previous)
        .unwrap();

    let error = account
        .apply_cumulative_usage(previous, amount(39, 5, 0))
        .unwrap_err();

    assert_eq!(error.dimension(), Some(BudgetDimension::TotalTokens));
    assert!(matches!(error, BudgetError::UsageRegression { .. }));
    assert_eq!(account.spent(), previous);
}

#[test]
fn cumulative_usage_returns_delta_and_respects_open_reservations() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let _reservation = account
        .reserve(TaskId::from("child"), amount(60, 4, 1))
        .unwrap();

    let delta = account
        .apply_cumulative_usage(BudgetAmount::ZERO, amount(30, 2, 0))
        .unwrap();
    assert_eq!(delta, amount(30, 2, 0));

    let error = account
        .apply_cumulative_usage(amount(30, 2, 0), amount(50, 7, 0))
        .unwrap_err();
    assert_eq!(error.dimension(), Some(BudgetDimension::TotalTokens));
    assert_eq!(account.spent(), amount(30, 2, 0));
}

#[test]
fn actual_usage_above_reservation_is_rejected_atomically() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account
        .reserve(TaskId::from("child"), amount(40, 3, 1))
        .unwrap();

    let error = account
        .settle(reservation.clone(), amount(41, 3, 1))
        .unwrap_err();

    assert_eq!(error.dimension(), Some(BudgetDimension::TotalTokens));
    assert!(matches!(
        error,
        BudgetError::ActualExceedsReservation { .. }
    ));
    assert_eq!(account.spent(), BudgetAmount::ZERO);
    assert_eq!(account.reserved(), amount(40, 3, 1));

    account.settle(reservation, amount(35, 2, 1)).unwrap();
}

#[test]
fn nested_child_spend_can_be_rolled_up_to_the_parent() {
    let mut parent = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 3)));
    let child_reservation = parent
        .reserve(TaskId::from("child"), amount(70, 7, 2))
        .unwrap();
    let mut child = BudgetAccount::new(BudgetLimits::limited(child_reservation.amount()));
    let grandchild = child
        .reserve(TaskId::from("grandchild"), amount(40, 4, 1))
        .unwrap();

    child.settle(grandchild, amount(25, 2, 1)).unwrap();
    parent.settle(child_reservation, child.spent()).unwrap();

    assert_eq!(parent.spent(), amount(25, 2, 1));
    assert_eq!(parent.remaining(), BudgetLimits::limited(amount(75, 8, 2)));
}

#[test]
fn reservations_for_the_same_task_have_unique_generations() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 3)));
    let first = account
        .reserve(TaskId::from("child"), amount(10, 1, 1))
        .unwrap();
    let second = account
        .reserve(TaskId::from("child"), amount(20, 2, 1))
        .unwrap();

    assert_ne!(first, second);
    assert_ne!(first.generation(), second.generation());
    assert_eq!(account.open_reservations(), 2);
}

#[test]
fn mixed_limits_preserve_unlimited_dimensions_in_remaining_state() {
    let limits = BudgetLimits {
        input_tokens: None,
        output_tokens: None,
        total_tokens: Some(100),
        tool_calls: None,
        cost_micros: Some(1_000),
        wall_time_ms: None,
        retries: Some(2),
        child_tasks: Some(1),
        worktrees: None,
    };
    let mut account = BudgetAccount::new(limits);
    let reservation = account
        .reserve(
            TaskId::from("child"),
            BudgetAmount {
                input_tokens: u64::MAX,
                output_tokens: 75,
                total_tokens: 40,
                tool_calls: 50,
                cost_micros: 300,
                wall_time_ms: 90_000,
                retries: 1,
                child_tasks: 1,
                worktrees: 8,
            },
        )
        .unwrap();

    assert_eq!(
        account.remaining(),
        BudgetLimits {
            input_tokens: None,
            output_tokens: None,
            total_tokens: Some(60),
            tool_calls: None,
            cost_micros: Some(700),
            wall_time_ms: None,
            retries: Some(1),
            child_tasks: Some(0),
            worktrees: None,
        }
    );
    account.release(reservation).unwrap();
}

#[test]
fn reservation_cannot_be_consumed_by_an_identical_foreign_account() {
    let limits = BudgetLimits::limited(amount(100, 10, 2));
    let mut issuer = BudgetAccount::new(limits.clone());
    let mut foreign = BudgetAccount::new(limits);
    let issued = issuer
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap();
    let foreign_twin = foreign
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap();

    let release_error = foreign.release(issued.clone()).unwrap_err();
    assert!(matches!(
        release_error,
        BudgetError::ForeignReservation { .. }
    ));
    assert_eq!(release_error.dimension(), None);

    let settle_error = foreign
        .settle(issued.clone(), amount(20, 1, 1))
        .unwrap_err();
    assert!(matches!(
        settle_error,
        BudgetError::ForeignReservation { .. }
    ));
    assert_eq!(settle_error.dimension(), None);
    assert_eq!(foreign.reserved(), amount(50, 3, 1));
    assert_eq!(foreign.spent(), BudgetAmount::ZERO);

    issuer.settle(issued, amount(20, 1, 1)).unwrap();
    foreign.release(foreign_twin).unwrap();
}

#[test]
fn altered_reservation_is_an_identity_error_without_a_dimension() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account
        .reserve(TaskId::from("child"), amount(50, 3, 1))
        .unwrap();
    let mut altered = reservation.clone();
    altered.amount.total_tokens = 49;

    let error = account.release(altered).unwrap_err();

    assert!(matches!(error, BudgetError::ReservationMismatch { .. }));
    assert_eq!(error.dimension(), None);
    assert_eq!(account.reserved(), amount(50, 3, 1));
    account.release(reservation).unwrap();
}
