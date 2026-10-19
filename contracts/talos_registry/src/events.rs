//! Event emitters for TalosRegistry.
//!
//! All event helpers are `pub(crate)` free functions that take an `&Env`
//! and the typed arguments described in the event schema comment below.
//!
//! Event schema (topics → data):
//!   tls_crt  : (symbol, creator: Address)     → (talos_id: u32, name: String, category: String)
//!   pat_upd  : (symbol, talos_id: u32)        → (creator: Address, creator_share: u32, investor_share: u32)
//!   fee_chg  : (symbol,)                      → (old_bps: u32, new_bps: u32)
//!   adm_prp  : (symbol,)                      → (current: Address, proposed: Address)
//!   adm_acc  : (symbol,)                      → (new_admin: Address)
//!   adm_cnl  : (symbol,)                      → (cancelled: Address)
//!   tl_sch   : (symbol, proposal_id: u64)     → (action: AdminAction, eta: u64, proposer: Address)
//!   tl_exec  : (symbol, proposal_id: u64)     → (action: AdminAction, executor: Address)
//!   tl_cnl   : (symbol, proposal_id: u64)     → (action: AdminAction, canceller: Address)
//!   tl_cfg   : (symbol,)                      → (old_min_delay: u64, new_min_delay: u64, grace_period: u64)
//!   dep_path : (symbol,)                      → (deprecated: String, replacement: String)
//!                                                 Privacy-safe: no caller/tx/value data.
//!   pause_on : (symbol, domain: PauseDomain)  → (actor: Address, expires_at: u64)
//!   pause_off: (symbol, domain: PauseDomain)  → (actor: Address)
//!   guard_add: (symbol,)                      → (guardian: Address)
//!   guard_rem: (symbol,)                      → (guardian: Address)
//!   allow_add: (symbol,)                      → (asset: Address, admin: Address)
//!   allow_rem: (symbol,)                      → (asset: Address, admin: Address)
//!   tls_paus : (symbol, talos_id: u32)        → (controller: Address)
//!   tls_resu : (symbol, talos_id: u32)        → (controller: Address)

use soroban_sdk::{symbol_short, Address, Env, String};

use crate::{AdminAction, Patron, PauseDomain};

pub(crate) fn emit_talos_created(
    env: &Env,
    talos_id: u32,
    creator: Address,
    name: String,
    category: String,
) {
    let topics = (symbol_short!("tls_crt"), creator.clone());
    env.events()
        .publish(topics, (talos_id, name.clone(), category.clone()));

    let topics2 = (symbol_short!("tls_crt2"), creator);
    env.events().publish(topics2, (1u32, talos_id, name, category));
}

pub(crate) fn emit_patron_updated(env: &Env, talos_id: u32, patron: &Patron) {
    let topics = (symbol_short!("pat_upd"), talos_id);
    env.events().publish(
        topics,
        (
            patron.creator_addr.clone(),
            patron.creator_share,
            patron.investor_share,
        ),
    );
}

pub(crate) fn emit_protocol_fee_changed(env: &Env, old_bps: u32, new_bps: u32) {
    let topics = (symbol_short!("fee_chg"),);
    env.events().publish(topics, (old_bps, new_bps));
}

/// Emitted when the current admin nominates a new admin.
/// `proposed` is the address that must call `accept_admin` to complete the transfer.
pub(crate) fn emit_admin_proposed(env: &Env, current: Address, proposed: Address) {
    let topics = (symbol_short!("adm_prp"),);
    env.events().publish(topics, (current, proposed));
}

/// Emitted when the pending admin accepts the transfer.
/// `new_admin` is now the active protocol wallet / admin.
pub(crate) fn emit_admin_accepted(env: &Env, new_admin: Address) {
    let topics = (symbol_short!("adm_acc"),);
    env.events().publish(topics, (new_admin,));
}

/// Emitted when the current admin cancels an in-progress admin transfer.
/// `cancelled` is the address whose nomination was revoked.
pub(crate) fn emit_admin_cancelled(env: &Env, cancelled: Address) {
    let topics = (symbol_short!("adm_cnl"),);
    env.events().publish(topics, (cancelled,));
}

pub(crate) fn emit_timelock_scheduled(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    eta: u64,
    proposer: Address,
) {
    let topics = (symbol_short!("tl_sch"), proposal_id);
    env.events()
        .publish(topics, (action.clone(), eta, proposer));
}

pub(crate) fn emit_timelock_executed(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    executor: Address,
) {
    let topics = (symbol_short!("tl_exec"), proposal_id);
    env.events().publish(topics, (action.clone(), executor));
}

pub(crate) fn emit_timelock_cancelled(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    canceller: Address,
) {
    let topics = (symbol_short!("tl_cnl"), proposal_id);
    env.events().publish(topics, (action.clone(), canceller));
}

pub(crate) fn emit_timelock_config_changed(
    env: &Env,
    old_min_delay: u64,
    new_min_delay: u64,
    grace_period: u64,
) {
    let topics = (symbol_short!("tl_cfg"),);
    env.events()
        .publish(topics, (old_min_delay, new_min_delay, grace_period));
}

/// Emitted when a domain is paused (or an existing pause is refreshed/extended).
pub(crate) fn emit_pause_set(env: &Env, domain: &PauseDomain, actor: &Address, expires_at: u64) {
    let topics = (symbol_short!("pause_on"), domain.clone());
    env.events().publish(topics, (actor.clone(), expires_at));
}

/// Emitted when an admin lifts an active pause on a domain.
pub(crate) fn emit_pause_cleared(env: &Env, domain: &PauseDomain, actor: &Address) {
    let topics = (symbol_short!("pause_off"), domain.clone());
    env.events().publish(topics, (actor.clone(),));
}

pub(crate) fn emit_guardian_added(env: &Env, guardian: Address) {
    let topics = (symbol_short!("guard_add"),);
    env.events().publish(topics, (guardian,));
}

pub(crate) fn emit_guardian_removed(env: &Env, guardian: Address) {
    let topics = (symbol_short!("guard_rem"),);
    env.events().publish(topics, (guardian,));
}

pub(crate) fn emit_allowlist_added(env: &Env, asset: Address, admin: Address) {
    let topics = (symbol_short!("allow_add"),);
    env.events().publish(topics, (asset, admin));
}

pub(crate) fn emit_allowlist_removed(env: &Env, asset: Address, admin: Address) {
    let topics = (symbol_short!("allow_rem"),);
    env.events().publish(topics, (asset, admin));
}

pub(crate) fn emit_deprecated_call(env: &Env, deprecated: &str, replacement: &str) {
    let topics = (symbol_short!("dep_path"),);
    env.events().publish(topics, (deprecated, replacement));
}

pub(crate) fn emit_talos_paused(env: &Env, talos_id: u32, controller: Address) {
    let topics = (symbol_short!("tls_paus"), talos_id);
    env.events().publish(topics, controller);
}

pub(crate) fn emit_talos_resumed(env: &Env, talos_id: u32, controller: Address) {
    let topics = (symbol_short!("tls_resu"), talos_id);
    env.events().publish(topics, controller);
}
