//! TalosRegistry — Soroban smart contract for Talos Protocol.
//!
//! Handles:
//! - Talos creation (with Pulse token metadata)
//! - Protocol fee collection (3% launchpad fee)
//! - Talos metadata storage and retrieval
//! - Patron registration with minimum Pulse holding validation
//!
//! ## Module layout
//!
//! | Module           | Responsibility                                        |
//! |------------------|-------------------------------------------------------|
//! | `events`         | All event emitter free functions                      |
//! | `timelock`       | Timelock config/schedule/execute/cancel helpers       |
//! | `pause`          | Emergency pause and guardian management helpers       |
//! | `ttl`            | Soroban storage TTL sweep helpers                     |
//! | `migrations`     | Storage schema migration logic                        |
//! | `allowlist`      | Asset allowlist management                            |
//! | `tests`          | Inline test suite (non-WASM only)                     |

#![no_std]

pub mod allowlist;
pub(crate) mod events;
pub(crate) mod migrations;
pub(crate) mod pause;
pub(crate) mod timelock;
pub(crate) mod ttl;

#[cfg(all(test, not(target_arch = "wasm32")))]
pub mod registry_schema_fixtures;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod registry_schema_tests;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
extern crate std;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, BytesN, Env,
    String, Symbol, Vec,
};
use storage_migration;
use ttl_manager;
use pause_control;
use admin_controls;

// Re-export shared types from admin_controls so the contract ABI is stable.
// Both `TimelockConfig` and `ProposalStatus` are identical in both contracts;
// we now use the single canonical definition from admin_controls.
pub use admin_controls::{ProposalStatus, TimelockConfig};

use events::{
    emit_admin_accepted, emit_admin_cancelled, emit_admin_proposed, emit_allowlist_added,
    emit_allowlist_removed, emit_deprecated_call, emit_patron_updated, emit_protocol_fee_changed,
    emit_talos_created, emit_talos_paused, emit_talos_resumed,
};

// ── Event Schema Version ────────────────────────────────────────────

/// Major event-schema version supported by this contract.
const SUPPORTED_MAJOR: u32 = 1;

/// Typed event-schema version returned to off-chain indexers.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSchemaVersion {
    pub major: u32,
    pub minor: u32,
}

/// Canonical event-schema version for this contract.
pub const EVENT_SCHEMA_VERSION: EventSchemaVersion = EventSchemaVersion {
    major: SUPPORTED_MAJOR,
    minor: 0,
};

// ── Data Types ──────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InvalidPatronShares = 1,
}

#[contracttype]
#[derive(Clone)]
pub struct Patron {
    pub creator_share: u32,
    pub investor_share: u32,
    pub treasury_share: u32,
    pub creator_addr: Address,
    pub investor_addr: Address,
    pub treasury_addr: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct Kernel {
    pub approval_threshold: i128,
    pub gtm_budget: i128,
    pub min_patron_pulse: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct Pulse {
    pub total_supply: i128,
    pub price_usd_cents: i128,
    pub token_symbol: String,
}

#[contracttype]
#[derive(Clone)]
pub struct Talos {
    pub id: u32,
    pub name: String,
    pub category: String,
    pub description: String,
    pub creator: Address,
    pub patron: Patron,
    pub kernel: Kernel,
    pub pulse: Pulse,
    pub created_at: u64,
    pub active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminAction {
    SetProtocolFee(u32),
    ProposeAdmin(Address),
}

/// Registry-specific timelock proposal. Uses the canonical [`ProposalStatus`]
/// from `admin_controls` and the registry-local [`AdminAction`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockProposal {
    pub id: u64,
    pub action: AdminAction,
    pub eta: u64,
    pub status: ProposalStatus,
    pub scheduled_at: u64,
    pub scheduled_by: Address,
}

/// A narrowly-scoped write path that can be independently paused.
///
/// Reads are never gated by any pause domain — `get_talos`, `is_active`,
/// `next_talos_id`, etc. always reflect current state regardless of pause
/// status. Administrative entry-points (`set_protocol_fee`, admin transfer,
/// timelock management) are intentionally **not** pausable: they already
/// carry their own admin-auth (and optional timelock) protection, and
/// making them pausable would risk an admin permanently locking itself out.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PauseDomain {
    TalosCreation,
    PatronUpdates,
    KernelUpdates,
    PulseUpdates,
    Deactivation,
}

/// Persisted record of an active pause on a single [`PauseDomain`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused_by: Address,
    pub paused_at: u64,
    /// Unix timestamp after which the pause automatically lifts.
    /// `0` means indefinite — only settable by the admin, never by a guardian.
    pub expires_at: u64,
}

