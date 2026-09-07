// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-workflow/src/engine.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-workflow/src/host.rs
// License: Apache-2.0
// Lato changes: generalized scalar workflow accounting into atomic hierarchical multi-dimensional reservations

use crate::{TaskId, TaskUsage};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    InputTokens,
    OutputTokens,
    TotalTokens,
    ToolCalls,
    CostMicros,
    WallTimeMs,
    Retries,
    ChildTasks,
    Worktrees,
}

impl std::fmt::Display for BudgetDimension {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::TotalTokens => "total_tokens",
            Self::ToolCalls => "tool_calls",
            Self::CostMicros => "cost_micros",
            Self::WallTimeMs => "wall_time_ms",
            Self::Retries => "retries",
            Self::ChildTasks => "child_tasks",
            Self::Worktrees => "worktrees",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BudgetError {
    #[error("budget arithmetic overflow in {dimension}")]
    ArithmeticOverflow { dimension: BudgetDimension },
    #[error("budget arithmetic underflow in {dimension}")]
    ArithmeticUnderflow { dimension: BudgetDimension },
    #[error("budget limit exceeded in {dimension}: requested {requested}, available {available}")]
    LimitExceeded {
        dimension: BudgetDimension,
        requested: u64,
        available: u64,
    },
    #[error("cumulative budget usage regressed in {dimension}: {previous} to {next}")]
    UsageRegression {
        dimension: BudgetDimension,
        previous: u64,
        next: u64,
    },
    #[error(
        "actual budget usage exceeds reservation in {dimension}: reserved {reserved}, actual {actual}"
    )]
    ActualExceedsReservation {
        dimension: BudgetDimension,
        reserved: u64,
        actual: u64,
    },
    #[error(
        "unknown or already consumed budget reservation for task {task_id} generation {generation}"
    )]
    UnknownReservation { task_id: TaskId, generation: u64 },
    #[error("budget reservation contents do not match task {task_id} generation {generation}")]
    ReservationMismatch { task_id: TaskId, generation: u64 },
    #[error(
        "budget reservation was issued by a different account for task {task_id} generation {generation}"
    )]
    ForeignReservation { task_id: TaskId, generation: u64 },
    #[error("budget reservation generation counter exhausted")]
    GenerationExhausted,
}

