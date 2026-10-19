//! Emergency pause and guardian helpers for TalosRegistry.
//!
//! Exposes pure-logic helpers used by the pause/guardian entry-points in
//! `lib.rs`. Storage reads and writes happen directly against the ledger;
//! no caching layer is used so behaviour is identical to the inline code
//! it replaces.

use soroban_sdk::{Address, Env, Vec};

use crate::{
    events::{emit_guardian_added, emit_guardian_removed, emit_pause_cleared, emit_pause_set},
    DataKey, PauseDomain, PauseInfo, PauseState,
    MAX_ADMIN_PAUSE_SECS, MAX_GUARDIANS, MAX_GUARDIAN_PAUSE_SECS,
};

// ── Guardian helpers ─────────────────────────────────────────────────

pub(crate) fn get_guardians(e: &Env) -> Vec<Address> {
    e.storage()
        .persistent()
        .get(&DataKey::Guardians)
        .unwrap_or(Vec::new(e))
}

pub(crate) fn is_guardian_internal(e: &Env, addr: &Address) -> bool {
    get_guardians(e).iter().any(|g| g == *addr)
}

/// Add `guardian` to the registered set, emitting `guard_add`.
/// Idempotent: adding an already-registered guardian is a no-op.
///
/// # Panics
/// - `"Guardian limit reached"` — when the set already holds [`MAX_GUARDIANS`].
pub(crate) fn add_guardian_internal(e: &Env, guardian: Address) {
    let mut guardians = get_guardians(e);
    if guardians.iter().any(|g| g == guardian) {
        return;
    }
    if guardians.len() >= MAX_GUARDIANS {
        panic!("Guardian limit reached");
    }
    guardians.push_back(guardian.clone());
    e.storage().persistent().set(&DataKey::Guardians, &guardians);
    emit_guardian_added(e, guardian);
}

/// Remove `guardian` from the registered set, emitting `guard_rem`.
/// Idempotent: removing a non-existent address is a no-op.
pub(crate) fn remove_guardian_internal(e: &Env, guardian: Address) {
    let guardians = get_guardians(e);
    let mut remaining = Vec::new(e);
    let mut found = false;
    for g in guardians.iter() {
        if g == guardian {
            found = true;
            continue;
        }
        remaining.push_back(g);
    }
    if !found {
        return;
    }
    e.storage().persistent().set(&DataKey::Guardians, &remaining);
    emit_guardian_removed(e, guardian);
}

// ── Domain pause helpers ─────────────────────────────────────────────

/// Evaluate whether `domain` is currently paused, lazily treating an expired
/// (non-indefinite) pause record as inactive without mutating storage.
pub(crate) fn is_domain_paused(e: &Env, domain: &PauseDomain) -> bool {
    match e
        .storage()
        .persistent()
        .get::<_, PauseState>(&DataKey::PauseState(domain.clone()))
    {
        None => false,
        Some(state) => state.expires_at == 0 || e.ledger().timestamp() < state.expires_at,
    }
}

/// Panic with `"Write path paused"` if `domain` is currently paused.
#[allow(dead_code)]
pub(crate) fn require_not_paused(e: &Env, domain: PauseDomain) {
    if is_domain_paused(e, &domain) {
        panic!("Write path paused");
    }
}

/// Apply a pause to `domain` on behalf of `caller`.
///
/// Duration semantics:
/// - Admin (`is_admin_caller = true`): `duration = 0` → indefinite; non-zero
///   must be ≤ [`MAX_ADMIN_PAUSE_SECS`].
/// - Guardian (`is_admin_caller = false`): `duration` must be 1..=
///   [`MAX_GUARDIAN_PAUSE_SECS`] — guardians can never impose indefinite pauses.
///
/// Idempotent: calling again overwrites the existing record, **except** that a
/// guardian cannot overwrite a pause the admin established.
///
/// # Panics
/// - `"Domain locked by admin; guardians cannot modify"` — guardian tried to
///   overwrite an admin lock.
/// - `"Duration exceeds admin maximum"` — admin non-zero duration too large.
/// - `"Guardian pause duration out of bounds"` — guardian duration invalid.
pub(crate) fn apply_pause(
    e: &Env,
    caller: &Address,
    admin: &Address,
    is_admin_caller: bool,
    domain: PauseDomain,
    duration: u64,
) {
    let key = DataKey::PauseState(domain.clone());

    if !is_admin_caller {
        if let Some(existing) = e.storage().persistent().get::<_, PauseState>(&key) {
            if existing.paused_by == *admin {
                panic!("Domain locked by admin; guardians cannot modify");
            }
        }
    }

    let now = e.ledger().timestamp();
    let expires_at = if is_admin_caller {
        if duration == 0 {
            0
        } else {
            if duration > MAX_ADMIN_PAUSE_SECS {
                panic!("Duration exceeds admin maximum");
            }
            now.saturating_add(duration)
        }
    } else {
        if duration == 0 || duration > MAX_GUARDIAN_PAUSE_SECS {
            panic!("Guardian pause duration out of bounds");
        }
        now.saturating_add(duration)
    };

    e.storage().persistent().set(
        &key,
        &PauseState {
            paused_by: caller.clone(),
            paused_at: now,
            expires_at,
        },
    );

    emit_pause_set(e, &domain, caller, expires_at);
}

/// Lift a pause from `domain`. No-op if the domain is not currently paused.
/// Emits `pause_off` only when a record was actually present.
pub(crate) fn clear_pause(e: &Env, admin: &Address, domain: PauseDomain) {
    let key = DataKey::PauseState(domain.clone());
    if e.storage().persistent().get::<_, PauseState>(&key).is_none() {
        return;
    }
    e.storage().persistent().remove(&key);
    emit_pause_cleared(e, &domain, admin);
}

/// Build a read-friendly [`PauseInfo`] view from raw storage.
pub(crate) fn pause_info(e: &Env, domain: PauseDomain) -> PauseInfo {
    match e
        .storage()
        .persistent()
        .get::<_, PauseState>(&DataKey::PauseState(domain.clone()))
    {
        None => PauseInfo {
            active: false,
            paused_by: None,
            paused_at: None,
            expires_at: None,
        },
        Some(state) => {
            let active = state.expires_at == 0 || e.ledger().timestamp() < state.expires_at;
            PauseInfo {
                active,
                paused_by: Some(state.paused_by),
                paused_at: Some(state.paused_at),
                expires_at: if state.expires_at == 0 {
                    None
                } else {
                    Some(state.expires_at)
                },
            }
        }
    }
}