/// Computed, read-friendly view of a domain's pause status.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseInfo {
    pub active: bool,
    pub paused_by: Option<Address>,
    pub paused_at: Option<u64>,
    /// `None` when not paused, or when paused indefinitely by the admin.
    pub expires_at: Option<u64>,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    NextTalosId,
    Talos(u32),
    CreatorOf(u32),
    ProtocolWallet,
    ProtocolFeeBps,
    /// Holds the address nominated by the current admin but not yet accepted.
    /// Presence of this key means a transfer is in progress; absence means none.
    PendingAdmin,
    TimelockConfig,
    TimelockProposal(u64),
    NextTimelockId,
    LastTouched(u32),
    Guardians,
    PauseState(PauseDomain),
    PauseController(u32),
    ResumeController(u32),
}

// ── Helpers ─────────────────────────────────────────────────────────

fn validate_patron_shares(env: &Env, patron: &Patron) {
    let sum = patron
        .creator_share
        .checked_add(patron.investor_share)
        .and_then(|s| s.checked_add(patron.treasury_share));
    match sum {
        Some(100) => {}
        _ => panic_with_error!(env, ContractError::InvalidPatronShares),
    }
}

/// Validate a Pulse `token_symbol`: it is a required identifier, so it must
/// be non-empty, and it is byte-bounded by [`MAX_TOKEN_SYMBOL_BYTES`].
fn validate_token_symbol(token_symbol: &String) {
    if token_symbol.len() == 0 {
        panic!("Token symbol cannot be empty");
    }
    if token_symbol.len() > MAX_TOKEN_SYMBOL_BYTES {
        panic!("Token symbol exceeds maximum byte length");
    }
}

/// Enforce byte limits on caller-supplied Talos metadata before it is
/// persisted.
fn validate_talos_metadata(name: &String, category: &String, description: &String, pulse: &Pulse) {
    if name.len() == 0 {
        panic!("Name cannot be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        panic!("Name exceeds maximum byte length");
    }
    if category.len() > MAX_CATEGORY_BYTES {
        panic!("Category exceeds maximum byte length");
    }
    if description.len() > MAX_DESCRIPTION_BYTES {
        panic!("Description exceeds maximum byte length");
    }
    validate_token_symbol(&pulse.token_symbol);
}

// ── Constants ───────────────────────────────────────────────────────

const PROTOCOL_FEE_BPS: u32 = 300; // 3%
const MAX_PROTOCOL_FEE_BPS: u32 = 10_000; // 100%

// Timelock and pause constants now come from admin_controls for deduplication.
pub(crate) use admin_controls::{
    DEFAULT_GRACE_PERIOD, MAX_ADMIN_PAUSE_SECS, MAX_GUARDIAN_PAUSE_SECS, MAX_GUARDIANS,
    MAX_MIN_DELAY,
};

// ── Metadata byte limits ────────────────────────────────────────────

/// Maximum byte length of `Talos.name`.
const MAX_NAME_BYTES: u32 = 64;
/// Maximum byte length of `Talos.category`.
const MAX_CATEGORY_BYTES: u32 = 32;
/// Maximum byte length of `Talos.description`.
const MAX_DESCRIPTION_BYTES: u32 = 512;
/// Maximum byte length of `Pulse.token_symbol`.
const MAX_TOKEN_SYMBOL_BYTES: u32 = 12;

// ── Storage schema migrations (see `storage_migration` crate) ────────

/// Pre-migration-system baseline.
pub(crate) const SCHEMA_GENESIS: u32 = 1;
/// v1 -> v2: persist an explicit `TimelockConfig` default.
pub(crate) const SCHEMA_TIMELOCK_DEFAULTS: u32 = 2;
/// `rollback_schema` may only move the version pointer back this many steps.
const MAX_ROLLBACK_DEPTH: u32 = 1;

/// Compile-time interface version of TalosRegistry.
pub const CONTRACT_VERSION: (u32, u32, u32) = (1, 4, 0);

/// Stable 32-byte interface identifier for TalosRegistry v1.
pub const INTERFACE_ID: [u8; 32] = [
    0x54, 0x61, 0x6C, 0x6F, 0x73, 0x52, 0x65, 0x67, // "TalosReg"
    0x69, 0x73, 0x74, 0x72, 0x79, 0x00, 0x00, 0x00, // "istry" + zero pads
    0x00, 0x00, 0x00, 0x01, // major = 1
    0x00, 0x00, 0x00, 0x03, // minor = 3
    0x00, 0x00, 0x00, 0x00, // patch = 0
    0x00, 0x00, 0x00, 0x00,
];

pub const INTERFACE_NAMESPACE: &str = "TalosRegistry";

pub fn features_list() -> &'static [&'static str] {
    &[
        "create_talos",
        "talos_lifecycle",
        "admin_transfer",
        "timelock_admin",
        "protocol_fee",
        "interface_query",
        "fees_collector",
    ]
}