impl BudgetError {
    pub const fn dimension(&self) -> Option<BudgetDimension> {
        match self {
            Self::ArithmeticOverflow { dimension }
            | Self::ArithmeticUnderflow { dimension }
            | Self::LimitExceeded { dimension, .. }
            | Self::UsageRegression { dimension, .. }
            | Self::ActualExceedsReservation { dimension, .. } => Some(*dimension),
            Self::UnknownReservation { .. }
            | Self::ReservationMismatch { .. }
            | Self::ForeignReservation { .. }
            | Self::GenerationExhausted => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BudgetAmount {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub tool_calls: u64,
    pub cost_micros: u64,
    pub wall_time_ms: u64,
    pub retries: u64,
    pub child_tasks: u64,
    pub worktrees: u64,
}

macro_rules! budget_fields {
    ($macro:ident) => {
        $macro! {
            input_tokens => InputTokens,
            output_tokens => OutputTokens,
            total_tokens => TotalTokens,
            tool_calls => ToolCalls,
            cost_micros => CostMicros,
            wall_time_ms => WallTimeMs,
            retries => Retries,
            child_tasks => ChildTasks,
            worktrees => Worktrees,
        }
    };
}

impl BudgetAmount {
    pub const ZERO: Self = Self {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        tool_calls: 0,
        cost_micros: 0,
        wall_time_ms: 0,
        retries: 0,
        child_tasks: 0,
        worktrees: 0,
    };

    pub fn checked_add(self, rhs: Self) -> Result<Self, BudgetError> {
        macro_rules! add_fields {
            ($($field:ident => $dimension:ident,)*) => {
                Ok(Self {
                    $($field: self.$field.checked_add(rhs.$field).ok_or(
                        BudgetError::ArithmeticOverflow {
                            dimension: BudgetDimension::$dimension,
                        },
                    )?,)*
                })
            };
        }
        budget_fields!(add_fields)
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, BudgetError> {
        macro_rules! sub_fields {
            ($($field:ident => $dimension:ident,)*) => {
                Ok(Self {
                    $($field: self.$field.checked_sub(rhs.$field).ok_or(
                        BudgetError::ArithmeticUnderflow {
                            dimension: BudgetDimension::$dimension,
                        },
                    )?,)*
                })
            };
        }
        budget_fields!(sub_fields)
    }

    pub fn first_excess(self, limit: Self) -> Option<BudgetDimension> {
        macro_rules! find_excess {
            ($($field:ident => $dimension:ident,)*) => {
                $(if self.$field > limit.$field {
                    return Some(BudgetDimension::$dimension);
                })*
            };
        }
        budget_fields!(find_excess);
        None
    }

    fn value(self, dimension: BudgetDimension) -> u64 {
        match dimension {
            BudgetDimension::InputTokens => self.input_tokens,
            BudgetDimension::OutputTokens => self.output_tokens,
            BudgetDimension::TotalTokens => self.total_tokens,
            BudgetDimension::ToolCalls => self.tool_calls,
            BudgetDimension::CostMicros => self.cost_micros,
            BudgetDimension::WallTimeMs => self.wall_time_ms,
            BudgetDimension::Retries => self.retries,
            BudgetDimension::ChildTasks => self.child_tasks,
            BudgetDimension::Worktrees => self.worktrees,
        }
    }

    fn first_regression(self, previous: Self) -> Option<BudgetDimension> {
        previous.first_excess(self)
    }
}

impl From<&TaskUsage> for BudgetAmount {
    fn from(usage: &TaskUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            tool_calls: usage.tool_calls,
            cost_micros: usage.cost_micros,
            retries: usage.retries,
            ..Self::ZERO
        }
    }
}

impl From<TaskUsage> for BudgetAmount {
    fn from(usage: TaskUsage) -> Self {
        Self::from(&usage)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BudgetLimits {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub cost_micros: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub retries: Option<u64>,
    pub child_tasks: Option<u64>,
    pub worktrees: Option<u64>,
}

impl BudgetLimits {
    pub const fn limited(maximum: BudgetAmount) -> Self {
        Self {
            input_tokens: Some(maximum.input_tokens),
            output_tokens: Some(maximum.output_tokens),
            total_tokens: Some(maximum.total_tokens),
            tool_calls: Some(maximum.tool_calls),
            cost_micros: Some(maximum.cost_micros),
            wall_time_ms: Some(maximum.wall_time_ms),
            retries: Some(maximum.retries),
            child_tasks: Some(maximum.child_tasks),
            worktrees: Some(maximum.worktrees),
        }
    }

    pub const fn unlimited() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            tool_calls: None,
            cost_micros: None,
            wall_time_ms: None,
            retries: None,
            child_tasks: None,
            worktrees: None,
        }
    }

    fn first_excess(&self, amount: BudgetAmount) -> Option<BudgetDimension> {
        macro_rules! find_excess {
            ($($field:ident => $dimension:ident,)*) => {
                $(if self.$field.is_some_and(|limit| amount.$field > limit) {
                    return Some(BudgetDimension::$dimension);
                })*
            };
        }
        budget_fields!(find_excess);
        None
    }

    fn value(&self, dimension: BudgetDimension) -> Option<u64> {
        match dimension {
            BudgetDimension::InputTokens => self.input_tokens,
            BudgetDimension::OutputTokens => self.output_tokens,
            BudgetDimension::TotalTokens => self.total_tokens,
            BudgetDimension::ToolCalls => self.tool_calls,
            BudgetDimension::CostMicros => self.cost_micros,
            BudgetDimension::WallTimeMs => self.wall_time_ms,
            BudgetDimension::Retries => self.retries,
            BudgetDimension::ChildTasks => self.child_tasks,
            BudgetDimension::Worktrees => self.worktrees,
        }
    }

    fn remaining_after(&self, committed: BudgetAmount) -> Self {
        macro_rules! subtract_committed {
            ($($field:ident => $dimension:ident,)*) => {
                Self {
                    $($field: self.$field.map(|limit| {
                        limit.checked_sub(committed.$field).expect(
                            "private budget state always satisfies each configured limit",
                        )
                    }),)*
                }
            };
        }
        budget_fields!(subtract_committed)
    }
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self::unlimited()
    }
}

#[derive(Debug)]
struct BudgetAccountIdentity;

#[derive(Clone, Debug)]
pub struct BudgetReservation {
    pub task_id: TaskId,
    pub amount: BudgetAmount,
    generation: u64,
    issuer: Arc<BudgetAccountIdentity>,
}

impl PartialEq for BudgetReservation {
    fn eq(&self, other: &Self) -> bool {
        self.task_id == other.task_id
            && self.amount == other.amount
            && self.generation == other.generation
            && Arc::ptr_eq(&self.issuer, &other.issuer)
    }
}

impl Eq for BudgetReservation {}

impl BudgetReservation {
    pub const fn amount(&self) -> BudgetAmount {
        self.amount
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug)]
pub struct BudgetAccount {
    identity: Arc<BudgetAccountIdentity>,
    limits: BudgetLimits,
    spent: BudgetAmount,
    reserved: BudgetAmount,
    next_generation: u64,
    open: HashMap<(TaskId, u64), BudgetAmount>,
}

impl BudgetAccount {
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            identity: Arc::new(BudgetAccountIdentity),
            limits,
            spent: BudgetAmount::ZERO,
            reserved: BudgetAmount::ZERO,
            next_generation: 0,
            open: HashMap::new(),
        }
    }

