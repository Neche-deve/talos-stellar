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
//! | `timelock`       | Timelock config, schedule/execute/cancel helpers      |
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProposalStatus {
    Scheduled,
    Executed,
    Cancelled,
}

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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockConfig {
    pub min_delay: u64,
    pub grace_period: u64,
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
/// persisted. `name` and `Pulse.token_symbol` are required identifiers and
/// must be non-empty; `category` and `description` are descriptive and may
/// be empty, but are still byte-bounded. Panics are explicit and
/// privacy-safe — no caller or value data is included.
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
pub(crate) const DEFAULT_GRACE_PERIOD: u64 = 604_800; // 7 days in seconds
pub(crate) const MAX_MIN_DELAY: u64 = 2_592_000; // 30 days in seconds
pub(crate) const MAX_GUARDIAN_PAUSE_SECS: u64 = 604_800; // 7 days — bounds guardian blast radius
pub(crate) const MAX_ADMIN_PAUSE_SECS: u64 = 2_592_000; // 30 days; 0 (indefinite) is also allowed for admin
pub(crate) const MAX_GUARDIANS: u32 = 10; // bounds unbounded storage growth

// ── Metadata byte limits ────────────────────────────────────────────
//
// Talos metadata is caller-supplied and persisted verbatim, so it is
// bounded in bytes (not characters) to cap per-entry storage rent and to
// keep off-chain indexers from having to accept unbounded payloads. The
// limits are enforced on every write path that carries the field.

/// Maximum byte length of `Talos.name`.
const MAX_NAME_BYTES: u32 = 64;
/// Maximum byte length of `Talos.category`.
const MAX_CATEGORY_BYTES: u32 = 32;
/// Maximum byte length of `Talos.description`.
const MAX_DESCRIPTION_BYTES: u32 = 512;
/// Maximum byte length of `Pulse.token_symbol`.
const MAX_TOKEN_SYMBOL_BYTES: u32 = 12;

// ── Storage schema migrations (see `storage_migration` crate) ────────

/// Pre-migration-system baseline. Every TalosRegistry instance, including
/// ones deployed before this framework existed, is treated as starting here.
pub(crate) const SCHEMA_GENESIS: u32 = 1;
/// v1 -> v2: persist an explicit `TimelockConfig` default so future reads
/// no longer depend on `get_timelock_config`'s implicit `unwrap_or` fallback.
pub(crate) const SCHEMA_TIMELOCK_DEFAULTS: u32 = 2;
/// `rollback_schema` may only move the version pointer back this many steps.
const MAX_ROLLBACK_DEPTH: u32 = 1;

/// Compile-time interface version of TalosRegistry.
///
/// Format: `(major, minor, patch)` following Semantic Versioning.
///
/// Bump rules:
/// - **major** — incompatible ABI change (removed/renamed entry-points, changed argument types)
/// - **minor** — backwards-compatible new entry-point or return-field added
/// - **patch** — bug-fix with no observable ABI change
///
/// This constant is embedded in the WASM binary at compile time and is
/// therefore immutable once deployed; it cannot be altered by any admin
/// call, storage write, or cross-contract invocation.
pub const CONTRACT_VERSION: (u32, u32, u32) = (1, 4, 0);

/// Stable 32-byte interface identifier for TalosRegistry v1.
///
/// Derived deterministically from the contract namespace
/// (`"TalosRegistry"`) and `CONTRACT_VERSION` so that any client or
/// dependent contract that re-runs the algorithm reproduces the same
/// bytes (see the golden-vector tests in `mod tests`).
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
    ///
    /// # Arguments
    /// * `e` - Soroban environment
    /// * `name` - Talos name
    /// * `category` - Category (Marketing, Sales, etc.)
    /// * `description` - Description
    /// * `patron` - Patron configuration (shares + addresses)
    /// * `kernel` - Kernel policy (thresholds, budget)
    /// * `pulse` - Pulse token config (supply, price, symbol)
    /// * `protocol_wallet` - Protocol wallet to receive 3% fee
    ///
    /// Returns the new Talos ID.
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

        // Require creator authorization
        patron.creator_addr.require_auth();

        validate_patron_shares(&e, &patron);
        validate_talos_metadata(&name, &category, &description, &pulse);

        // If the registry has been initialized, ensure callers use the configured
        // protocol wallet. This keeps the create_talos ABI backwards compatible
        // while preventing a mismatched fee recipient from being supplied.
        if let Some(configured_wallet) = e
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::ProtocolWallet)
        {
            if configured_wallet != protocol_wallet {
                panic!("Protocol wallet mismatch");
            }
        }

        // Get next Talos ID
        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);

        // Create Talos struct
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

        // Store Talos
        e.storage()
            .persistent()
            .set(&DataKey::Talos(next_id), &talos);

        // Track creator
        e.storage()
            .persistent()
            .set(&DataKey::CreatorOf(next_id), &talos.creator);

        // Increment next ID
        e.storage()
            .persistent()
            .set(&DataKey::NextTalosId, &(next_id + 1));

        // Emit event
        emit_talos_created(
            &e,
            next_id,
            talos.creator.clone(),
            name,
            talos.category.clone(),
        );

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

        // Require creator authorization
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
    /// Can be called by creator or authorized pause controller.
    pub fn deactivate_talos(e: Env, talos_id: u32, controller: Address) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_DEACTIVATION);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        controller.require_auth();

        // Check if caller is creator or authorized pause controller
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
    /// Can be called by creator or authorized resume controller.
    pub fn activate_talos(e: Env, talos_id: u32, controller: Address) {
        pause_control::check_not_paused(&e, PAUSE_TALOS_ACTIVATION);

        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        controller.require_auth();

        // Check if caller is creator or authorized resume controller
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

    /// Set the pause controller for a Talos.
    /// Only the creator can set this.
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

    /// Set the resume controller for a Talos.
    /// Only the creator can set this.
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
    /// Only the configured protocol wallet may update the fee.
    ///
    /// **Deprecation:** When the timelock is enabled (`min_delay > 0`),
    /// this direct entry-point is rejected. Callers should use
    /// `schedule_action(SetProtocolFee(..), delay)` followed by
    /// `execute_action` after the timelock ETA. The rejection emits a
    /// `dep_path` event with the recommended replacement before
    /// panicking so off-chain telemetry can still see the attempted
    /// legacy call.
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
    /// The transfer is **not** immediate. The nominated address must call
    /// `accept_admin` to confirm and complete the handover. Until then the
    /// current admin retains all privileges.
    ///
    /// Calling `propose_admin` while a transfer is already pending silently
    /// replaces the old nomination with the new one (no separate cancellation
    /// step is required for the proposer). The replacement emits a fresh
    /// `adm_prp` event without emitting `adm_cnl`.
    ///
    /// **Deprecation:** When the timelock is enabled (`min_delay > 0`),
    /// this direct entry-point is rejected. Callers should schedule
    /// `ProposeAdmin(new_admin)` via `schedule_action` and execute it
    /// after the timelock ETA. The rejection emits a `dep_path` event
    /// with the recommended replacement before panicking so off-chain
    /// telemetry can still observe the attempted legacy call.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
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
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
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
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
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
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
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
    ///
    /// The nominated address (stored under `PendingAdmin`) must call this
    /// method and authorize it. On success:
    ///   1. `ProtocolWallet` is updated to the caller's address.
    ///   2. The `PendingAdmin` storage entry is removed.
    ///   3. An `adm_acc` event is emitted.
    ///
    /// # Authorization
    /// Requires the pending admin to sign the transaction.
    ///
    /// # Panics
    /// - `"No pending admin transfer"` — if `propose_admin` has not been called.
    pub fn accept_admin(e: Env) {
        pause_control::check_not_paused(&e, PAUSE_PROTOCOL_CONFIG);

        let pending: Address = e
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .expect("No pending admin transfer");

        pending.require_auth();

        // Promote pending → active admin.
        e.storage()
            .persistent()
            .set(&DataKey::ProtocolWallet, &pending);

        // Clear the pending slot.
        e.storage().persistent().remove(&DataKey::PendingAdmin);

        emit_admin_accepted(&e, pending);
    }

    /// Cancel an in-progress admin transfer.
    ///
    /// Removes the `PendingAdmin` entry and revokes the nomination without
    /// changing the current admin.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    /// - `"No pending admin transfer"` — if there is nothing to cancel.
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

    /// Pause a single write-path domain, blocking it while leaving all
    /// reads and every other domain fully functional.
    ///
    /// # Authorization
    /// `caller` must be either the current admin (`ProtocolWallet`) or a
    /// registered guardian, and must sign the transaction.
    ///
    /// # Duration semantics
    /// - Admin: `duration = 0` pauses indefinitely (only the admin can lift
    ///   it later via `unpause`). A non-zero `duration` must not exceed
    ///   [`MAX_ADMIN_PAUSE_SECS`].
    /// - Guardian: `duration` must be non-zero and at most
    ///   [`MAX_GUARDIAN_PAUSE_SECS`] — guardians can never impose an
    ///   indefinite pause, bounding the damage a compromised guardian key
    ///   can do.
    ///
    /// Calling `pause` again on an already-paused domain refreshes
    /// (overwrites) the existing record — this is intentionally idempotent
    /// so a retried or duplicated transaction is safe. The one exception:
    /// a guardian may never overwrite a pause that the admin established
    /// (see `Panics`), preventing a guardian from weakening an admin lock.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    /// - `"Caller is not admin or guardian"` — unauthorized caller.
    /// - `"Domain locked by admin; guardians cannot modify"` — a guardian
    ///   attempted to overwrite an admin-established pause.
    /// - `"Duration exceeds admin maximum"` / `"Guardian pause duration out of bounds"`.
    pub fn pause(e: Env, caller: Address, domain: PauseDomain, duration: u64) {
        caller.require_auth();

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        let is_admin_caller = caller == admin;
        if !is_admin_caller && !pause::is_guardian_internal(&e, &caller) {
            panic!("Caller is not admin or guardian");
        }

        pause::apply_pause(&e, &caller, &admin, is_admin_caller, domain, duration);
    }

    /// Lift an active pause on `domain`.
    ///
    /// Idempotent: if the domain has no active pause record (never paused,
    /// already lifted, or already expired), this is a deterministic no-op —
    /// it does not panic, so duplicate or retried `unpause` calls are safe.
    ///
    /// # Authorization
    /// Only the current admin (`ProtocolWallet`) may unpause, regardless of
    /// whether the pause was originally set by the admin or a guardian.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    pub fn unpause(e: Env, domain: PauseDomain) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        pause::clear_pause(&e, &admin, domain);
    }

    /// Returns `true` if `domain` is currently paused (and its pause has not
    /// yet expired). Never requires authorization — this is a read.
    pub fn is_paused(e: Env, domain: PauseDomain) -> bool {
        pause::is_domain_paused(&e, &domain)
    }

    /// Full observability view of a domain's pause status: who paused it,
    /// when, and when it expires (or `None` for indefinite/unpaused).
    pub fn pause_info(e: Env, domain: PauseDomain) -> PauseInfo {
        pause::pause_info(&e, domain)
    }

    /// Register a guardian address authorized to trigger (but never lift)
    /// bounded-duration pauses. Bounded to [`MAX_GUARDIANS`] entries.
    ///
    /// Idempotent: adding an already-registered guardian is a no-op.
    ///
    /// # Authorization
    /// Requires current admin (`ProtocolWallet`) authorization.
    pub fn add_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        pause::add_guardian_internal(&e, guardian);
    }

    /// Revoke a guardian's pause authority.
    ///
    /// Idempotent: removing an address that is not a guardian is a no-op.
    /// Does not affect any pause the guardian already set — use `unpause`
    /// (admin-only) to lift it explicitly.
    ///
    /// # Authorization
    /// Requires current admin (`ProtocolWallet`) authorization.
    pub fn remove_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        pause::remove_guardian_internal(&e, guardian);
    }

    /// Check whether `addr` currently holds guardian pause authority.
    pub fn is_guardian(e: Env, addr: Address) -> bool {
        pause::is_guardian_internal(&e, &addr)
    }

    /// List all currently registered guardians (bounded to `MAX_GUARDIANS`).
    pub fn list_guardians(e: Env) -> Vec<Address> {
        pause::get_guardians(&e)
    }

    /// Return the contract's interface version as `(major, minor, patch)`.
    ///
    /// The value is a compile-time constant baked into the WASM binary.
    /// It is **not** stored in ledger state and cannot be altered by any
    /// administrator, upgrade, or cross-contract call after deployment.
    ///
    /// Clients should call this method to verify ABI compatibility before
    /// invoking other entry-points. A change in `major` signals a breaking
    /// change; a change in `minor` adds new entry-points while remaining
    /// backwards compatible; `patch` carries bug-fixes only.
    ///
    /// # Returns
    /// `(major: u32, minor: u32, patch: u32)` — currently `(1, 4, 0)`.
    pub fn version(_e: Env) -> (u32, u32, u32) {
        CONTRACT_VERSION
    }

    /// Return the contract's 32-byte stable interface identifier.
    ///
    /// The identifier is content-derived from
    /// `(INTERFACE_NAMESPACE, CONTRACT_VERSION)` and is identical to the
    /// `INTERFACE_ID` constant embedded in the WASM binary at compile
    /// time. Cross-contract callers and SDKs use it to detect which
    /// `interface_id()` consumer is registered at a given address — they
    /// should treat a mismatch as a hard `abort()` rather than fall back,
    /// since the contract family has changed.
    ///
    /// Stable identifiers are also documented as "golden vectors" in the
    /// test suite — regenerate from the namespace + version tuple at any
    /// time to verify.
    pub fn interface_id(e: Env) -> BytesN<32> {
        BytesN::from_array(&e, &INTERFACE_ID)
    }

    /// Return `true` when the deployed contract semver is at least the
    /// `major.minor.patch` requested by the caller.
    ///
    /// Compatibility rule (see `version_supports`):
    /// - `major` must match exactly;
    /// - `minor` must be `>=` required, with `patch` checked on exact minor.
    ///
    /// Returns `false` for any incompatible combination (including
    /// illegal inputs such as `major > u32::MAX`, which silently
    /// round-trips to `0` — tests cover this).
    pub fn supports_version(e: Env, major: u32, minor: u32, patch: u32) -> bool {
        let _ = e;
        version_supports(CONTRACT_VERSION, (major, minor, patch))
    }

    /// Return the list of capability symbols supported by this contract.
    ///
    /// Capability IDs are **stable feature markers**: if a consumer is
    /// gated on a particular capability (e.g. `"timelock_admin"`), they
    /// can read this list at integration time rather than guessing from
    /// version bumps. Adding a capability requires a `minor` bump;
    /// removing or renaming requires a `major` bump.
    pub fn interface_features(e: Env) -> Vec<Symbol> {
        let caps = features_list();
        let mut out = Vec::new(&e);
        for cap in caps {
            out.push_back(Symbol::new(&e, cap));
        }
        out
    }

    /// Reported count of deprecated direct-admin paths exposed for
    /// telemetry. Each entry in the static table corresponds to exactly
    /// one entry-point that becomes a no-op after timelocks are enabled.
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
    ///
    /// This is a pure read. Indexers should reject a deployment when its
    /// major version differs from the version they support.
    pub fn event_schema_version(_e: Env) -> EventSchemaVersion {
        if EVENT_SCHEMA_VERSION.major != SUPPORTED_MAJOR {
            panic!("Unsupported event schema major version");
        }
        EVENT_SCHEMA_VERSION
    }

    // ── Allowlist Management ──────────────────────────────────────

    /// Check whether an asset is allowlisted for value-transfer operations.
    ///
    /// This is a read-only entry-point and does not require authorization.
    /// Returns `true` if the asset is currently allowlisted, `false` otherwise.
    pub fn is_asset_allowed(e: Env, asset: Address) -> bool {
        crate::allowlist::AssetAllowlist::is_allowed(&e, &asset)
    }

    /// Add an asset to the allowlist.
    ///
    /// # Authorization
    /// Requires the current protocol wallet (admin) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
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
    ///
    /// # Authorization
    /// Requires the current protocol wallet (admin) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
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
    ///
    /// Since every `set()` call on persistent storage auto-bumps the TTL,
    /// this function re-reads the Talos struct and writes it back unchanged.
    /// It only writes when the entry has not been touched recently (within
    /// the renewal threshold) to save gas.
    ///
    /// Returns `true` if a touch was performed, `false` if skipped.
    pub fn touch_talos(e: Env, talos_id: u32) -> bool {
        ttl::touch_talos_inner(&e, talos_id)
    }

    /// Batch-touch all Talos records up to a limit (paginated).
    ///
    /// The sweep is bounded by `limit` and by the renewal age window; use
    /// [`TalosRegistry::extend_ttl_batch`] to pass explicit bounds.
    ///
    /// # Authorization
    /// Requires the protocol wallet (admin) to sign.
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
    ///
    /// Visits ids `[start_id, min(next_id, start_id + limit))`, clipped to
    /// [`ttl_manager::DEFAULT_MAX_BATCH_KEYS`] ids per call, and re-writes only
    /// the records whose age in ledgers falls inside `[min_age, max_age]`.
    /// Records outside the window are reported as `skipped` in the return value
    /// and in the `ttl_batch` event; ids with no record are ignored.
    ///
    /// Returns `(touched, skipped)`.
    ///
    /// # Panics
    /// - `"Domain is paused"` — when the protocol-config domain is paused.
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    /// - `"ttl bounds: ..."` — static diagnostic for malformed bounds.
    ///
    /// # Authorization
    /// Requires the protocol wallet (admin) to sign.
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
    ///
    /// Returns `(min_age, max_age, keys_below_warn, keys_below_crit, total)`.
    /// Emits `ttl_warn` if any entry is at risk.
    pub fn get_storage_health(e: Env) -> (u32, u32, u32, u32, u32) {
        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);
        ttl::storage_health(&e, next_id)
    }

    // ── Storage Schema Migrations ─────────────────────────────────

    /// Current on-chain storage schema version. Defaults to
    /// [`SCHEMA_GENESIS`] for any instance that has never run
    /// `initialize` or `run_migrations` under this framework.
    pub fn schema_version(e: Env) -> u32 {
        storage_migration::schema_version(&e).unwrap_or(SCHEMA_GENESIS)
    }

    /// Apply any pending storage migrations, one ordered step at a time.
    /// Safe to call repeatedly: once the contract is at the latest known
    /// version this is a no-op. Returns the resulting schema version.
    ///
    /// # Authorization
    /// Requires the protocol wallet (admin) to sign.
    pub fn run_migrations(e: Env) -> u32 {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        migrations::run_migrations_inner(&e)
    }

    /// Roll the schema version pointer back to `target_version`, bounded by
    /// [`MAX_ROLLBACK_DEPTH`] steps. Does **not** undo data written by
    /// forward migrations — see `storage_migration`'s README before relying
    /// on old entry-points again after a rollback.
    ///
    /// # Authorization
    /// Requires the protocol wallet (admin) to sign.
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