pub const DEPRECATED_DIRECT_ADMIN: &[(&str, &str)] = &[
    (
        "set_protocol_fee (direct, pre-timelock)",
        "schedule_action(SetProtocolFee, ..) + execute_action",
    ),
    (
        "propose_admin (direct, pre-timelock)",
        "schedule_action(ProposeAdmin, ..) + execute_action",
    ),
];

pub fn version_supports(actual: (u32, u32, u32), required: (u32, u32, u32)) -> bool {
    if actual.0 != required.0 {
        return false;
    }
    if actual.1 > required.1 {
        return true;
    }
    if actual.1 < required.1 {
        return false;
    }
    actual.2 >= required.2
}

// ── Pause Domains ───────────────────────────────────────────────────

/// Pause domain for Talos creation (create_talos).
pub const PAUSE_TALOS_CREATION: u32 = 1;
/// Pause domain for Talos metadata updates (update_patron, update_kernel, update_pulse).
pub const PAUSE_TALOS_UPDATE: u32 = 2;
/// Pause domain for Talos deactivation.
pub const PAUSE_TALOS_DEACTIVATION: u32 = 3;
/// Pause domain for Talos activation.
pub const PAUSE_TALOS_ACTIVATION: u32 = 5;
/// Pause domain for protocol configuration (fees, admin, timelock).
pub const PAUSE_PROTOCOL_CONFIG: u32 = 4;

// ── Contract ────────────────────────────────────────────────────────

#[contract]
pub struct TalosRegistry;