    pub fn limits(&self) -> &BudgetLimits {
        &self.limits
    }

    pub const fn spent(&self) -> BudgetAmount {
        self.spent
    }

    pub const fn reserved(&self) -> BudgetAmount {
        self.reserved
    }

    pub fn remaining(&self) -> BudgetLimits {
        let committed = self
            .spent
            .checked_add(self.reserved)
            .expect("private budget accounting never overflows");
        self.limits.remaining_after(committed)
    }

    pub fn open_reservations(&self) -> usize {
        self.open.len()
    }

    pub fn reserve(
        &mut self,
        task_id: TaskId,
        amount: BudgetAmount,
    ) -> Result<BudgetReservation, BudgetError> {
        let candidate_reserved = self.reserved.checked_add(amount)?;
        self.check_limit(self.spent, candidate_reserved)?;
        let next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(BudgetError::GenerationExhausted)?;
        let reservation = BudgetReservation {
            task_id,
            amount,
            generation: self.next_generation,
            issuer: Arc::clone(&self.identity),
        };

        self.open.insert(
            (reservation.task_id.clone(), reservation.generation),
            reservation.amount,
        );
        self.reserved = candidate_reserved;
        self.next_generation = next_generation;
        Ok(reservation)
    }

    pub fn release(&mut self, reservation: BudgetReservation) -> Result<(), BudgetError> {
        let stored = self.reservation_amount(&reservation)?;
        let candidate_reserved = self.reserved.checked_sub(stored)?;

        self.open
            .remove(&(reservation.task_id, reservation.generation));
        self.reserved = candidate_reserved;
        Ok(())
    }

    pub fn settle(
        &mut self,
        reservation: BudgetReservation,
        actual: BudgetAmount,
    ) -> Result<(), BudgetError> {
        let stored = self.reservation_amount(&reservation)?;
        if let Some(dimension) = actual.first_excess(stored) {
            return Err(BudgetError::ActualExceedsReservation {
                dimension,
                reserved: stored.value(dimension),
                actual: actual.value(dimension),
            });
        }

        let candidate_reserved = self.reserved.checked_sub(stored)?;
        let candidate_spent = self.spent.checked_add(actual)?;
        self.check_limit(candidate_spent, candidate_reserved)?;

        self.open
            .remove(&(reservation.task_id, reservation.generation));
        self.reserved = candidate_reserved;
        self.spent = candidate_spent;
        Ok(())
    }

    pub fn apply_cumulative_usage(
        &mut self,
        previous: BudgetAmount,
        next: BudgetAmount,
    ) -> Result<BudgetAmount, BudgetError> {
        if let Some(dimension) = next.first_regression(previous) {
            return Err(BudgetError::UsageRegression {
                dimension,
                previous: previous.value(dimension),
                next: next.value(dimension),
            });
        }
        let delta = next.checked_sub(previous)?;
        let candidate_spent = self.spent.checked_add(delta)?;
        self.check_limit(candidate_spent, self.reserved)?;

        self.spent = candidate_spent;
        Ok(delta)
    }

    fn reservation_amount(
        &self,
        reservation: &BudgetReservation,
    ) -> Result<BudgetAmount, BudgetError> {
        if !Arc::ptr_eq(&self.identity, &reservation.issuer) {
            return Err(BudgetError::ForeignReservation {
                task_id: reservation.task_id.clone(),
                generation: reservation.generation,
            });
        }
        let key = (reservation.task_id.clone(), reservation.generation);
        let Some(stored) = self.open.get(&key).copied() else {
            return Err(BudgetError::UnknownReservation {
                task_id: reservation.task_id.clone(),
                generation: reservation.generation,
            });
        };
        if stored != reservation.amount {
            return Err(BudgetError::ReservationMismatch {
                task_id: reservation.task_id.clone(),
                generation: reservation.generation,
            });
        }
        Ok(stored)
    }

    fn check_limit(&self, spent: BudgetAmount, reserved: BudgetAmount) -> Result<(), BudgetError> {
        let committed = spent.checked_add(reserved)?;
        if let Some(dimension) = self.limits.first_excess(committed) {
            return Err(BudgetError::LimitExceeded {
                dimension,
                requested: committed.value(dimension),
                available: self
                    .limits
                    .value(dimension)
                    .expect("exceeded dimensions always have a configured limit"),
            });
        }
        Ok(())
    }
}

impl Default for BudgetAccount {
    fn default() -> Self {
        Self::new(BudgetLimits::unlimited())
    }
}
