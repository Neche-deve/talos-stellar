//! Storage schema migration logic for TalosRegistry.
//!
//! Each numbered migration is a pure function that runs exactly once, guarded
//! by `storage_migration::begin_migration` / `complete_migration`. The
//! entry-points (`run_migrations`, `rollback_schema`, `schema_version`,
//! `migration_history_len`, `migration_record_at`) live in `lib.rs`; the
//! actual per-version data-transformation work is here.

use soroban_sdk::Env;
use storage_migration;

use crate::{DataKey, TimelockConfig, DEFAULT_GRACE_PERIOD, SCHEMA_GENESIS, SCHEMA_TIMELOCK_DEFAULTS};

/// Apply all pending migrations in order, returning the new schema version.
///
/// Idempotent: calling when already at the latest version is a no-op.
///
/// # Panics
/// - `"Migration out of order or already in progress"` — guard violation
///   (should never happen in normal usage).
pub(crate) fn run_migrations_inner(e: &Env) -> u32 {
    let mut current = storage_migration::schema_version(e).unwrap_or(SCHEMA_GENESIS);

    // v1 → v2: backfill an explicit TimelockConfig default for instances
    // that were deployed before `set_timelock_config` was introduced, so
    // future reads do not depend on the `unwrap_or` fallback.
    if current == SCHEMA_GENESIS {
        storage_migration::begin_migration(e, current, SCHEMA_GENESIS, SCHEMA_TIMELOCK_DEFAULTS)
            .unwrap_or_else(|_| panic!("Migration out of order or already in progress"));

        if !e.storage().persistent().has(&DataKey::TimelockConfig) {
            e.storage().persistent().set(
                &DataKey::TimelockConfig,
                &TimelockConfig {
                    min_delay: 0,
                    grace_period: DEFAULT_GRACE_PERIOD,
                },
            );
        }

        storage_migration::complete_migration(e, SCHEMA_GENESIS, SCHEMA_TIMELOCK_DEFAULTS);
        current = SCHEMA_TIMELOCK_DEFAULTS;
    }

    current
}