#[contractimpl]
impl TalosRegistry {
    /// Create a new Talos on-chain.
    pub fn create_talos(
        e: Env,
        name: String,
        category: String,
        description: String,
        patron: Patron,
        kernel: Kernel,
        pulse: Pulse,
        protocol_wallet: Address,
    ) -> u32 {
        pause_control::check_not_paused(&e, PAUSE_TALOS_CREATION);

        patron.creator_addr.require_auth();

        validate_patron_shares(&e, &patron);
        validate_talos_metadata(&name, &category, &description, &pulse);

        if let Some(configured_wallet) = e
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::ProtocolWallet)
        {
            if configured_wallet != protocol_wallet {
                panic!("Protocol wallet mismatch");
            }
        }

        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);

        let talos = Talos {
            id: next_id,
            name: name.clone(),
            category,
            description,
            creator: patron.creator_addr.clone(),
            patron,
            kernel,
            pulse,
            created_at: e.ledger().timestamp(),
            active: true,
        };

        e.storage()
            .persistent()
            .set(&DataKey::Talos(next_id), &talos);
        e.storage()
            .persistent()
            .set(&DataKey::CreatorOf(next_id), &talos.creator);
        e.storage()
            .persistent()
            .set(&DataKey::NextTalosId, &(next_id + 1));

        emit_talos_created(&e, next_id, talos.creator.clone(), name, talos.category.clone());

        next_id
    }

    /// Get Talos by ID.
    pub fn get_talos(e: Env, talos_id: u32) -> Option<Talos> {
        e.storage().persistent().get(&DataKey::Talos(talos_id))
    }

    /// Get the creator address of a Talos.
    pub fn creator_of(e: Env, talos_id: u32) -> Option<Address> {
        e.storage().persistent().get(&DataKey::CreatorOf(talos_id))
    }

    /// Check if a Talos is active.
    pub fn is_active(e: Env, talos_id: u32) -> bool {
        e.storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .map(|t: Talos| t.active)
            .unwrap_or(false)
    }

    /// Get the next Talos ID (for counting).
    pub fn next_talos_id(e: Env) -> u32 {
        e.storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1)
    }

    /// Update patron shares for a Talos.
    pub fn update_patron(e: Env, talos_id: u32, patron: Patron) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_UPDATE);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        validate_patron_shares(&e, &patron);

        talos.patron = patron.clone();
        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);

        emit_patron_updated(&e, talos_id, &patron);
    }

    /// Update kernel policy for a Talos.
    pub fn update_kernel(e: Env, talos_id: u32, kernel: Kernel) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_UPDATE);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        talos.kernel = kernel;
        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);
    }

    /// Update pulse token config for a Talos.
    pub fn update_pulse(e: Env, talos_id: u32, pulse: Pulse) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_UPDATE);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        validate_token_symbol(&pulse.token_symbol);
        talos.pulse = pulse;
        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);
    }

    /// Deactivate a Talos.
    pub fn deactivate_talos(e: Env, talos_id: u32, controller: Address) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_DEACTIVATION);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        controller.require_auth();

        let is_creator = talos.creator == controller;
        let pause_controller: Option<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::PauseController(talos_id));
        let is_pause_controller = pause_controller.as_ref() == Some(&controller);

        if !is_creator && !is_pause_controller {
            panic!("Unauthorized: only creator or pause controller can deactivate");
        }

        talos.active = false;
        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);

        emit_talos_paused(&e, talos_id, controller);
    }

    /// Activate a Talos.
    pub fn activate_talos(e: Env, talos_id: u32, controller: Address) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_ACTIVATION);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        controller.require_auth();

        let is_creator = talos.creator == controller;
        let resume_controller: Option<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::ResumeController(talos_id));
        let is_resume_controller = resume_controller.as_ref() == Some(&controller);

        if !is_creator && !is_resume_controller {
            panic!("Unauthorized: only creator or resume controller can activate");
        }

        talos.active = true;
        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);

        emit_talos_resumed(&e, talos_id, controller);
    }

    /// Set the pause controller for a Talos. Only the creator can set this.
    pub fn set_pause_controller(e: Env, talos_id: u32, pause_controller: Address) {
        let talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        e.storage()
            .persistent()
            .set(&DataKey::PauseController(talos_id), &pause_controller);
    }

    /// Set the resume controller for a Talos. Only the creator can set this.
    pub fn set_resume_controller(e: Env, talos_id: u32, resume_controller: Address) {
        let talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        e.storage()
            .persistent()
            .set(&DataKey::ResumeController(talos_id), &resume_controller);
    }

    /// Get the pause controller for a Talos.
    pub fn get_pause_controller(e: Env, talos_id: u32) -> Option<Address> {
        e.storage()
            .persistent()
            .get(&DataKey::PauseController(talos_id))
    }

    /// Get the resume controller for a Talos.
    pub fn get_resume_controller(e: Env, talos_id: u32) -> Option<Address> {
        e.storage()
            .persistent()
            .get(&DataKey::ResumeController(talos_id))
    }

    /// Initialize the contract with protocol wallet and fee.
    pub fn initialize(e: Env, protocol_wallet: Address) {
        if e.storage()
            .persistent()
            .get::<_, Address>(&DataKey::ProtocolWallet)
            .is_some()
        {
            panic!("Already initialized");
        }

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolWallet, &protocol_wallet);
        e.storage()
            .persistent()
            .set(&DataKey::ProtocolFeeBps, &PROTOCOL_FEE_BPS);
        e.storage().persistent().set(&DataKey::NextTalosId, &1u32);
        storage_migration::initialize_schema(&e, SCHEMA_GENESIS);
    }

    /// Get the protocol wallet address.
    pub fn protocol_wallet(e: Env) -> Option<Address> {
        e.storage().persistent().get(&DataKey::ProtocolWallet)
    }

    /// Get the protocol fee in basis points.
    pub fn protocol_fee_bps(e: Env) -> Option<u32> {
        e.storage().persistent().get(&DataKey::ProtocolFeeBps)
    }

    fn set_protocol_fee_internal(e: &Env, fee_bps: u32) {
        if fee_bps > MAX_PROTOCOL_FEE_BPS {
            panic!("Protocol fee cannot exceed 100%");
        }

        let old_bps: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(PROTOCOL_FEE_BPS);

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);

        emit_protocol_fee_changed(e, old_bps, fee_bps);
    }

    fn propose_admin_internal(e: &Env, current: Address, new_admin: Address) {
        e.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);

        emit_admin_proposed(e, current, new_admin);
    }

    /// Update the protocol fee in basis points.
    ///
    /// **Deprecation:** When the timelock is enabled, this direct entry-point
    /// is rejected. Use `schedule_action(SetProtocolFee(..))` + `execute_action`.
    pub fn set_protocol_fee(e: Env, fee_bps: u32) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        if fee_bps > MAX_PROTOCOL_FEE_BPS {
            panic!("Protocol fee cannot exceed 100%");
        }

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        admin.require_auth();

        let config = Self::get_timelock_config(e.clone());
        if config.min_delay > 0 {
            emit_deprecated_call(
                &e,
                DEPRECATED_DIRECT_ADMIN[0].0,
                DEPRECATED_DIRECT_ADMIN[0].1,
            );
            panic!("Timelock enabled: action must be scheduled");
        }

        Self::set_protocol_fee_internal(&e, fee_bps);
    }

    // ── Two-step admin transfer ──────────────────────────────────────

    /// Nominate `new_admin` as the next protocol wallet (admin).
    ///
    /// **Deprecation:** When the timelock is enabled, this direct entry-point
    /// is rejected. Use `schedule_action(ProposeAdmin(..))` + `execute_action`.
    pub fn propose_admin(e: Env, new_admin: Address) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let current: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        current.require_auth();

        let config = Self::get_timelock_config(e.clone());
        if config.min_delay > 0 {
            emit_deprecated_call(
                &e,
                DEPRECATED_DIRECT_ADMIN[1].0,
                DEPRECATED_DIRECT_ADMIN[1].1,
            );
            panic!("Timelock enabled: action must be scheduled");
        }

        Self::propose_admin_internal(&e, current, new_admin);
    }

    // ── Timelock Administration ─────────────────────────────────────

    /// Configure timelock parameter settings (`min_delay` and `grace_period`).
    pub fn set_timelock_config(e: Env, min_delay: u64, grace_period: u64) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        timelock::set_timelock_config_internal(&e, min_delay, grace_period);
    }

    /// Retrieve active timelock configuration.
    pub fn get_timelock_config(e: Env) -> TimelockConfig {
        timelock::get_timelock_config(&e)
    }

    /// Schedule a high-impact administrative action for future execution.
    pub fn schedule_action(e: Env, action: AdminAction, delay: u64) -> u64 {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        timelock::schedule_action_internal(&e, admin, action, delay)
    }

    /// Execute a scheduled administrative action after its timelock ETA has matured.
    pub fn execute_action(e: Env, proposal_id: u64) {
        let proposal = timelock::validate_executable_proposal(&e, proposal_id);

        match &proposal.action {
            AdminAction::SetProtocolFee(fee_bps) => {
                Self::set_protocol_fee_internal(&e, *fee_bps);
            }
            AdminAction::ProposeAdmin(new_admin) => {
                let current: Address = e
                    .storage()
                    .persistent()
                    .get(&DataKey::ProtocolWallet)
                    .expect("Contract not initialized");
                Self::propose_admin_internal(&e, current, new_admin.clone());
            }
        }

        timelock::mark_executed(&e, proposal);
    }

    /// Cancel a scheduled administrative proposal before it is executed.
    pub fn cancel_action(e: Env, proposal_id: u64) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        timelock::cancel_action_internal(&e, admin, proposal_id);
    }

    /// Retrieve a timelock proposal by ID.
    pub fn get_timelock_proposal(e: Env, proposal_id: u64) -> Option<TimelockProposal> {
        e.storage()
            .persistent()
            .get(&DataKey::TimelockProposal(proposal_id))
    }

    /// Complete the pending admin transfer.
    pub fn accept_admin(e: Env) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let pending: Address = e
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .expect("No pending admin transfer");

        pending.require_auth();

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolWallet, &pending);
        e.storage().persistent().remove(&DataKey::PendingAdmin);

        emit_admin_accepted(&e, pending);
    }

    /// Cancel an in-progress admin transfer.
    pub fn cancel_admin_transfer(e: Env) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let current: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        current.require_auth();

        let pending: Address = e
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .expect("No pending admin transfer");

        e.storage().persistent().remove(&DataKey::PendingAdmin);

        emit_admin_cancelled(&e, pending);
    }

    /// Return the pending admin address, if any.
    pub fn pending_admin(e: Env) -> Option<Address> {
        e.storage().persistent().get(&DataKey::PendingAdmin)
    }

    // ── Emergency Pause Controls ──────────────────────────────────────

    /// Pause a single write-path domain.
    ///
    /// `caller` must be the admin or a registered guardian and must sign.
    /// Duration rules: admin may use 0 (indefinite) or up to `MAX_ADMIN_PAUSE_SECS`;
    /// guardians must use 1..=`MAX_GUARDIAN_PAUSE_SECS`.
    pub fn pause(e: Env, caller: Address, domain: PauseDomain, duration: u64) {
        caller.require_auth();

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        let is_admin_caller = caller == admin;
        let guardians: Vec<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::Guardians)
            .unwrap_or(Vec::new(&e));

        if !is_admin_caller && !admin_controls::is_guardian(&guardians, &caller) {
            panic!("Caller is not admin or guardian");
        }

        pause::apply_pause(&e, &caller, &admin, is_admin_caller, domain, duration);
    }

    /// Lift an active pause on `domain`. Idempotent.
    pub fn unpause(e: Env, domain: PauseDomain) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        pause::clear_pause(&e, &admin, domain);
    }

    /// Returns `true` if `domain` is currently paused.
    pub fn is_paused(e: Env, domain: PauseDomain) -> bool {
        pause::is_domain_paused(&e, &domain)
    }

    /// Full observability view of a domain's pause status.
    pub fn pause_info(e: Env, domain: PauseDomain) -> PauseInfo {
        pause::pause_info(&e, domain)
    }

    /// Register a guardian address. Idempotent.
    pub fn add_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let mut guardians: Vec<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::Guardians)
            .unwrap_or(Vec::new(&e));

        admin_controls::add_guardian(&e, &mut guardians, guardian);
        e.storage().persistent().set(&DataKey::Guardians, &guardians);
    }

    /// Revoke a guardian's pause authority. Idempotent.
    pub fn remove_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let mut guardians: Vec<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::Guardians)
            .unwrap_or(Vec::new(&e));

        admin_controls::remove_guardian(&e, &mut guardians, guardian);
        e.storage().persistent().set(&DataKey::Guardians, &guardians);
    }

    /// Check whether `addr` currently holds guardian pause authority.
    pub fn is_guardian(e: Env, addr: Address) -> bool {
        let guardians: Vec<Address> = e
            .storage()
            .persistent()
            .get(&DataKey::Guardians)
            .unwrap_or(Vec::new(&e));
        admin_controls::is_guardian(&guardians, &addr)
    }

    /// List all currently registered guardians.
    pub fn list_guardians(e: Env) -> Vec<Address> {
        e.storage()
            .persistent()
            .get(&DataKey::Guardians)
            .unwrap_or(Vec::new(&e))
    }

    /// Return the contract's interface version as `(major, minor, patch)`.
    pub fn version(_e: Env) -> (u32, u32, u32) {
        CONTRACT_VERSION
    }

    /// Return the contract's 32-byte stable interface identifier.
    pub fn interface_id(e: Env) -> BytesN<32> {
        BytesN::from_array(&e, &INTERFACE_ID)
    }

    /// Return `true` when the deployed contract semver is at least the
    /// `major.minor.patch` requested by the caller.
    pub fn supports_version(e: Env, major: u32, minor: u32, patch: u32) -> bool {
        let _ = e;
        version_supports(CONTRACT_VERSION, (major, minor, patch))
    }

    /// Return the list of capability symbols supported by this contract.
    pub fn interface_features(e: Env) -> Vec<Symbol> {
        let caps = features_list();
        let mut out = Vec::new(&e);
        for cap in caps {
            out.push_back(Symbol::new(&e, cap));
        }
        out
    }

    /// Reported count of deprecated direct-admin paths.
    pub fn deprecated_entry_count(_e: Env) -> u32 {
        DEPRECATED_DIRECT_ADMIN.len() as u32
    }

    /// Calculate the protocol fee for an amount using the configured fee bps.
    pub fn calculate_protocol_fee(e: Env, amount: i128) -> i128 {
        if amount < 0 {
            panic!("Amount must be non-negative");
        }

        let fee_bps: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(PROTOCOL_FEE_BPS);

        amount * fee_bps as i128 / MAX_PROTOCOL_FEE_BPS as i128
    }

    /// Return the event-schema version expected by off-chain indexers.
    pub fn event_schema_version(_e: Env) -> EventSchemaVersion {
        if EVENT_SCHEMA_VERSION.major != SUPPORTED_MAJOR {
            panic!("Unsupported event schema major version");
        }
        EVENT_SCHEMA_VERSION
    }

    // ── Allowlist Management ──────────────────────────────────────

    /// Check whether an asset is allowlisted for value-transfer operations.
    pub fn is_asset_allowed(e: Env, asset: Address) -> bool {
        crate::allowlist::AssetAllowlist::is_allowed(&e, &asset)
    }

    /// Add an asset to the allowlist.
    pub fn add_asset_to_allowlist(e: Env, asset: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();
        crate::allowlist::AssetAllowlist::add(&e, &admin, &asset);
        emit_allowlist_added(&e, asset, admin);
    }

    /// Remove an asset from the allowlist.
    pub fn remove_asset_from_allowlist(e: Env, asset: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();
        crate::allowlist::AssetAllowlist::remove(&e, &admin, &asset);
        emit_allowlist_removed(&e, asset, admin);
    }

    // ── Storage TTL Management ───────────────────────────────────

    /// Touch a Talos record to reset its Soroban storage TTL.
    pub fn touch_talos(e: Env, talos_id: u32) -> bool {
        ttl::touch_talos_inner(&e, talos_id)
    }

    /// Batch-touch all Talos records up to a limit (paginated).
    pub fn touch_batch(e: Env, start_id: u32, limit: u32) -> (u32, u32) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);
        let range = start_id..next_id.min(start_id.saturating_add(limit));
        ttl::sweep_talos_ttl(&e, &ttl_manager::TtlBounds::renewal(), range)
    }

    /// Batched TTL extension with explicit age and work bounds.
    pub fn extend_ttl_batch(
        e: Env,
        start_id: u32,
        limit: u32,
        min_age: u32,
        max_age: u32,
    ) -> (u32, u32) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);
        let bounds =
            ttl_manager::TtlBounds::new(min_age, max_age, ttl_manager::DEFAULT_MAX_BATCH_KEYS);
        let range = ttl_manager::bounded_range(start_id, limit, &bounds, next_id)
            .unwrap_or_else(|err| panic!("{}", err.message()));
        ttl::sweep_talos_ttl(&e, &bounds, range)
    }

    /// Query storage health by comparing entry ages against thresholds.
    pub fn get_storage_health(e: Env) -> (u32, u32, u32, u32, u32) {
        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);
        ttl::storage_health(&e, next_id)
    }

    // ── Storage Schema Migrations ─────────────────────────────────

    /// Current on-chain storage schema version.
    pub fn schema_version(e: Env) -> u32 {
        storage_migration::schema_version(&e).unwrap_or(SCHEMA_GENESIS)
    }

    /// Apply any pending storage migrations.
    pub fn run_migrations(e: Env) -> u32 {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        migrations::run_migrations_inner(&e)
    }

    /// Roll the schema version pointer back to `target_version`.
    pub fn rollback_schema(e: Env, target_version: u32) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let current = storage_migration::schema_version(&e).unwrap_or(SCHEMA_GENESIS);
        storage_migration::rollback(&e, current, target_version, MAX_ROLLBACK_DEPTH)
            .unwrap_or_else(|_| panic!("Rollback rejected: out of range or migration in progress"));
    }

    /// Number of entries in the migration/rollback history log.
    pub fn migration_history_len(e: Env) -> u32 {
        storage_migration::migration_history_len(&e)
    }

    /// Fetch a single migration/rollback history entry, oldest first.
    pub fn migration_record_at(e: Env, index: u32) -> Option<storage_migration::MigrationRecord> {
        storage_migration::migration_record_at(&e, index)
    }
}
