//! Timelock administration helpers for TalosRegistry.
//!
//! Provides the data types and pure-logic helpers used by the timelock
//! entry-points (`set_timelock_config`, `schedule_action`, `execute_action`,
//! `cancel_action`, `get_timelock_proposal`). The actual contract
//! entry-points live in `lib.rs`; they delegate to these helpers after
//! performing authorization checks.

use soroban_sdk::{Address, Env};

use crate::{
    events::{
        emit_timelock_cancelled, emit_timelock_config_changed, emit_timelock_executed,
        emit_timelock_scheduled,
    },
    AdminAction, DataKey, ProposalStatus, TimelockConfig, TimelockProposal,
    DEFAULT_GRACE_PERIOD, MAX_MIN_DELAY,
};

/// Read the current timelock config from storage, returning a safe default
/// (no delay, 7-day grace period) when nothing has been written yet.
pub(crate) fn get_timelock_config(e: &Env) -> TimelockConfig {
    e.storage()
        .persistent()
        .get(&DataKey::TimelockConfig)
        .unwrap_or(TimelockConfig {
            min_delay: 0,
            grace_period: DEFAULT_GRACE_PERIOD,
        })
}

/// Validate and persist a new timelock config, then emit `tl_cfg`.
pub(crate) fn set_timelock_config_internal(e: &Env, min_delay: u64, grace_period: u64) {
    if grace_period == 0 {
        panic!("Grace period must be positive");
    }
    if min_delay > MAX_MIN_DELAY {
        panic!("Min delay exceeds maximum limit");
    }

    let old_config = get_timelock_config(e);

    let new_config = TimelockConfig {
        min_delay,
        grace_period,
    };

    e.storage()
        .persistent()
        .set(&DataKey::TimelockConfig, &new_config);

    emit_timelock_config_changed(e, old_config.min_delay, min_delay, grace_period);
}

/// Create a new timelock proposal, persist it, and emit `tl_sch`.
/// Returns the new proposal ID.
pub(crate) fn schedule_action_internal(
    e: &Env,
    admin: Address,
    action: AdminAction,
    delay: u64,
) -> u64 {
    let config = get_timelock_config(e);
    if delay < config.min_delay {
        panic!("Delay less than minimum required delay");
    }

    let id: u64 = e
        .storage()
        .persistent()
        .get(&DataKey::NextTimelockId)
        .unwrap_or(1);

    let now = e.ledger().timestamp();
    let eta = now.saturating_add(delay);

    let proposal = TimelockProposal {
        id,
        action: action.clone(),
        eta,
        status: ProposalStatus::Scheduled,
        scheduled_at: now,
        scheduled_by: admin.clone(),
    };

    e.storage()
        .persistent()
        .set(&DataKey::TimelockProposal(id), &proposal);
    e.storage()
        .persistent()
        .set(&DataKey::NextTimelockId, &(id + 1));

    emit_timelock_scheduled(e, id, &action, eta, admin);
    id
}

/// Cancel a scheduled proposal and emit `tl_cnl`.
pub(crate) fn cancel_action_internal(e: &Env, admin: Address, proposal_id: u64) {
    let mut proposal: TimelockProposal = e
        .storage()
        .persistent()
        .get(&DataKey::TimelockProposal(proposal_id))
        .expect("Timelock proposal not found");

    if proposal.status != ProposalStatus::Scheduled {
        panic!("Proposal not active");
    }

    proposal.status = ProposalStatus::Cancelled;
    e.storage()
        .persistent()
        .set(&DataKey::TimelockProposal(proposal_id), &proposal);

    emit_timelock_cancelled(e, proposal_id, &proposal.action, admin);
}

/// Validate that a proposal has matured and is still within the grace window.
/// Returns the proposal for the caller to dispatch.
///
/// Panics with descriptive messages on any failure:
/// - `"Timelock proposal not found"` — unknown ID
/// - `"Proposal not active"` — already executed or cancelled
/// - `"Timelock delay not met"` — ETA in the future
/// - `"Proposal expired"` — ETA + grace period in the past
pub(crate) fn validate_executable_proposal(e: &Env, proposal_id: u64) -> TimelockProposal {
    let proposal: TimelockProposal = e
        .storage()
        .persistent()
        .get(&DataKey::TimelockProposal(proposal_id))
        .expect("Timelock proposal not found");

    if proposal.status != ProposalStatus::Scheduled {
        panic!("Proposal not active");
    }

    let now = e.ledger().timestamp();
    if now < proposal.eta {
        panic!("Timelock delay not met");
    }

    let config = get_timelock_config(e);
    if now > proposal.eta.saturating_add(config.grace_period) {
        panic!("Proposal expired");
    }

    proposal
}

/// Mark a proposal as executed and emit `tl_exec`.
pub(crate) fn mark_executed(e: &Env, mut proposal: TimelockProposal) {
    let executor = proposal.scheduled_by.clone();
    proposal.status = ProposalStatus::Executed;
    e.storage()
        .persistent()
        .set(&DataKey::TimelockProposal(proposal.id), &proposal);
    emit_timelock_executed(e, proposal.id, &proposal.action, executor);
}
