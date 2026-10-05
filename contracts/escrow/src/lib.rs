#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Env, IntoVal, String, Symbol, Val, Vec,
};

mod events;

/// Current on-chain storage schema version stamped at init.
const CURRENT_SCHEMA: u32 = 2;

/// Maximum number of `(agent, service_id)` pairs accepted by a single
/// `get_usage_batch` call. Chosen at 100 as a conservative cap: the batch
/// read iterates the input once doing one persistent read per pair, so the
/// bound keeps the loop (and the host's storage-read budget) predictable and
/// prevents a single call from triggering an unboundedly large amount of work.
/// Callers needing more pairs should page the requests.
pub const MAX_BATCH_READ: u32 = 100;

/// Hard cap on the per-agent service index length. Capped at 256 to prevent
/// unbounded storage growth: an adversary recording usage across an ever-growing
/// set of service ids would otherwise increase the `AgentServiceIndex` vector
/// indefinitely. At 256 the index write on a new service costs at most one
/// additional persistent read/write; callers that genuinely need more than 256
/// services per agent should enumerate them off-chain from event logs.
pub const MAX_AGENT_SERVICE_INDEX: u32 = 256;

/// Hard cap on the number of services `settle_all` may settle in one call.
/// Mirrors `MAX_AGENT_SERVICE_INDEX` so the index never exceeds the settle cap.
pub const MAX_SETTLE_ALL: u32 = 256;

/// Free-form metadata about a service. Stored under
/// `DataKey::ServiceMetadata(service_id)` so dashboards and clients can
/// resolve a service to a human-readable description and owner without
/// keeping a parallel registry off-chain.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceMetadata {
    pub description: String,
    pub owner: Address,
}

/// A snapshot of the current admin-handover state, returned by
/// [`Escrow::get_admin_summary`].
///
/// Combines `get_admin` and `get_pending_admin` into one read so callers
/// (dashboards, migration tooling) can check whether a handover is in
/// progress without two round trips. Both fields default to `None` — before
/// `init`, and whenever no handover is pending, respectively — never a
/// panic. The individual getters remain available and always agree with the
/// corresponding field here.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminSummary {
    pub admin: Option<Address>,
    pub pending_admin: Option<Address>,
}

/// A snapshot of all global contract configuration, returned by
/// [`Escrow::get_contract_config`].
///
/// Each field carries the same default as its dedicated getter when the
/// underlying storage slot is absent — for example, `max_requests_per_call`
/// defaults to `u32::MAX` (no cap) and `schema_version` defaults to `1` (the
/// implicit pre-migration value). The individual getters remain available and
/// always agree with the corresponding field here; this struct is a
/// convenience read for dashboards and health checks that would otherwise need
/// a fan-out of nine separate calls.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractConfig {
    pub paused: bool,
    pub allowlist_enabled: bool,
    pub require_service_registration: bool,
    pub max_requests_per_call: u32,
    pub min_requests_per_call: u32,
    pub max_requests_per_window: u32,
    pub window_seconds: u64,
    pub schema_version: u32,
    pub admin: Option<Address>,
}

/// A combined billing snapshot for an `(agent, service_id)` pair, returned by
/// [`Escrow::get_billing_summary`].
///
/// Provides a coherent, single-round-trip view of usage, price, and the
/// computed bill, all resolved from the same ledger state. This prevents
/// race conditions where separate reads could return inconsistent snapshots
/// (e.g., a usage value from one ledger and a price from another).
///
/// For unknown pairs (no usage recorded, no price set), all numeric fields
/// default to zero and `last_settlement` is `None`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BillingSummary {
    /// Accumulated request count for the pair. Defaults to `0` when no usage
    /// has been recorded.
    pub requests: u32,
    /// Per-request price in stroops. Defaults to `0` when no price has been set.
    pub price_stroops: i128,
    /// Computed bill: `requests * price_stroops` with saturating arithmetic.
    /// Saturates at `i128::MAX` on overflow.
    pub billed: i128,
    /// Ledger timestamp (seconds since unix epoch) of the last `settle` call
    /// that drained this pair, or `None` if the pair has never been settled.
    pub last_settlement: Option<u64>,
}

/// A single-service pricing snapshot, returned by
/// [`Escrow::get_service_pricing`].
///
/// Combines the four separate reads a caller would otherwise need
/// (`get_service_price`, `get_price_tiers`, `get_min_service_price`,
/// `get_max_service_price`) into one round trip, so an indexer or
/// dashboard can answer "what would this service currently charge, and
/// under what global bounds" without four calls that could observe four
/// different ledger states.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePricing {
    /// Flat per-request price in stroops. Defaults to `0` when unset.
    /// Ignored by billing when `tiers` is `Some` — see [`Self::tiers`].
    pub price_stroops: i128,
    /// The volume-discount tier schedule, if one is configured. When
    /// `Some`, `compute_billing` and `settle` use this instead of
    /// `price_stroops`.
    pub tiers: Option<Vec<PriceTier>>,
    /// Global minimum service price in stroops (`0` when unset). Bounds
    /// `set_service_price` only — does **not** bound `tiers` entries; see
    /// `docs/escrow/pricing.md`.
    pub min_bound: i128,
    /// Global maximum service price in stroops (`i128::MAX` when unset).
    /// Same flat-rate-only scope as `min_bound`.
    pub max_bound: i128,
    /// Whether the service is currently disabled
    /// (`set_service_disabled`). A disabled service rejects both new
    /// prices and new usage regardless of the values above.
    pub disabled: bool,
}

/// A cross-service settlement snapshot for one agent, returned by
/// [`Escrow::get_agent_settlement_summary`].
///
/// [`BillingSummary`] covers a single `(agent, service_id)` pair; this
/// covers the agent's whole active-service index in one bounded read (see
/// `MAX_AGENT_SERVICE_INDEX`), so callers don't have to fan out a
/// `get_billing_summary` call per service to answer "is this agent fully
/// settled, and when did they last settle?"
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSettlementSummary {
    /// Lifetime settled amount for this agent across all services, in
    /// stroops. Same value as `get_total_settled_by_agent`. Defaults to `0`.
    pub total_settled: i128,
    /// Number of services in the agent's active-service index that
    /// currently carry non-zero (unsettled) usage.
    pub outstanding_services: u32,
    /// The most recent `LastSettlement` timestamp among services *currently
    /// in the agent's active-service index*, or `None` if none of them
    /// carry a stamp (including when the index is empty).
    ///
    /// Caveat: `settle` removes a service from the index once it is fully
    /// drained, so a service settled individually (rather than via
    /// `settle_all`, which does not deindex) stops contributing to this
    /// field the moment it is settled. Use `get_last_settlement` for the
    /// authoritative per-`(agent, service_id)` timestamp regardless of
    /// index membership.
    pub last_settlement: Option<u64>,
}

/// Storage keys used by the escrow contract.
///
/// Persistent slots survive across full TTL cycles and are appropriate for
/// long-lived configuration (e.g. the admin address) and for per-(agent,
/// service) usage accumulators that AgentPay's settlement loop reads.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Operational admin address; set once at `init`.
    Admin,
    /// Accumulated usage counter for a given `(agent, service_id)` pair.
    Usage(Address, Symbol),
    /// Price per request, in stroops, for a registered service.
    ServicePrice(Symbol),
    /// `true` when the contract is paused (no state-changing entrypoints
    /// accept calls).
    Paused,
    /// Pending admin address proposed via `propose_admin_transfer`,
    /// waiting on `accept_admin_transfer`. Two-step handover prevents
    /// accidentally locking out of the contract via a bad signing key.
    PendingAdmin,
    /// `true` if a service is registered (i.e. admin has explicitly
    /// listed it). When `RequireServiceRegistration` is enabled,
    /// `record_usage` rejects unknown services with a typed error.
    ServiceRegistered(Symbol),
    /// `true` when `record_usage` should reject unknown services.
    RequireServiceRegistration,
    /// Upper bound on `requests` per single `record_usage` call. When
    /// set, `record_usage` rejects calls above this delta. Defaults to
    /// `u32::MAX` (no limit) when absent.
    MaxRequestsPerCall,
    /// Lower bound on `requests` per single `record_usage` call.
    /// Useful for amortising the per-write ledger cost.
    MinRequestsPerCall,
    /// Per-agent allowlist flag. When `AllowlistEnabled` is true,
    /// `record_usage` rejects agents whose entry is absent or false.
    AgentAllowed(Address),
    /// Prepaid credit balance for an agent, in stroops. Settlement draws
    /// down this balance; `record_usage` rejects a call when the balance is
    /// insufficient for the bill that will be settled for the new total.
    AgentCredit(Address),
    /// Master toggle: when true, the per-agent allowlist is enforced.
    AllowlistEnabled,
    /// Cross-service total request count for a given agent.
    /// Settlement does NOT reset this counter; it is the lifetime
    /// signal for analytics and SLA tiering.
    TotalUsageByAgent(Address),
    /// Protocol-wide lifetime request counter, written by every
    /// successful `record_usage`. Useful as a single grafana gauge.
    TotalRequestsAllTime,
    /// Cross-service lifetime settled amount, in stroops, for a given
    /// agent. Settlement does NOT reset this counter; it is the lifetime
    /// value signal for credit limits, loyalty pricing, and SLA tiering.
    TotalSettledByAgent(Address),
    /// Protocol-wide lifetime settled amount, in stroops, written by every
    /// successful settlement drain. Saturates at `i128::MAX`.
    TotalSettledAllTime,
    /// Ledger timestamp (seconds since unix epoch) at which the last
    /// `settle` call drained this `(agent, service_id)` pair. Lets
    /// off-chain SLA monitoring catch stuck settlement cycles.
    LastSettlement(Address, Symbol),
    /// Monotonic optimistic-concurrency version for settlement of one
    /// `(agent, service_id)` pair. Starts at 0 and bumps after every
    /// successful direct or batched settlement of that pair.
    SettlementVersion(Address, Symbol),
    /// On-chain storage schema version. Distinct from the contract
    /// version() (which is the compiled wasm version): SchemaVersion
    /// tracks what the persisted state layout looks like so callers can
    /// confirm a `migrate` has run on a redeployed contract.
    SchemaVersion,
    /// Free-form metadata (`description`, `owner`) about a service.
    ServiceMetadata(Symbol),
    /// `true` when a service has been temporarily disabled by admin.
    /// Distinct from `ServiceRegistered`: a registered service can be
    /// disabled without unregistering, preserving the metadata and the
    /// per-(agent, service) usage history.
    ServiceDisabled(Symbol),
    /// Max `requests` an agent may accumulate within one rate-limit
    /// window. `0` (the default) disables the limiter entirely.
    MaxRequestsPerWindow,
    /// Length of the fixed rate-limit window in seconds. `0` (the
    /// default) disables the limiter entirely.
    WindowSeconds,
    /// Per-agent fixed-window rate-limit state: `(window_start, count)`
    /// where `window_start` is the ledger timestamp the current window
    /// opened and `count` is the requests accumulated in it so far.
    RateWindow(Address),
    /// Per-agent blocklist flag. When `true`, `record_usage` rejects the
    /// agent with `AgentBlocked`, taking precedence over the allowlist.
    AgentBlocked(Address),
    /// Volume-discount tier schedule for a service: a `Vec<PriceTier>`
    /// sorted ascending by `threshold_requests`. When present,
    /// `compute_billing` and `settle` use the tier-aware helper instead
    /// of the flat `ServicePrice`. Falls back to `ServicePrice` (or 0)
    /// when absent, preserving full backward compatibility.
    PriceTiers(Symbol),
    /// Per-agent service index: a `Vec<Symbol>` of service ids for which
    /// this agent has accumulated (or had) non-zero usage since the last
    /// settlement. Maintained by `index_agent_service` / `deindex_agent_service`.
    AgentServiceIndex(Address),
    /// Alias used by `settle_all` to load the agent's service index.
    /// Points to the same logical slot as `AgentServiceIndex`; kept as a
    /// separate variant for API clarity.
    AgentServices(Address),
    /// Open-dispute flag for a `(agent, service_id)` pair. `true` while
    /// an unresolved dispute is pending.
    Dispute(Address, Symbol),
    /// Usage-alert threshold for a `(agent, service_id)` pair. When the
    /// accumulated usage crosses this value on a `record_usage` call a
    /// `usage_hi` event is emitted (edge-triggered).
    UsageAlertThreshold,
    /// Global minimum service price in stroops. When set, `set_service_price`
    /// rejects any price below this floor with `PriceOutOfBounds`.
    /// Defaults to `0` (no floor) when absent.
    MinServicePrice,
    /// Global maximum service price in stroops. When set, `set_service_price`
    /// rejects any price above this ceiling with `PriceOutOfBounds`.
    /// Defaults to `i128::MAX` (no ceiling) when absent.
    MaxServicePrice,
}

/// Typed contract errors. Codes are append-only to keep client SDKs stable.
///
/// See [`docs/escrow/errors.md`](../../docs/escrow/errors.md) for the full
/// trigger-condition table and [`CONTRIBUTING.md`](../../CONTRIBUTING.md) for
/// the append-only convention and the PR checklist.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EscrowError {
    /// `init` was already called and the admin address is already stored.
    AlreadyInitialized = 1,
    /// `record_usage` was called with `requests == 0`.
    RequestsMustBePositive = 2,
    /// An admin-gated entrypoint was invoked but the admin is not set.
    NotInitialized = 3,
    /// A state-changing entrypoint was called while `Paused` is `true`.
    ContractPaused = 4,
    /// `accept_admin_transfer` was called but no pending admin is set.
    NoPendingAdminTransfer = 5,
    /// `accept_admin_transfer` was called by the wrong address.
    NotPendingAdmin = 6,
    /// `record_usage` referenced a service that has not been registered
    /// while strict registration is enabled.
    ServiceNotRegistered = 7,
    /// `record_usage` was called with a `requests` value above the
    /// configured `MaxRequestsPerCall` cap.
    RequestsExceedsMaxPerCall = 8,
    /// `record_usage` was called with a `requests` value below the
    /// configured `MinRequestsPerCall` floor.
    RequestsBelowMinPerCall = 9,
    /// `record_usage` was called by/for an agent not on the allowlist
    /// while strict allowlisting is enabled.
    AgentNotAllowed = 10,
    /// `migrate_v1_to_v2` was called from a non-v1 schema. v2 itself is
    /// already migrated.
    MigrationVersionMismatch = 11,
    /// `record_usage` referenced a service that has been disabled.
    ServiceDisabled = 12,
    /// A metadata-scoped entrypoint referenced a service that has no
    /// `ServiceMetadata` slot set.
    ServiceMetadataNotFound = 13,
    /// `propose_admin_transfer` was called with the current admin as the
    /// proposed new admin — a no-op handover that is rejected to surface
    /// caller mistakes early.
    InvalidAdminProposal = 14,
    /// `record_usage` would push the agent's per-window request count
    /// above the configured `MaxRequestsPerWindow` cap.
    RateLimitExceeded = 15,
    /// `get_usage_batch` was called with more than `MAX_BATCH_READ` pairs.
    BatchTooLarge = 16,
    /// `record_usage` was called by/for an agent on the per-agent
    /// blocklist. Takes precedence over the allowlist.
    AgentBlocked = 17,
    /// `set_price_tiers` was called with a malformed tier schedule:
    /// either the schedule is empty, contains duplicate thresholds, or
    /// is not strictly ascending in `threshold_requests`.
    InvalidPriceTiers = 18,
    /// `settle_all` was called but the agent's service index exceeds
    /// `MAX_SETTLE_ALL`.
    SettleAllTooLarge = 19,
    /// `open_dispute` was called but a dispute is already open for the
    /// given `(agent, service_id)` pair.
    DisputeAlreadyOpen = 20,
    /// `resolve_dispute` was called but no dispute is open for the pair.
    NoOpenDispute = 21,
    /// `resolve_dispute` was called with `refund_requests` exceeding the
    /// current accumulated usage — prevents double-refunds.
    RefundExceedsUsage = 22,
    /// `set_min_requests_per_call` was called with a `min` that exceeds the
    /// currently-stored `MaxRequestsPerCall`, or `set_max_requests_per_call`
    /// was called with a `max` that is below the currently-stored
    /// `MinRequestsPerCall`. Either configuration would make the
    /// `min <= max` invariant unsatisfiable, permanently bricking metering
    /// until corrected. An equal value (`min == max`) is accepted — it
    /// enforces an exact per-call request count.
    InvalidRequestBounds = 23,
    /// `set_service_price` was called with a price outside the configured
    /// `[MinServicePrice, MaxServicePrice]` bounds.
    PriceOutOfBounds = 24,
    /// `set_price_bounds` was called with `min_stroops > max_stroops`,
    /// which would create an impossible price band.
    InvertedPriceBand = 25,
    /// An entrypoint was called by an address that is neither the
    /// contract admin nor the authorised party (e.g. a service owner
    /// attempting to settle a service they do not own, or transfer
    /// ownership of metadata they do not control).
    Unauthorized = 26,
    /// `transfer_service_ownership` was called with a `new_owner` that
    /// matches the current owner — a no-op that would waste a storage
    /// write and emit a spurious `owner_chg` event. Rejected to match
    /// the existing `InvalidAdminProposal` guard on
    /// `propose_admin_transfer`.
    InvalidOwnerTransfer = 27,
    /// `record_usage` was called for an agent whose prepaid credit balance is
    /// insufficient to cover the bill that would be settled for the updated
    /// usage total.
    InsufficientCreditBalance = 28,
    /// `settle` was called with an expected settlement version that does
    /// not match the pair's current version.
    ///
    /// Code 30 intentionally avoids the code 29 allocation already used by
    /// the in-flight settlement replay guard, so the two independent changes
    /// cannot publish conflicting stable discriminants.
    VersionConflict = 30,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageRecord {
    pub agent: Address,
    pub service_id: Symbol,
    pub requests: u32,
}

/// A single volume-discount tier for a service.
///
/// A tier applies to all requests **up to and including** `threshold_requests`
/// that have not already been consumed by a lower tier. In a multi-tier
/// schedule the tiers must be sorted ascending by `threshold_requests` with
/// no duplicates; `set_price_tiers` enforces this at write-time.
///
/// The last tier in the schedule (the one with the highest threshold) acts as
/// an open-ended tier: any requests beyond `threshold_requests` of all
/// previous tiers are billed at this marginal `price_stroops`. A threshold of
/// `u32::MAX` on the final tier therefore means "unlimited".
///
/// Example schedule (ascending):
/// ```text
/// tier 0: threshold=100,  price=10  -> first 100 requests @ 10 stroops each
/// tier 1: threshold=1000, price=7   -> next  900 requests @ 7  stroops each
/// tier 2: threshold=MAX,  price=4   -> remainder          @ 4  stroops each
/// ```
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceTier {
    /// Inclusive upper bound on cumulative requests for this tier. The tier
    /// covers requests from the previous tier's threshold (exclusive) up to
    /// and including this value.
    pub threshold_requests: u32,
    /// Marginal price per request within this tier, in stroops. Must be
    /// non-negative; zero is allowed (free tier).
    pub price_stroops: i128,
}

// New persistent boolean flags should be read/written via `read_flag` /
// `write_flag` so they inherit the `unwrap_or(false)` default convention.
// (See CONTRIBUTING.md § Getter-default convention.)

/// Read a persistent boolean flag, defaulting to `false` when unset.
/// Centralises the `unwrap_or(false)` convention so a new flag can never
/// accidentally default to `true` or skip a check.
fn read_flag(env: &Env, key: &DataKey) -> bool {
    env.storage().persistent().get(key).unwrap_or(false)
}

/// Write a persistent boolean flag.
fn write_flag(env: &Env, key: &DataKey, value: bool) {
    env.storage().persistent().set(key, &value);
}

/// Reject a `service_id` that is not currently usable, shared by every
/// entrypoint that either records usage against a service or attaches a
/// price to one (`record_usage`, `set_service_price`).
///
/// Panics with [`EscrowError::ServiceNotRegistered`] when strict
/// registration is enabled (`RequireServiceRegistration`) and the service
/// has not been registered. Panics with [`EscrowError::ServiceDisabled`]
/// when the service has been explicitly disabled, regardless of the
/// strict-registration setting.
fn ensure_service_usable(env: &Env, service_id: &Symbol) {
    // Conditional read: ServiceRegistered is only touched when strict
    // registration is enabled (the `&&` short-circuits otherwise).
    if read_flag(env, &DataKey::RequireServiceRegistration)
        && !read_flag(env, &DataKey::ServiceRegistered(service_id.clone()))
    {
        panic_with_error!(env, EscrowError::ServiceNotRegistered);
    }
    if read_flag(env, &DataKey::ServiceDisabled(service_id.clone())) {
        panic_with_error!(env, EscrowError::ServiceDisabled);
    }
}

/// Publish the shared `cfg_set(tag, value)` event used by every single-
/// scalar admin config setter (`set_allowlist_enabled`,
/// `set_min_requests_per_call`, `set_max_requests_per_call`,
/// `set_max_requests_per_window`, `set_rate_window_seconds`,
/// `set_require_service_registration`). Centralising the publish call keeps
/// the topic and payload shape identical at every call site — a new config
/// setter only needs to pick a `tag` and call this helper, rather than
/// duplicating the `env.events().publish(...)` block.
fn publish_cfg_event<T>(env: &Env, tag: Symbol, value: T)
where
    T: IntoVal<Env, Val>,
    (Symbol, T): IntoVal<Env, Val>,
{
    env.events().publish((events::TOPIC_CFG_SET,), (tag, value));
}

// Shared access-control helpers.
//
// Admin-gated entrypoints and the pause gate repeat the same small blocks of
// logic. These free functions centralise that logic so every call site stays
// byte-for-byte identical in behaviour (same error codes, same checks) while
// removing the duplication. They are deliberately plain module-level `fn`s,
// not `Escrow` methods: call them directly (e.g. `require_admin(&env)`), not
// via `Self::`. When adding a new admin-gated entrypoint, start its body with
// `let admin = require_admin(&env);` (or drop the binding when the admin value
// is unused), and gate state-changing entrypoints with `ensure_not_paused`
// at the same position the existing convention dictates.

/// Load the stored admin and require its authorization.
///
/// Panics with [`EscrowError::NotInitialized`] when no admin has been set
/// (i.e. `init` has not run). Otherwise calls `admin.require_auth()` and
/// returns the admin address. This is the canonical admin gate for
/// admin-only entrypoints.
fn require_admin(env: &Env) -> Address {
    let admin = get_admin_address(env);
    admin.require_auth();
    admin
}

/// Returns `true` iff `caller` is the admin or the given service `owner`.
///
/// Several entrypoints (`settle`, `settle_all`,
/// `transfer_service_ownership`) authorize either the contract admin or a
/// specific service's `ServiceMetadata.owner`. This centralises that
/// comparison; call sites remain responsible for loading the relevant
/// `owner` and for panicking with their own (call-site-specific) error
/// code, since existing call sites do not all use the same code for this
/// rejection.
fn is_owner_or_admin(admin: &Address, caller: &Address, owner: &Address) -> bool {
    caller == admin || caller == owner
}

/// Shared settlement-authorization check used by [`Escrow::settle`] and
/// [`Escrow::settle_all`]: `caller` may settle `service_id` if it is the
/// contract admin, or the `ServiceMetadata.owner` of that service.
///
/// Admin callers skip the metadata lookup entirely (matching the previous
/// inline behaviour at each call site), so calling this with `caller ==
/// admin` never panics even for a `service_id` with no metadata set.
///
/// Panics with [`EscrowError::ServiceMetadataNotFound`] if a non-admin
/// caller references a service with no metadata, or with
/// `unauthorized_err` if a non-admin, non-owner caller is rejected --
/// callers pass their own existing code here (`settle` uses
/// `NotPendingAdmin`, `settle_all` uses `Unauthorized`) so this extraction
/// changes no ABI-visible rejection behaviour.
fn require_settlement_authorized(
    env: &Env,
    admin: &Address,
    caller: &Address,
    service_id: &Symbol,
    unauthorized_err: EscrowError,
) {
    if caller == admin {
        return;
    }
    let meta: ServiceMetadata = env
        .storage()
        .persistent()
        .get(&DataKey::ServiceMetadata(service_id.clone()))
        .unwrap_or_else(|| panic_with_error!(env, EscrowError::ServiceMetadataNotFound));
    if !is_owner_or_admin(admin, caller, &meta.owner) {
        panic_with_error!(env, unauthorized_err);
    }
}

/// Reject the call if the contract is currently paused.
///
/// Panics with [`EscrowError::ContractPaused`] when the `Paused` flag is set.
/// Mirrors the inline pause check used by state-changing entrypoints.
fn ensure_not_paused(env: &Env) {
    if read_flag(env, &DataKey::Paused) {
        panic_with_error!(env, EscrowError::ContractPaused);
    }
}

/// Remaining-ledger threshold below which a persistent entry's TTL is
/// refreshed.  Chosen as ~7 days (100 800 ledgers at ~1440 ledgers/day)
/// so entries are bumped well before archival while keeping the cost of
/// the no-op path (TTL above threshold) negligible.
pub(crate) const LEDGERS_TTL_THRESHOLD: u32 = 100_800;

/// Target TTL (in ledgers) applied when an entry falls below the
/// threshold.  ~14 days (201 600 ledgers) provides a comfortable margin
/// before the next required bump.
pub(crate) const LEDGERS_TTL_EXTEND_TO: u32 = 201_600;

/// Extend the TTL of a persistent storage entry when it falls at or
/// below [`LEDGERS_TTL_THRESHOLD`].  The entry's TTL is then set to
/// [`LEDGERS_TTL_EXTEND_TO`].  When the current TTL is already above
/// the threshold the call is a host-level no-op and costs essentially
/// nothing.
///
/// This is the single shared helper for TTL bumping so the policy lives
/// in one place and is easy to audit.
fn bump_persistent(env: &Env, key: &DataKey) {
    if env.storage().persistent().has(key) {
        env.storage()
            .persistent()
            .extend_ttl(key, LEDGERS_TTL_THRESHOLD, LEDGERS_TTL_EXTEND_TO);
    }
}

/// Read the admin address without requiring auth.
///
/// This is the shared storage precondition for entrypoints that accept either
/// the admin or another authorized caller. It preserves the canonical
/// [`EscrowError::NotInitialized`] rejection when `init` has not run.
fn get_admin_address(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&DataKey::Admin)
        .unwrap_or_else(|| panic_with_error!(env, EscrowError::NotInitialized))
}

/// Read the accumulated usage for an `(agent, service_id)` pair.
/// Returns `0` when no usage has been recorded.
fn read_usage(env: &Env, agent: &Address, service_id: &Symbol) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::Usage(agent.clone(), service_id.clone()))
        .unwrap_or(0)
}

/// Read an agent's prepaid credit balance in stroops.
/// Returns `0` when no credit balance has been recorded.
fn read_agent_credit(env: &Env, agent: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::AgentCredit(agent.clone()))
        .unwrap_or(0)
}

/// Check that an agent's prepaid credit balance (if set) can cover the projected bill for `total_requests`.
///
/// Panics with [`EscrowError::InsufficientCreditBalance`] when `credit_balance > 0`
/// and the projected bill for `total_requests` exceeds that balance.
fn check_debit_precondition(env: &Env, agent: &Address, service_id: &Symbol, total_requests: u32) {
    let credit_balance = read_agent_credit(env, agent);
    if credit_balance > 0 {
        let projected_bill = compute_billing_for_requests(env, service_id, total_requests);
        if projected_bill > credit_balance {
            panic_with_error!(env, EscrowError::InsufficientCreditBalance);
        }
    }
}

/// Draw down an agent's prepaid credit balance by `billed` stroops.
///
/// If `billed > 0` and `credit_balance > 0`, deducts `min(billed, credit_balance)` from the agent's
/// prepaid credit balance, updates storage, and publishes a `cred_deb` event.
fn debit_agent_credit(env: &Env, agent: &Address, billed: i128) {
    let credit_balance = read_agent_credit(env, agent);
    if billed > 0 && credit_balance > 0 {
        let debit = billed.min(credit_balance);
        let new_balance = credit_balance.saturating_sub(debit);
        env.storage()
            .persistent()
            .set(&DataKey::AgentCredit(agent.clone()), &new_balance);
        env.events().publish(
            (events::TOPIC_CRED_DEB,),
            (agent.clone(), debit, new_balance),
        );
    }
}

/// Compute the bill for a given service and request total using the configured
/// flat price or tier schedule. This is shared by `record_usage`, `settle`,
/// and the public `compute_billing` read.
fn compute_billing_for_requests(env: &Env, service_id: &Symbol, requests: u32) -> i128 {
    if let Some(tiers) = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<PriceTier>>(&DataKey::PriceTiers(service_id.clone()))
    {
        compute_billing_tiered(requests, &tiers)
    } else {
        let price: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::ServicePrice(service_id.clone()))
            .unwrap_or(0);
        (requests as i128).saturating_mul(price)
    }
}

/// Add `service_id` to the per-agent service index if not already present.
/// Capped at [`MAX_AGENT_SERVICE_INDEX`]; once full, new services are silently
/// dropped (the per-pair counter is still written; only the index entry is
/// skipped).
fn index_agent_service(env: &Env, agent: &Address, service_id: &Symbol) {
    let key = DataKey::AgentServiceIndex(agent.clone());
    let mut index: Vec<Symbol> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    // Idempotent: skip if already indexed.
    for existing in index.iter() {
        if existing == *service_id {
            return;
        }
    }
    if index.len() >= MAX_AGENT_SERVICE_INDEX {
        return; // cap reached; drop silently
    }
    index.push_back(service_id.clone());
    env.storage().persistent().set(&key, &index);
}

/// Remove `service_id` from the per-agent service index. No-op when absent.
fn deindex_agent_service(env: &Env, agent: &Address, service_id: &Symbol) {
    let key = DataKey::AgentServiceIndex(agent.clone());
    let index: Vec<Symbol> = match env.storage().persistent().get(&key) {
        Some(v) => v,
        None => return,
    };
    let mut new_index: Vec<Symbol> = Vec::new(env);
    for existing in index.iter() {
        if existing != *service_id {
            new_index.push_back(existing);
        }
    }
    env.storage().persistent().set(&key, &new_index);
}

/// Compute the tier-aware bill for `requests` using the provided tier schedule.
///
/// Iterates tiers in order (assumed strictly ascending by `threshold_requests`).
/// Each tier covers the band from the previous tier's threshold (exclusive) to
/// this tier's threshold (inclusive). The last tier covers all remaining
/// requests. Saturates at `i128::MAX`.
fn compute_billing_tiered(requests: u32, tiers: &Vec<PriceTier>) -> i128 {
    let mut remaining = requests;
    let mut total: i128 = 0;
    let mut prev_threshold: u32 = 0;

    for i in 0..tiers.len() {
        let tier = tiers.get(i).unwrap();
        let tier_capacity = tier.threshold_requests.saturating_sub(prev_threshold);
        let in_tier = if remaining <= tier_capacity {
            remaining
        } else {
            tier_capacity
        };
        let cost = (in_tier as i128).saturating_mul(tier.price_stroops);
        total = total.saturating_add(cost);
        remaining = remaining.saturating_sub(in_tier);
        prev_threshold = tier.threshold_requests;
        if remaining == 0 {
            break;
        }
    }

    // Any requests beyond all tier thresholds are billed at the last tier's price.
    if remaining > 0 && !tiers.is_empty() {
        let last = tiers.get(tiers.len() - 1).unwrap();
        let overflow_cost = (remaining as i128).saturating_mul(last.price_stroops);
        total = total.saturating_add(overflow_cost);
    }

    total
}

/// Add a successful settlement bill to the lifetime settled-amount counters.
///
/// Non-positive bills leave the counters untouched, preserving the monotonic
/// lifetime invariant while avoiding unnecessary zero-value storage slots.
fn add_settled_totals(env: &Env, agent: &Address, billed: i128) {
    if billed <= 0 {
        return;
    }

    let agent_key = DataKey::TotalSettledByAgent(agent.clone());
    let agent_prev: i128 = env.storage().persistent().get(&agent_key).unwrap_or(0);
    env.storage()
        .persistent()
        .set(&agent_key, &agent_prev.saturating_add(billed));

    let all_prev: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::TotalSettledAllTime)
        .unwrap_or(0);
    env.storage().persistent().set(
        &DataKey::TotalSettledAllTime,
        &all_prev.saturating_add(billed),
    );
}
#[contract]
pub struct Escrow;

#[contractimpl]
impl Escrow {
    /// Initialize the escrow contract with the operational admin.
    ///
    /// Requires `admin.require_auth()` and panics with
    /// [`EscrowError::AlreadyInitialized`] if an admin has already been stored.
    /// Idempotency is enforced strictly: a second call with the same admin
    /// address still fails. Use a redeploy or a future admin-rotation
    /// entrypoint if the admin needs to change.
    pub fn init(env: Env, admin: Address) {
        if env.storage().persistent().has(&DataKey::Admin) {
            panic_with_error!(&env, EscrowError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().persistent().set(&DataKey::Admin, &admin);
        env.storage()
            .persistent()
            .set(&DataKey::SchemaVersion, &CURRENT_SCHEMA);
    }

    /// Returns the admin address stored at `init`, if any.
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Admin)
    }

    /// Record that an agent has consumed usage for a service.
    ///
    /// Accumulates `requests` into the persistent counter keyed by
    /// `(agent, service_id)`. Rejects zero-request calls with
    /// [`EscrowError::RequestsMustBePositive`] so off-chain settlement
    /// loops never see a no-op event in the audit trail. Saturates at
    /// `u32::MAX` rather than overflowing — the settlement loop is
    /// expected to drain the counter long before that becomes plausible.
    ///
    /// Returns a `UsageRecord` carrying the *new total*, not the delta,
    /// so the caller can confirm the post-write state without a second
    /// storage read.
    ///
    /// # Authorization
    ///
    /// **Step 0**: The recorded `agent` must authorize this call via
    /// `agent.require_auth()`. This closes a usage-forgery vector where any
    /// party could inflate a competitor agent's counters (and therefore its
    /// bill on the next `settle`) with no signature from the agent.
    ///
    /// Soroban's auth tree supports sub-invocation authorization: an agent can
    /// pre-authorize a trusted metering operator to call `record_usage` on its
    /// behalf by having the operator's call appear as a sub-invocation of an
    /// agent-signed outer call. This allows existing off-chain settlement loops
    /// to continue operating without requiring every agent to sign each
    /// individual `record_usage` call directly:
    ///
    /// 1. The agent signs an outer transaction that authorizes the operator's
    ///    contract call via Soroban's `authorize_as_current_contract` or
    ///    sub-invocation auth.
    /// 2. The operator's metering loop submits `record_usage` as a
    ///    sub-invocation within that authorized context.
    /// 3. Alternatively, agents can sign each `record_usage` call directly
    ///    (standard path) if the metering loop supports it.
    ///
    /// # Validation order
    ///
    /// Auth checks are performed in this order (early exits on first failure):
    ///
    /// | Step | Check                  | Error                          |
    /// | ---- | ---------------------- | ------------------------------ |
    /// | 0    | `agent.require_auth()` | Soroban host auth error        |
    /// | 1    | Contract paused        | `#4 ContractPaused`            |
    /// | 2    | `requests == 0`        | `#2 RequestsMustBePositive`    |
    /// | 3    | `requests > max`       | `#8 RequestsExceedsMaxPerCall` |
    /// | 4    | `requests < min`       | `#9 RequestsBelowMinPerCall`   |
    /// | 5    | Service not registered | `#7 ServiceNotRegistered`      |
    /// | 6    | Service disabled       | `#12 ServiceDisabled`          |
    /// | 7    | Agent blocked          | `#17 AgentBlocked`             |
    /// | 8    | Agent not allowed      | `#10 AgentNotAllowed`          |
    pub fn record_usage(
        env: Env,
        agent: Address,
        service_id: Symbol,
        requests: u32,
    ) -> UsageRecord {
        // Step 0: Require the agent to authorize this call. This prevents usage
        // forgery where any party could inflate a competitor's bill. Soroban's
        // auth tree supports sub-invocation authorization, allowing a metering
        // operator to record on behalf of an agent if the agent has authorized
        // the operator's contract call.
        agent.require_auth();

        ensure_not_paused(&env);
        if requests == 0 {
            panic_with_error!(&env, EscrowError::RequestsMustBePositive);
        }
        // Cached: read once, compared once. Defaults to u32::MAX (no cap).
        let max_per_call: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerCall)
            .unwrap_or(u32::MAX);
        if requests > max_per_call {
            panic_with_error!(&env, EscrowError::RequestsExceedsMaxPerCall);
        }
        // Cached: read once, compared once. Defaults to 0 (no floor).
        let min_per_call: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MinRequestsPerCall)
            .unwrap_or(0);
        if requests < min_per_call {
            panic_with_error!(&env, EscrowError::RequestsBelowMinPerCall);
        }
        ensure_service_usable(&env, &service_id);
        // Per-agent blocklist takes precedence over the allowlist: a blocked
        // agent is rejected even if also allow-listed.
        if read_flag(&env, &DataKey::AgentBlocked(agent.clone())) {
            panic_with_error!(&env, EscrowError::AgentBlocked);
        }
        // Conditional read: AgentAllowed is only touched when the allowlist is
        // enabled (the `&&` short-circuits otherwise).
        if read_flag(&env, &DataKey::AllowlistEnabled)
            && !read_flag(&env, &DataKey::AgentAllowed(agent.clone()))
        {
            panic_with_error!(&env, EscrowError::AgentNotAllowed);
        }

        // Per-agent fixed-window rate limit. Enabled only when both the cap
        // and the window length are non-zero. The window is anchored at the
        // first in-window call's timestamp and rolls forward whole-window
        // (fixed, not sliding) once `now >= window_start + window_seconds`.
        let max_per_window: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerWindow)
            .unwrap_or(0);
        let window_seconds: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::WindowSeconds)
            .unwrap_or(0);
        if max_per_window > 0 && window_seconds > 0 {
            let now = env.ledger().timestamp();
            let (window_start, count): (u64, u32) = env
                .storage()
                .persistent()
                .get(&DataKey::RateWindow(agent.clone()))
                .unwrap_or((0, 0));
            // Roll the window forward if the current one has expired. The
            // agent can never reset it early: window_start only advances.
            let (window_start, count) = if now >= window_start.saturating_add(window_seconds) {
                (now, 0u32)
            } else {
                (window_start, count)
            };
            let new_count = count.saturating_add(requests);
            if new_count > max_per_window {
                panic_with_error!(&env, EscrowError::RateLimitExceeded);
            }
            env.storage().persistent().set(
                &DataKey::RateWindow(agent.clone()),
                &(window_start, new_count),
            );
        }

        let key = DataKey::Usage(agent.clone(), service_id.clone());
        let prev: u32 = env.storage().persistent().get(&key).unwrap_or(0);
        let total = prev.saturating_add(requests);
        check_debit_precondition(&env, &agent, &service_id, total);
        // saturate: settlement drains long before u32::MAX; never panic the hot path.
        env.storage().persistent().set(&key, &total);

        // Maintain per-agent service index. index_agent_service is idempotent
        // (no-op when the service is already indexed), so it is safe to call on
        // every record_usage regardless of whether this is the first call for
        // the (agent, service_id) pair.
        index_agent_service(&env, &agent, &service_id);

        // Cross-service lifetime counter for the agent. Saturates at u32::MAX.
        let total_key = DataKey::TotalUsageByAgent(agent.clone());
        let prev_total: u32 = env.storage().persistent().get(&total_key).unwrap_or(0);
        // saturate: settlement drains long before u32::MAX; never panic the hot path.
        env.storage()
            .persistent()
            .set(&total_key, &prev_total.saturating_add(requests));

        // Protocol-wide lifetime counter (u64 to delay the saturation horizon).
        let proto_prev: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalRequestsAllTime)
            .unwrap_or(0);
        // u64 horizon; saturate not panic.
        env.storage().persistent().set(
            &DataKey::TotalRequestsAllTime,
            &proto_prev.saturating_add(requests as u64),
        );

        env.events().publish(
            (events::TOPIC_USAGE,),
            (agent.clone(), service_id.clone(), requests, total),
        );

        // Usage-alert threshold: emit `usage_hi` on the crossing edge only.
        //
        // Edge-trigger semantics:
        // - Fires exactly once per settlement window, on the call where the
        //   per-pair total crosses from below-threshold to at/above-threshold.
        // - Does NOT fire on subsequent calls while already above the threshold,
        //   preventing event spam regardless of how many requests accumulate.
        // - Re-arms automatically after `settle` (or `reset_usage`) drains the
        //   counter below the threshold, allowing the next crossing to fire again.
        // - When the threshold is 0 (the default) the block is skipped entirely;
        //   the feature is disabled by default and adds no overhead in that case.
        //
        // Security note: the event payload exposes only data that `record_usage`
        // already returns (agent, service_id, new total) — no additional
        // information is disclosed.
        let threshold: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::UsageAlertThreshold)
            .unwrap_or(0);
        if threshold > 0 && prev < threshold && total >= threshold {
            env.events().publish(
                (events::TOPIC_USAGE_HI,),
                (agent.clone(), service_id.clone(), total),
            );

        }

        UsageRecord {
            agent,
            service_id,
            requests: total,
        }
    }

    /// Subtract `amount` from the per-(agent, service_id) usage counter.
    ///
    /// Admin-gated and pause-respecting. Uses saturating subtraction so the
    /// counter clamps at zero and never underflows. Returns the new total.
    ///
    /// Rejects `amount == 0` with
    /// [`EscrowError::RequestsMustBePositive`] to prevent no-op corrections
    /// in the audit trail.
    ///
    /// # Lifetime counters
    ///
    /// `TotalUsageByAgent` and `TotalRequestsAllTime` are deliberately **not**
    /// adjusted. They track raw reported figures for analytics; corrections
    /// to the per-pair balance should not retroactively distort the lifetime
    /// signal. Off-chain billing pipelines that need the corrected view
    /// should subtract the decrement event from the lifetime counter when
    /// processing the `usage_dec` event.
    ///
    /// # Events
    ///
    /// Emits `usage_dec(agent, service_id, amount, new_total)` so corrections
    /// are auditable and distinguishable from `record_usage` and `settle`.
    pub fn decrement_usage(env: Env, agent: Address, service_id: Symbol, amount: u32) -> u32 {
        ensure_not_paused(&env);
        if amount == 0 {
            panic_with_error!(&env, EscrowError::RequestsMustBePositive);
        }
        let _admin: Address = require_admin(&env);

        let key = DataKey::Usage(agent.clone(), service_id.clone());
        let prev: u32 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_total = prev.saturating_sub(amount);
        env.storage().persistent().set(&key, &new_total);

        env.events().publish(
            (events::TOPIC_USAGE_DEC,),
            (agent, service_id, amount, new_total),
        );

        new_total
    }

    /// Read the ledger timestamp at which `settle` last drained an
    /// `(agent, service_id)` pair. Returns `None` for pairs that have
    /// never been settled (vs. `Some(0)`, which would be a genesis-block
    /// settlement and should not be confused with absent).
    pub fn get_last_settlement(env: Env, agent: Address, service_id: Symbol) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::LastSettlement(agent, service_id))
    }

    /// Return the optimistic-concurrency version for settlement of an
    /// `(agent, service_id)` pair. New pairs start at version 0.
    ///
    /// The version bumps only after a successful settlement transaction.
    /// Callers should read this value immediately before `settle` and pass
    /// it back as `expected_version`.
    pub fn get_settlement_version(env: Env, agent: Address, service_id: Symbol) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::SettlementVersion(agent, service_id))
            .unwrap_or(0)
    }

    /// Return a cross-service settlement snapshot for one agent.
    ///
    /// Pure read — no `require_auth`, no pause gate. Reuses
    /// `get_total_settled_by_agent` for the lifetime total, then does one
    /// bounded pass over the agent's active-service index (capped at
    /// `MAX_AGENT_SERVICE_INDEX`) to count services with outstanding usage
    /// and find the most recent settlement timestamp. Returns
    /// `outstanding_services: 0` and `last_settlement: None` for an agent
    /// with an empty or absent index — never a panic.
    pub fn get_agent_settlement_summary(env: Env, agent: Address) -> AgentSettlementSummary {
        let total_settled = Self::get_total_settled_by_agent(env.clone(), agent.clone());
        let index: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::AgentServiceIndex(agent.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        let mut outstanding_services: u32 = 0;
        let mut last_settlement: Option<u64> = None;
        for service_id in index.iter() {
            if read_usage(&env, &agent, &service_id) > 0 {
                outstanding_services = outstanding_services.saturating_add(1);
            }
            let stamped: Option<u64> = env
                .storage()
                .persistent()
                .get(&DataKey::LastSettlement(agent.clone(), service_id));
            last_settlement = match (last_settlement, stamped) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
        }

        AgentSettlementSummary {
            total_settled,
            outstanding_services,
            last_settlement,
        }
    }

    /// Credit an agent with prepaid balance in stroops.
    ///
    /// Admin-gated and pause-respecting. The balance is drawn down by
    /// `settle` when a bill is successfully settled and is used by
    /// `record_usage` to prevent usage from being accepted when the
    /// prepaid balance is insufficient for the bill that would be settled.
    pub fn credit_agent(env: Env, agent: Address, amount: i128) {
        ensure_not_paused(&env);
        require_admin(&env);
        if amount <= 0 {
            panic_with_error!(&env, EscrowError::RequestsMustBePositive);
        }
        let current = read_agent_credit(&env, &agent);
        env.storage().persistent().set(
            &DataKey::AgentCredit(agent),
            &current.saturating_add(amount),
        );
    }

    /// Read an agent's prepaid credit balance in stroops.
    pub fn get_agent_credit(env: Env, agent: Address) -> i128 {
        read_agent_credit(&env, &agent)
    }

    /// Read the protocol-wide lifetime request counter (u64).
    pub fn get_total_requests_all_time(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalRequestsAllTime)
            .unwrap_or(0)
    }

    /// Read the cross-service lifetime request count for an agent.
    /// Not affected by `settle` (which only drains per-service counters).
    pub fn get_total_usage_by_agent(env: Env, agent: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalUsageByAgent(agent))
            .unwrap_or(0)
    }

    /// Read the cross-service lifetime settled amount for an agent, in stroops.
    ///
    /// This counter is written by settlement drains and never decremented or
    /// reset by later `settle` calls.
    pub fn get_total_settled_by_agent(env: Env, agent: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalSettledByAgent(agent))
            .unwrap_or(0)
    }

    /// Read the protocol-wide lifetime settled amount, in stroops.
    ///
    /// Defaults to `0` before the first billable settlement and saturates at
    /// `i128::MAX` instead of overflowing.
    pub fn get_total_settled_all_time(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalSettledAllTime)
            .unwrap_or(0)
    }

    /// Return the accumulated request count for an `(agent, service_id)`
    /// pair, or `0` if no usage has been recorded yet.
    pub fn get_usage(env: Env, agent: Address, service_id: Symbol) -> u32 {
        read_usage(&env, &agent, &service_id)
    }

    /// Return the raw per-agent fixed-window rate-limit state:
    /// `(window_start, count)` where `window_start` is the ledger timestamp
    /// the current window opened and `count` is the requests accumulated.
    /// Returns `(0, 0)` if no window has opened.
    ///
    /// Pure read — no window advance on read.
    pub fn get_rate_window(env: Env, agent: Address) -> (u64, u32) {
        env.storage()
            .persistent()
            .get(&DataKey::RateWindow(agent))
            .unwrap_or((0, 0))
    }

    /// Return the remaining capacity for an agent in the current rate-limit
    /// window, accounting for window expiration. Returns `MaxRequestsPerWindow`
    /// if the window has expired or the limiter is disabled (window_seconds=0
    /// or max_requests_per_window=0).
    ///
    /// Note: `env.ledger().timestamp()` is used to determine window expiration.
    pub fn get_remaining_in_window(env: Env, agent: Address) -> u32 {
        let max_per_window: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerWindow)
            .unwrap_or(0);
        let window_seconds: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::WindowSeconds)
            .unwrap_or(0);

        if max_per_window == 0 || window_seconds == 0 {
            return max_per_window;
        }

        let (window_start, count): (u64, u32) = Self::get_rate_window(env.clone(), agent);
        let now = env.ledger().timestamp();

        if now >= window_start.saturating_add(window_seconds) {
            max_per_window
        } else {
            max_per_window.saturating_sub(count)
        }
    }

    /// Batched usage read: returns the accumulated request count for each
    /// input `(agent, service_id)` pair, in the same order as `pairs`.
    ///
    /// Pure read — no `require_auth`, no pause gate — so off-chain
    /// dashboards and settlement loops can fetch many counters in one call.
    /// Each entry is resolved with the same `read_usage` helper as
    /// [`Escrow::get_usage`], so unknown pairs return `0` and duplicate
    /// pairs simply yield the same value at each position.
    ///
    /// Panics with [`EscrowError::BatchTooLarge`] when
    /// `pairs.len() > MAX_BATCH_READ`. Rejecting oversized requests keeps
    /// the read loop bounded and the host's storage-read budget
    /// predictable; callers should page larger queries.
    pub fn get_usage_batch(env: Env, pairs: Vec<(Address, Symbol)>) -> Vec<u32> {
        if pairs.len() > MAX_BATCH_READ {
            panic_with_error!(&env, EscrowError::BatchTooLarge);
        }
        let mut results: Vec<u32> = Vec::new(&env);
        for (agent, service_id) in pairs.iter() {
            results.push_back(read_usage(&env, &agent, &service_id));
        }
        results
    }

    /// Return all service ids in the per-agent service index.
    ///
    /// Pure read — no `require_auth`, no pause gate. The returned `Vec`
    /// contains every service id for which this agent has (or had) non-zero
    /// usage since the last time the entry was pruned by `settle`. Services
    /// that have been fully settled are removed from the index, so the result
    /// reflects *currently active* services rather than the full historical
    /// set.
    ///
    /// An agent with no usage history returns an empty vector.
    ///
    /// Callers that only need a bounded slice should prefer
    /// [`Escrow::get_agent_usage_page`].
    pub fn get_agent_services(env: Env, agent: Address) -> Vec<Symbol> {
        env.storage()
            .persistent()
            .get(&DataKey::AgentServiceIndex(agent))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Return a paginated slice of `(service_id, usage)` pairs for an agent.
    ///
    /// Pure read — no `require_auth`, no pause gate. Reads at most `limit`
    /// entries from the per-agent service index starting at position `start`
    /// (zero-based). Each entry is a `(Symbol, u32)` pair of the service id
    /// and its current accumulated request count.
    ///
    /// Pagination rules:
    /// - `start` past the end of the index returns an empty vector.
    /// - `limit` is clamped to [`MAX_BATCH_READ`]; pass `MAX_BATCH_READ` or
    ///   `0` to get the largest page. A zero `limit` is treated as
    ///   `MAX_BATCH_READ` so callers do not have to special-case it.
    /// - The caller can detect the last page when the returned length is
    ///   less than `limit` (or the result is empty).
    ///
    /// This entrypoint bounds the response size and keeps storage-read cost
    /// predictable, unlike `get_agent_services` which returns the full index.
    pub fn get_agent_usage_page(
        env: Env,
        agent: Address,
        start: u32,
        limit: u32,
    ) -> Vec<(Symbol, u32)> {
        let index: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::AgentServiceIndex(agent.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        let effective_limit = if limit == 0 || limit > MAX_BATCH_READ {
            MAX_BATCH_READ
        } else {
            limit
        };

        let total = index.len();
        let mut result: Vec<(Symbol, u32)> = Vec::new(&env);
        let mut pos: u32 = 0;
        for service_id in index.iter() {
            if pos < start {
                pos = pos.saturating_add(1);
                continue;
            }
            if result.len() >= effective_limit {
                break;
            }
            let usage = read_usage(&env, &agent, &service_id);
            result.push_back((service_id, usage));
            pos = pos.saturating_add(1);
        }
        let _ = total;
        result
    }

    /// Set the per-request price (in stroops) for a service.
    ///
    /// Admin-gated. Persists in `DataKey::ServicePrice(service_id)`.
    /// A negative price is rejected at call time so downstream billing
    /// math can assume a non-negative multiplicand; a zero price is
    /// allowed and means "free service" (still records usage, settles to
    /// zero).
    ///
    /// Registration coupling: when `RequireServiceRegistration` (the same
    /// strict-mode flag enforced by `record_usage`) is enabled, a price
    /// can only attach to a registered `service_id` — otherwise the call
    /// is rejected with [`EscrowError::ServiceNotRegistered`]. With the
    /// flag off (the default), pricing is unrestricted, preserving the
    /// prior backward-compatible behaviour.
    ///
    /// A disabled service is always rejected with
    /// [`EscrowError::ServiceDisabled`], mirroring `record_usage`'s gate,
    /// so prices cannot drift onto services that are out of commission.
    ///
    /// Emits `price_set(service_id, price_stroops)` only after every
    /// validation passes.
    pub fn set_service_price(env: Env, service_id: Symbol, price_stroops: i128) {
        require_admin(&env);
        if price_stroops < 0 {
            panic_with_error!(&env, EscrowError::RequestsMustBePositive);
        }
        ensure_service_usable(&env, &service_id);
        // Global price-bounds check. Defaults: floor = 0, ceiling = i128::MAX.
        // If a floor above 0 is configured, a price of 0 ("free service") is
        // explicitly **forbidden** — the admin must lower the floor to 0 first
        // if free services should be re-allowed. This is intentional: the
        // bounds exist to prevent accidental near-zero prices and a floor > 0
        // expresses a clear policy that the service must have a positive cost.
        let price_floor: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::MinServicePrice)
            .unwrap_or(0);
        let price_ceil: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::MaxServicePrice)
            .unwrap_or(i128::MAX);
        if price_stroops < price_floor || price_stroops > price_ceil {
            panic_with_error!(&env, EscrowError::PriceOutOfBounds);
        }
        env.storage()
            .persistent()
            .set(&DataKey::ServicePrice(service_id.clone()), &price_stroops);
        env.events()
            .publish((events::TOPIC_PRICE_SET,), (service_id, price_stroops));
    }

    /// Remove the configured per-request price for a service, freeing the
    /// `DataKey::ServicePrice(service_id)` storage slot.
    ///
    /// Admin-gated and honours the pause gate (panics with
    /// [`EscrowError::ContractPaused`] when paused, consistent with other
    /// admin mutations). Idempotent — removing the price of a service that
    /// was never priced is a no-op.
    ///
    /// After removal, `get_service_price` and `compute_billing` read back
    /// `0`, exactly as for a service that was never priced. Note the
    /// zero-vs-removed distinction: removal frees the underlying storage
    /// slot and emits a `price_rmv` event, whereas `set_service_price(_, 0)`
    /// leaves a stored slot holding `0`. Both read back as `0`, but only
    /// removal reclaims the slot. Emits `price_rmv(service_id)`.
    pub fn remove_service_price(env: Env, service_id: Symbol) {
        ensure_not_paused(&env);
        require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::ServicePrice(service_id.clone()));
        env.events()
            .publish((events::TOPIC_PRICE_RMV,), service_id);
    }

    /// Admin sets a volume-discount tier schedule for a service.
    ///
    /// The schedule is a `Vec<PriceTier>` sorted **strictly ascending** by
    /// `threshold_requests` with no duplicates. `set_price_tiers` validates
    /// the schedule at write-time and rejects malformed input with
    /// [`EscrowError::InvalidPriceTiers`]. An empty schedule is also rejected
    /// — use `remove_price_tiers` to revert to the flat `ServicePrice`.
    ///
    /// Once set, `compute_billing` and `settle` use the tier schedule instead
    /// of the flat `ServicePrice`. The flat price is preserved and can be
    /// restored by removing the tier schedule via `remove_price_tiers`.
    ///
    /// Admin-gated and honours the pause gate. Emits
    /// `tiers_set(service_id)` on success.  Extends the entry's
    /// persistent TTL on write.
    ///
    /// # Tier-schedule invariants (enforced at set-time)
    /// - Must contain at least one entry.
    /// - `threshold_requests` values must be strictly ascending (no ties).
    /// - Each `price_stroops` must be non-negative.
    pub fn set_price_tiers(env: Env, service_id: Symbol, tiers: Vec<PriceTier>) {
        require_admin(&env);
        ensure_not_paused(&env);
        // Reject empty schedules.
        if tiers.is_empty() {
            panic_with_error!(&env, EscrowError::InvalidPriceTiers);
        }
        // Validate: strictly ascending thresholds and non-negative prices.
        let mut prev: u32 = 0;
        for i in 0..tiers.len() {
            let tier = tiers.get(i).unwrap();
            if tier.price_stroops < 0 {
                panic_with_error!(&env, EscrowError::InvalidPriceTiers);
            }
            if i == 0 {
                if tier.threshold_requests == 0 {
                    panic_with_error!(&env, EscrowError::InvalidPriceTiers);
                }
                prev = tier.threshold_requests;
            } else {
                if tier.threshold_requests <= prev {
                    panic_with_error!(&env, EscrowError::InvalidPriceTiers);
                }
                prev = tier.threshold_requests;
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::PriceTiers(service_id.clone()), &tiers);
        bump_persistent(&env, &DataKey::PriceTiers(service_id.clone()));
        env.events()
            .publish((symbol_short!("tiers_set"),), service_id);
    }

    /// Read the tier schedule for a service, or `None` if no schedule has
    /// been set (the service uses flat `ServicePrice` billing).  Extends
    /// the entry's persistent TTL on read.
    pub fn get_price_tiers(env: Env, service_id: Symbol) -> Option<Vec<PriceTier>> {
        let key = DataKey::PriceTiers(service_id);
        let result: Option<Vec<PriceTier>> = env.storage().persistent().get(&key);
        bump_persistent(&env, &key);
        result
    }

    /// Admin removes the tier schedule for a service, reverting billing to
    /// the flat `ServicePrice`. Idempotent — removing an absent schedule is
    /// a no-op. Admin-gated and honours the pause gate. Emits
    /// `tiers_rm(service_id)`.  The entry is deleted, so no TTL
    /// extension is performed.
    pub fn remove_price_tiers(env: Env, service_id: Symbol) {
        require_admin(&env);
        ensure_not_paused(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::PriceTiers(service_id.clone()));
        env.events()
            .publish((symbol_short!("tiers_rm"),), service_id);
    }

    /// Get the per-request price (in stroops) for a service, or 0 if
    /// no price has been configured (the service is free / unset).
    pub fn get_service_price(env: Env, service_id: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::ServicePrice(service_id))
            .unwrap_or(0)
    }

    /// Compute the outstanding bill for an `(agent, service_id)` pair.
    ///
    /// When a tier schedule has been configured via `set_price_tiers` the
    /// bill is the sum of per-tier marginal costs (see [`compute_billing_tiered`]).
    /// When no tier schedule is present the bill falls back to the flat
    /// `ServicePrice`: `accumulated_requests * price_per_request`.
    ///
    /// Returns 0 when either side is zero. Saturates at `i128::MAX` on
    /// overflow — this is read-only, so a saturated value just signals
    /// to the off-chain settlement loop that something has gone wrong
    /// rather than panicking the host.
    pub fn compute_billing(env: Env, agent: Address, service_id: Symbol) -> i128 {
        let requests: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::Usage(agent, service_id.clone()))
            .unwrap_or(0);
        compute_billing_for_requests(&env, &service_id, requests)
    }

    /// Return a combined billing snapshot for an `(agent, service_id)` pair.
    ///
    /// This is a pure read — no `require_auth`, no pause gate — that returns
    /// usage, price, and the computed bill in a single round-trip, all resolved
    /// from the same ledger state. This prevents race conditions where separate
    /// reads could return inconsistent snapshots (e.g., a usage value from one
    /// ledger and a price from another).
    ///
    /// The `billed` field is computed as `requests * price_stroops` using the
    /// same saturating arithmetic as [`Escrow::compute_billing`]. When a tier
    /// schedule is configured, the bill uses the tier-aware computation.
    ///
    /// For unknown pairs (no usage recorded, no price set), all numeric fields
    /// default to zero and `last_settlement` is `None`.
    pub fn get_billing_summary(env: Env, agent: Address, service_id: Symbol) -> BillingSummary {
        let requests = read_usage(&env, &agent, &service_id);
        let price_stroops = env
            .storage()
            .persistent()
            .get(&DataKey::ServicePrice(service_id.clone()))
            .unwrap_or(0);
        let last_settlement = env
            .storage()
            .persistent()
            .get(&DataKey::LastSettlement(agent, service_id.clone()));

        // Use tier schedule when present; fall back to flat price.
        let billed = if let Some(tiers) = env
            .storage()
            .persistent()
            .get::<DataKey, Vec<PriceTier>>(&DataKey::PriceTiers(service_id.clone()))
        {
            compute_billing_tiered(requests, &tiers)
        } else {
            (requests as i128).saturating_mul(price_stroops)
        };

        BillingSummary {
            requests,
            price_stroops,
            billed,
            last_settlement,
        }
    }

    /// Return a service's full pricing configuration in one read: flat
    /// price, tier schedule (if any), the global price bounds, and the
    /// disabled flag.
    ///
    /// Pure read — no `require_auth`, no pause gate. See
    /// [`ServicePricing`] for field semantics, and
    /// `docs/escrow/pricing.md` for how `tiers` interacts with
    /// `price_stroops` and why `min_bound`/`max_bound` do not constrain
    /// `tiers`.
    pub fn get_service_pricing(env: Env, service_id: Symbol) -> ServicePricing {
        let price_stroops = env
            .storage()
            .persistent()
            .get(&DataKey::ServicePrice(service_id.clone()))
            .unwrap_or(0);
        let tiers = env
            .storage()
            .persistent()
            .get::<DataKey, Vec<PriceTier>>(&DataKey::PriceTiers(service_id.clone()));
        let min_bound = env
            .storage()
            .persistent()
            .get(&DataKey::MinServicePrice)
            .unwrap_or(0);
        let max_bound = env
            .storage()
            .persistent()
            .get(&DataKey::MaxServicePrice)
            .unwrap_or(i128::MAX);
        let disabled = read_flag(&env, &DataKey::ServiceDisabled(service_id));

        ServicePricing {
            price_stroops,
            tiers,
            min_bound,
            max_bound,
            disabled,
        }
    }

    /// Settle the accumulated usage for an `(agent, service_id)` pair.
    ///
    /// Admin-gated. Computes the outstanding bill (same math as
    /// `compute_billing`), resets the usage counter to zero, and returns
    /// the billed amount in stroops. The settlement loop is expected to
    /// transfer the returned amount off-chain or via a paired token
    /// contract call; this contract intentionally holds no balance.
    pub fn settle(
        env: Env,
        caller: Address,
        agent: Address,
        service_id: Symbol,
        expected_version: u64,
    ) -> i128 {
        ensure_not_paused(&env);
        caller.require_auth();
        let admin = get_admin_address(&env);
        require_settlement_authorized(
            &env,
            &admin,
            &caller,
            &service_id,
            EscrowError::NotPendingAdmin,
        );

        let version_key = DataKey::SettlementVersion(agent.clone(), service_id.clone());
        let current_version: u64 = env.storage().persistent().get(&version_key).unwrap_or(0);
        if expected_version != current_version {
            panic_with_error!(&env, EscrowError::VersionConflict);
        }

        let usage_key = DataKey::Usage(agent.clone(), service_id.clone());
        let requests: u32 = env.storage().persistent().get(&usage_key).unwrap_or(0);
        // Use tier schedule when present; fall back to flat price.
        let billed = compute_billing_for_requests(&env, &service_id, requests);
        debit_agent_credit(&env, &agent, billed);
        add_settled_totals(&env, &agent, billed);
        env.storage().persistent().set(&usage_key, &0u32);
        // Prune the service from the agent's index since usage is now zero.
        // This keeps the index consistent with the underlying counters and
        // prevents the index from accumulating services that have been fully
        // settled, which would skew `get_agent_services` results.
        deindex_agent_service(&env, &agent, &service_id);
        env.storage().persistent().set(
            &DataKey::LastSettlement(agent.clone(), service_id.clone()),
            &env.ledger().timestamp(),
        );

        let new_version = current_version.saturating_add(1);
        env.storage().persistent().set(&version_key, &new_version);
        env.events().publish(
            (events::TOPIC_SETTLE_V,),
            (agent.clone(), service_id.clone(), new_version),
        );

        env.events().publish(
            (symbol_short!("settled"),),
            (agent, service_id, requests, billed),
        );
        billed
    }

    /// Settle every outstanding service for an agent in a single call,
    /// returning a `Vec<(Symbol, i128)>` of `(service_id, billed)` pairs —
    /// one entry per service in the agent's active-service index, in
    /// index order.
    ///
    /// Authorization is identical to [`Escrow::settle`]: `caller` must be
    /// either the global admin **or** the `ServiceMetadata.owner` of **every**
    /// service in the index. In practice, only the admin can call
    /// `settle_all` for an agent whose services span multiple owners;
    /// a service owner should use `settle` for their individual service.
    /// Panics with [`EscrowError::Unauthorized`] if the caller does not own the service.
    ///
    /// Bounds: panics with [`EscrowError::SettleAllTooLarge`] when the
    /// stored index exceeds `MAX_SETTLE_ALL`. This should never occur in
    /// normal operation because `record_usage` caps the index at the same
    /// constant, but the guard protects against a future migration that
    /// could write a larger index.
    ///
    /// Each service that has a non-zero usage counter is settled (usage
    /// zeroed, `LastSettlement` stamped, `settled` event emitted) matching
    /// the semantics of a direct `settle` call. Services with zero usage
    /// are still included in the return value (with a billed amount of 0)
    /// so callers can confirm the full sweep. After the sweep, emits one
    /// `settl_all(agent, count, total_billed)` batch-summary event so
    /// indexers can track a full drain without summing the per-service
    /// `settled` events themselves. `count` is the number of services in
    /// the index (including zero-billed ones); `total_billed` is the sum
    /// of every `billed` amount, saturating at `i128::MAX`.
    ///
    /// Honours the pause gate: panics with [`EscrowError::ContractPaused`]
    /// when paused.
    pub fn settle_all(env: Env, caller: Address, agent: Address) -> Vec<(Symbol, i128)> {
        ensure_not_paused(&env);
        caller.require_auth();
        let admin = get_admin_address(&env);

        // Load the agent's active-service index.
        let svc_list: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::AgentServiceIndex(agent.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        // Guard: the index must not exceed MAX_SETTLE_ALL.
        if svc_list.len() > MAX_SETTLE_ALL {
            panic_with_error!(&env, EscrowError::SettleAllTooLarge);
        }

        let now = env.ledger().timestamp();
        let mut results: Vec<(Symbol, i128)> = Vec::new(&env);
        let mut total_billed: i128 = 0;

        for service_id in svc_list.iter() {
            // Non-admin callers must own this specific service.
            require_settlement_authorized(
                &env,
                &admin,
                &caller,
                &service_id,
                EscrowError::Unauthorized,
            );

            let usage_key = DataKey::Usage(agent.clone(), service_id.clone());
            let requests: u32 = env.storage().persistent().get(&usage_key).unwrap_or(0);
            let price: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::ServicePrice(service_id.clone()))
                .unwrap_or(0);
            // saturate: mirrors single-settle semantics.
            let billed = (requests as i128).saturating_mul(price);

            add_settled_totals(&env, &agent, billed);

            // Drain and stamp even when usage is zero (consistent with
            // single-settle: every drain updates LastSettlement).
            env.storage().persistent().set(&usage_key, &0u32);
            env.storage().persistent().set(
                &DataKey::LastSettlement(agent.clone(), service_id.clone()),
                &now,
            );

            // Batch settlement mutates the same per-pair settlement state as
            // `settle`, so it must also invalidate any version read before
            // this sweep.
            let version_key = DataKey::SettlementVersion(agent.clone(), service_id.clone());
            let current_version: u64 = env.storage().persistent().get(&version_key).unwrap_or(0);
            let new_version = current_version.saturating_add(1);
            env.storage().persistent().set(&version_key, &new_version);
            env.events().publish(
                (events::TOPIC_SETTLE_V,),
                (agent.clone(), service_id.clone(), new_version),
            );

            env.events().publish(
                (symbol_short!("settled"),),
                (agent.clone(), service_id.clone(), requests, billed),
            );

            total_billed = total_billed.saturating_add(billed);
            results.push_back((service_id.clone(), billed));
        }

        env.events().publish(
            (symbol_short!("settl_all"),),
            (agent, results.len(), total_billed),
        );

        results
    }

    /// Read the configured per-call floor, or `0` (no floor) when absent.
    pub fn get_min_requests_per_call(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::MinRequestsPerCall)
            .unwrap_or(0)
    }

    /// Set the global minimum and maximum price bounds for `set_service_price`.
    ///
    /// Admin-gated. Both `min_stroops` and `max_stroops` are persisted in
    /// `DataKey::MinServicePrice` / `DataKey::MaxServicePrice`. After this
    /// call, any `set_service_price` invocation with a price outside
    /// `[min_stroops, max_stroops]` is rejected with
    /// [`EscrowError::PriceOutOfBounds`].
    ///
    /// # Default (unbounded) behaviour
    ///
    /// When no bounds have been configured, `set_service_price` applies the
    /// implicit defaults: floor = `0`, ceiling = `i128::MAX`. Calling
    /// `set_price_bounds(0, i128::MAX)` restores these defaults explicitly.
    ///
    /// # Zero-is-free semantics
    ///
    /// A price of `0` means "free service" — usage is still recorded but
    /// settlement bills nothing. **If `min_stroops > 0`, free services are
    /// forbidden**: `set_service_price(svc, 0)` will be rejected with
    /// `PriceOutOfBounds` until the floor is lowered back to `0`. This is
    /// intentional policy: a positive floor expresses that all services in
    /// the band must have a non-zero cost. Admins who want to allow free
    /// services alongside bounded paid services should keep `min_stroops = 0`.
    ///
    /// # Inverted-band rejection
    ///
    /// Panics with [`EscrowError::InvertedPriceBand`] when
    /// `min_stroops > max_stroops` to prevent a logically impossible band
    /// from being stored.
    ///
    /// Emits `bounds_set(min_stroops, max_stroops)` on success.
    pub fn set_price_bounds(env: Env, min_stroops: i128, max_stroops: i128) {
        require_admin(&env);
        if min_stroops > max_stroops {
            panic_with_error!(&env, EscrowError::InvertedPriceBand);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MinServicePrice, &min_stroops);
        env.storage()
            .persistent()
            .set(&DataKey::MaxServicePrice, &max_stroops);
        env.events()
            .publish((symbol_short!("bnd_set"),), (min_stroops, max_stroops));
    }

    /// Read the configured global minimum service price in stroops.
    ///
    /// Returns `0` (no floor) when no bounds have been configured via
    /// [`Escrow::set_price_bounds`].
    pub fn get_min_service_price(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::MinServicePrice)
            .unwrap_or(0)
    }

    /// Read the configured global maximum service price in stroops.
    ///
    /// Returns `i128::MAX` (no ceiling) when no bounds have been configured via
    /// [`Escrow::set_price_bounds`].
    pub fn get_max_service_price(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::MaxServicePrice)
            .unwrap_or(i128::MAX)
    }

    /// Admin enables or disables the agent allowlist gate. While
    /// disabled, `record_usage` does not consult the per-agent entries.
    ///
    /// Emits a `cfg_set` event with data `(allowlist, enabled)` after the
    /// storage write so indexers can observe every toggle on-chain.
    pub fn set_allowlist_enabled(env: Env, enabled: bool) {
        require_admin(&env);
        write_flag(&env, &DataKey::AllowlistEnabled, enabled);
        publish_cfg_event(&env, symbol_short!("allowlist"), enabled);
    }

    /// Read the master allowlist toggle.
    pub fn is_allowlist_enabled(env: Env) -> bool {
        read_flag(&env, &DataKey::AllowlistEnabled)
    }

    /// Read whether an agent is explicitly allowed (false for never-set).
    pub fn is_agent_allowed(env: Env, agent: Address) -> bool {
        read_flag(&env, &DataKey::AgentAllowed(agent))
    }

    /// Admin sets the allowlist status for a specific agent.
    ///
    /// Emits an `agt_alw` event with `(agent, allowed)` after the storage
    /// write so indexers can observe every per-agent allowlist change
    /// on-chain, mirroring the `cfg_set` event already emitted by
    /// [`Self::set_allowlist_enabled`] for the master toggle.
    pub fn set_agent_allowed(env: Env, agent: Address, allowed: bool) {
        require_admin(&env);
        write_flag(&env, &DataKey::AgentAllowed(agent.clone()), allowed);
        env.events()
            .publish((symbol_short!("agt_alw"),), (agent, allowed));
    }

    /// Read whether an agent is on the blocklist (false for never-set).
    pub fn is_agent_blocked(env: Env, agent: Address) -> bool {
        read_flag(&env, &DataKey::AgentBlocked(agent))
    }

    /// Admin sets the blocklist status for a specific agent. A blocked
    /// agent is rejected by `record_usage` with `AgentBlocked`,
    /// independent of the allowlist and taking precedence over it: an
    /// agent that is both allow-listed and blocked is still rejected.
    ///
    /// Emits an `agt_blk` event with `(agent, blocked)` after the storage
    /// write so indexers can observe every per-agent blocklist change
    /// on-chain.
    pub fn set_agent_blocked(env: Env, agent: Address, blocked: bool) {
        require_admin(&env);
        write_flag(&env, &DataKey::AgentBlocked(agent.clone()), blocked);
        env.events()
            .publish((symbol_short!("agt_blk"),), (agent, blocked));
    }

    /// Admin sets the per-call lower bound on `requests` for batched
    /// writes. Pass `0` to disable the floor.
    ///
    /// # Invariant: `min <= max`
    ///
    /// Rejects a `min_requests` that exceeds the currently-stored
    /// `MaxRequestsPerCall` (defaulting to `u32::MAX` when unset) with
    /// [`EscrowError::InvalidRequestBounds`]. This prevents a contradictory
    /// configuration that would make every `record_usage` call unsatisfiable —
    /// no value could pass both the ceiling check (#8) and the floor check (#9)
    /// simultaneously.
    ///
    /// `min == max` (an exact-count requirement) is explicitly allowed:
    /// every `record_usage` call must supply precisely that many requests.
    ///
    /// # Setting order
    ///
    /// When both bounds need to change, set the ceiling first via
    /// [`Self::set_max_requests_per_call`] and then the floor via this
    /// entrypoint. Doing it in the reverse order risks a transient
    /// `InvalidRequestBounds` rejection if the new floor temporarily
    /// exceeds the old ceiling.
    ///
    /// Emits a `cfg_set` event with data `(min_call, min_requests)` after
    /// the storage write so indexers can observe every floor change on-chain.
    pub fn set_min_requests_per_call(env: Env, min_requests: u32) {
        require_admin(&env);
        // Cross-bound guard: reject a floor that exceeds the current ceiling.
        // Default the ceiling to u32::MAX (no cap) when it has never been set,
        // which means any min value is valid against an unset ceiling.
        let current_max: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerCall)
            .unwrap_or(u32::MAX);
        if min_requests > current_max {
            panic_with_error!(&env, EscrowError::InvalidRequestBounds);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MinRequestsPerCall, &min_requests);
        publish_cfg_event(&env, symbol_short!("min_call"), min_requests);
    }

    /// Read the configured per-call cap, or `u32::MAX` (no limit) if
    /// none has been set.
    pub fn get_max_requests_per_call(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerCall)
            .unwrap_or(u32::MAX)
    }

    /// Read the configured per-window request cap, or `0` (limiter
    /// disabled) when unset.
    pub fn get_max_requests_per_window(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::MaxRequestsPerWindow)
            .unwrap_or(0)
    }

    /// Admin sets the per-agent, per-window request cap. The limiter is
    /// active only when both this cap and the window length
    /// ([`Self::set_rate_window_seconds`]) are non-zero. Pass `0` to
    /// disable.
    ///
    /// Emits a `cfg_set` event with data `(max_win, max_requests)` after
    /// the storage write so indexers can observe every cap change on-chain.
    pub fn set_max_requests_per_window(env: Env, max_requests: u32) {
        require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::MaxRequestsPerWindow, &max_requests);
        publish_cfg_event(&env, symbol_short!("max_win"), max_requests);
    }

    /// Read the configured rate-limit window length in seconds, or `0`
    /// (limiter disabled) when unset.
    pub fn get_rate_window_seconds(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::WindowSeconds)
            .unwrap_or(0)
    }

    /// Admin sets the fixed rate-limit window length in seconds. The
    /// limiter is active only when both this and the per-window cap are
    /// non-zero. Pass `0` to disable.
    ///
    /// Emits a `cfg_set` event with data `(win_sec, window_seconds)` after
    /// the storage write so indexers can observe every window-duration change
    /// on-chain.
    pub fn set_rate_window_seconds(env: Env, window_seconds: u64) {
        require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::WindowSeconds, &window_seconds);
        publish_cfg_event(&env, symbol_short!("win_sec"), window_seconds);
    }

    /// Admin-gated, pause-respecting entrypoint that clears the per-agent
    /// rate-limit window state.
    ///
    /// Removes the `DataKey::RateWindow(agent)` storage slot so the next
    /// `record_usage` call for this agent opens a fresh window with a zero
    /// count. This lets an operator lift a throttle immediately — for
    /// example, when a misconfigured cap has been raised, or a legitimate
    /// burst the operator wants to forgive.
    ///
    /// Idempotent: resetting an agent that has no stored rate window is a
    /// no-op. The configured cap (`MaxRequestsPerWindow`) and window length
    /// (`WindowSeconds`) are **not** changed — only the agent's accumulated
    /// count for the current window is cleared.
    ///
    /// # Events
    ///
    /// Emits `rate_rst(agent)` so the override is auditable.
    pub fn reset_rate_window(env: Env, agent: Address) {
        ensure_not_paused(&env);
        require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::RateWindow(agent.clone()));
        env.events().publish((symbol_short!("rate_rst"),), agent);
    }

    /// Read the configured usage-alert threshold, or `0` (alerting
    /// disabled) when unset. See [`Self::set_usage_alert_threshold`] for
    /// what crossing this value does.
    pub fn get_usage_alert_threshold(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::UsageAlertThreshold)
            .unwrap_or(0)
    }

    /// Admin sets the global usage-alert threshold consulted by
    /// `record_usage`. Pass `0` to disable alerting (the default).
    ///
    /// `record_usage` emits a `usage_hi(agent, service_id, total)` event
    /// the first time a `(agent, service_id)` pair's accumulated usage
    /// crosses this value from below to at/above it (edge-triggered — it
    /// does not re-fire on every subsequent call while already above the
    /// threshold, and re-arms after `settle` drains the pair below it
    /// again). See `docs/escrow/events.md` for the full edge-trigger
    /// semantics.
    ///
    /// Before this entrypoint existed, `UsageAlertThreshold` had no
    /// setter, so the `usage_hi` path could never fire on a live deploy —
    /// this closes that gap.
    ///
    /// Emits a `cfg_set` event with data `(alert_thr, threshold)` after the
    /// storage write, consistent with every other scalar admin setting.
    /// (The tag is `alert_thr`, deliberately distinct from the `usage_hi`
    /// event topic that firing this threshold later triggers, so the two
    /// are never conflated by a listener.)
    pub fn set_usage_alert_threshold(env: Env, threshold: u32) {
        require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::UsageAlertThreshold, &threshold);
        env.events().publish(
            (symbol_short!("cfg_set"),),
            (symbol_short!("alert_thr"), threshold),
        );
    }

    /// Admin sets the per-call upper bound on `requests` accepted by
    /// `record_usage`. Pass `u32::MAX` to effectively disable the cap.
    ///
    /// # Invariant: `min <= max`
    ///
    /// Rejects a `max_requests` that is below the currently-stored
    /// `MinRequestsPerCall` (defaulting to `0` when unset) with
    /// [`EscrowError::InvalidRequestBounds`]. This prevents a contradictory
    /// configuration that would make every `record_usage` call unsatisfiable —
    /// no value could pass both the ceiling check (#8) and the floor check (#9)
    /// simultaneously.
    ///
    /// `max == min` (an exact-count requirement) is explicitly allowed:
    /// every `record_usage` call must supply precisely that many requests.
    ///
    /// # Setting order
    ///
    /// When both bounds need to change, set the ceiling first via this
    /// entrypoint and then the floor via
    /// [`Self::set_min_requests_per_call`]. Doing it in the reverse order
    /// risks a transient `InvalidRequestBounds` rejection if the new floor
    /// temporarily exceeds the old ceiling.
    ///
    /// Emits a `cfg_set` event with data `(max_call, max_requests)` after
    /// the storage write so indexers can observe every cap change on-chain.
    pub fn set_max_requests_per_call(env: Env, max_requests: u32) {
        require_admin(&env);
        // Cross-bound guard: reject a ceiling that falls below the current floor.
        // Default the floor to 0 (no floor) when it has never been set, which
        // means any max value is valid against an unset floor.
        let current_min: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::MinRequestsPerCall)
            .unwrap_or(0);
        if max_requests < current_min {
            panic_with_error!(&env, EscrowError::InvalidRequestBounds);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MaxRequestsPerCall, &max_requests);
        publish_cfg_event(&env, symbol_short!("max_call"), max_requests);
    }

    /// Admin toggles strict-registration mode. When enabled,
    /// `record_usage` rejects unknown services with
    /// EscrowError::ServiceNotRegistered.
    ///
    /// Emits a `cfg_set` event with data `(req_reg, required)` after the
    /// storage write so indexers can observe every toggle on-chain.
    pub fn set_require_service_registration(env: Env, required: bool) {
        require_admin(&env);
        write_flag(&env, &DataKey::RequireServiceRegistration, required);
        publish_cfg_event(&env, symbol_short!("req_reg"), required);
    }

    /// Read the strict-registration flag.
    pub fn is_service_registration_required(env: Env) -> bool {
        read_flag(&env, &DataKey::RequireServiceRegistration)
    }

    /// Read whether a service has been registered.
    pub fn is_service_registered(env: Env, service_id: Symbol) -> bool {
        read_flag(&env, &DataKey::ServiceRegistered(service_id))
    }

    /// Unregister a service. Admin-gated; idempotent (removing an absent
    /// entry is a no-op). Existing usage records and prices for the
    /// service are NOT touched — call reset_usage or remove the price
    /// separately if a clean wipe is required.
    pub fn unregister_service(env: Env, service_id: Symbol) {
        require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::ServiceRegistered(service_id.clone()));
        // Emit svc_rm so indexers can observe the deregistration without
        // polling ServiceRegistered storage directly.
        env.events().publish((symbol_short!("svc_rm"),), service_id);
    }

    /// Register a service so `record_usage` accepts it under strict
    /// registration. Admin-gated and idempotent.
    pub fn register_service(env: Env, service_id: Symbol) {
        require_admin(&env);
        write_flag(&env, &DataKey::ServiceRegistered(service_id.clone()), true);
        // Emit svc_add so indexers observe plain registrations (no metadata)
        // without having to infer the state change from absent events.
        env.events()
            .publish((symbol_short!("svc_add"),), service_id);
    }

    /// Atomically register a service AND set its metadata in one
    /// admin-gated, pause-respecting call.
    ///
    /// Sets `ServiceRegistered(service_id) = true` and persists the
    /// provided `ServiceMetadata`.  Emits `svc_reg(service_id, owner)`
    /// so indexers can observe the combined registration+metadata event
    /// in a single topic.
    ///
    /// Idempotent — re-registering an existing id overwrites its
    /// metadata.  An empty `description` is accepted.  Extends the
    /// metadata entry's persistent TTL on write.
    pub fn register_service_with_metadata(
        env: Env,
        service_id: Symbol,
        description: String,
        owner: Address,
    ) {
        ensure_not_paused(&env);
        require_admin(&env);
        write_flag(&env, &DataKey::ServiceRegistered(service_id.clone()), true);
        env.storage().persistent().set(
            &DataKey::ServiceMetadata(service_id.clone()),
            &ServiceMetadata {
                description,
                owner: owner.clone(),
            },
        );
        bump_persistent(&env, &DataKey::ServiceMetadata(service_id.clone()));
        env.events()
            .publish((symbol_short!("svc_reg"),), (service_id, owner));
    }

    /// Cancel a pending admin transfer. Current admin only. No-op when
    /// nothing is pending.
    ///
    /// Emits an `admin_can` event with `(admin, cancelled)` after the
    /// storage write, where `cancelled` is the pending address that was
    /// cleared (`None` when the call was a no-op) so indexers can observe
    /// every cancellation on-chain, including no-op ones.
    pub fn cancel_admin_transfer(env: Env) {
        let admin = require_admin(&env);
        let cancelled: Option<Address> = env.storage().persistent().get(&DataKey::PendingAdmin);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("admin_can"),), (admin, cancelled));
    }

    /// Read the pending admin, if any.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::PendingAdmin)
    }

    /// Return the current admin and any pending handover in a single read.
    ///
    /// Pure read — no `require_auth`, no pause gate. Equivalent to calling
    /// `get_admin` and `get_pending_admin` separately; this is a convenience
    /// snapshot only, for callers (dashboards, migration tooling) that want
    /// both in one round trip.
    pub fn get_admin_summary(env: Env) -> AdminSummary {
        AdminSummary {
            admin: Self::get_admin(env.clone()),
            pending_admin: Self::get_pending_admin(env),
        }
    }

    /// Step 2 of admin handover. The pending admin (set by step 1)
    /// claims the role; this proves they control the key. Panics with
    /// NoPendingAdminTransfer if none is pending, or NotPendingAdmin
    /// if the caller does not match the pending entry. On success, emits
    /// `admin_chg(old_admin, new_admin)` so indexers can track admin
    /// rotations without polling `get_admin`.
    pub fn accept_admin_transfer(env: Env, caller: Address) {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, EscrowError::NoPendingAdminTransfer));
        if pending != caller {
            panic_with_error!(&env, EscrowError::NotPendingAdmin);
        }
        let old_admin = get_admin_address(&env);
        env.storage().persistent().set(&DataKey::Admin, &caller);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("admin_chg"),), (old_admin, caller));
    }

    /// Step 1 of admin handover. Current admin proposes a new admin
    /// address; the new admin must then call `accept_admin_transfer`
    /// from their own key to finish the rotation. Re-proposing
    /// overwrites the prior pending entry.
    ///
    /// Emits an `admin_prp` event with `(admin, new_admin)` after the
    /// storage write so indexers can observe every proposal — including a
    /// re-proposal that overwrites a still-pending entry — without polling
    /// `get_pending_admin`.
    pub fn propose_admin_transfer(env: Env, new_admin: Address) {
        let admin = require_admin(&env);
        if new_admin == admin {
            panic_with_error!(&env, EscrowError::InvalidAdminProposal);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((symbol_short!("admin_prp"),), (admin, new_admin));
    }

    /// Returns `true` iff the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        read_flag(&env, &DataKey::Paused)
    }

    /// Resume operations after a previous `pause()`. Admin-gated and
    /// idempotent (unpausing an already-unpaused contract is a no-op).
    pub fn unpause(env: Env) {
        require_admin(&env);
        write_flag(&env, &DataKey::Paused, false);
        env.events().publish((symbol_short!("paused"),), false);
    }

    /// Pause the contract — every state-changing entrypoint will then
    /// panic with [`EscrowError::ContractPaused`]. Admin-gated and
    /// idempotent (pausing an already-paused contract is a no-op write).
    pub fn pause(env: Env) {
        require_admin(&env);
        write_flag(&env, &DataKey::Paused, true);
        env.events().publish((symbol_short!("paused"),), true);
    }

    /// Migrate the persisted schema from v1 to v2. Admin-gated and
    /// idempotent in shape — but panics with `MigrationVersionMismatch`
    /// if the current schema is already at v2 (or higher), to surface
    /// accidental double-runs. All v2 reads default sensibly when their
    /// new slots are absent, so the migration body itself only stamps
    /// the new SchemaVersion; no data fan-out is required.
    pub fn migrate_v1_to_v2(env: Env) {
        require_admin(&env);
        let current: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1);
        if current != 1 {
            panic_with_error!(&env, EscrowError::MigrationVersionMismatch);
        }
        env.storage()
            .persistent()
            .set(&DataKey::SchemaVersion, &2u32);
    }

    /// Read the metadata for a service, or `None` if none has been set.
    /// Extends the entry's persistent TTL on read.
    pub fn get_service_metadata(env: Env, service_id: Symbol) -> Option<ServiceMetadata> {
        let key = DataKey::ServiceMetadata(service_id);
        let result: Option<ServiceMetadata> = env.storage().persistent().get(&key);
        bump_persistent(&env, &key);
        result
    }

    /// Returns `true` iff the service has been disabled.
    pub fn is_service_disabled(env: Env, service_id: Symbol) -> bool {
        read_flag(&env, &DataKey::ServiceDisabled(service_id))
    }

    /// Admin sets the disabled flag for a service. Disabling a service
    /// causes `record_usage` to panic with `ServiceDisabled` for that
    /// id; registration and metadata are preserved.
    /// Emits `svc_dis(service_id, disabled)` so indexers observe both
    /// disable and re-enable transitions from a single event topic.
    pub fn set_service_disabled(env: Env, service_id: Symbol, disabled: bool) {
        require_admin(&env);
        write_flag(
            &env,
            &DataKey::ServiceDisabled(service_id.clone()),
            disabled,
        );
        env.events()
            .publish((symbol_short!("svc_dis"),), (service_id, disabled));
    }

    /// Admin sets human-readable metadata for a service. Persisted
    /// under `DataKey::ServiceMetadata(service_id)`. Description is
    /// capped at 256 UTF-8 bytes to bound storage cost.  Extends the
    /// entry's persistent TTL on write.
    /// Emits `meta_set(service_id, owner)` so indexers observe metadata
    /// writes without polling storage.
    pub fn set_service_metadata(env: Env, service_id: Symbol, description: String, owner: Address) {
        require_admin(&env);
        env.storage().persistent().set(
            &DataKey::ServiceMetadata(service_id.clone()),
            &ServiceMetadata {
                description,
                owner: owner.clone(),
            },
        );
        bump_persistent(&env, &DataKey::ServiceMetadata(service_id.clone()));
        env.events()
            .publish((symbol_short!("meta_set"),), (service_id, owner));
    }

    /// Transfer ownership of a service's metadata to `new_owner`,
    /// preserving the existing `description`. Authorised by `caller`,
    /// which must be the current owner OR the admin. Panics with
    /// `ServiceMetadataNotFound` if no metadata has been set,
    /// [`EscrowError::InvalidOwnerTransfer`] if `new_owner` matches the
    /// current owner, or [`EscrowError::Unauthorized`] if the caller is
    /// not the owner or admin.
    /// Emits `owner_chg(service_id, old_owner, new_owner)` for indexers
    /// only on genuine transfers (no-op self-transfers are rejected
    /// before any storage write or event emission).  Extends the
    /// metadata entry's persistent TTL on write.
    /// Honours the pause gate.
    pub fn transfer_service_ownership(
        env: Env,
        caller: Address,
        service_id: Symbol,
        new_owner: Address,
    ) {
        ensure_not_paused(&env);
        caller.require_auth();
        let admin = get_admin_address(&env);
        let mut meta: ServiceMetadata = env
            .storage()
            .persistent()
            .get(&DataKey::ServiceMetadata(service_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, EscrowError::ServiceMetadataNotFound));
        if !is_owner_or_admin(&admin, &caller, &meta.owner) {
            panic_with_error!(&env, EscrowError::Unauthorized);
        }
        // Reject a no-op transfer to the current owner. Mirrors the
        // `InvalidAdminProposal` guard on `propose_admin_transfer`:
        // skipping the guard would waste a storage write and emit an
        // `owner_chg` event whose old_owner == new_owner, misleading
        // indexers into reporting a meaningful handover.
        if new_owner == meta.owner {
            panic_with_error!(&env, EscrowError::InvalidOwnerTransfer);
        }
        let old_owner = meta.owner.clone();
        meta.owner = new_owner.clone();
        env.storage()
            .persistent()
            .set(&DataKey::ServiceMetadata(service_id.clone()), &meta);
        bump_persistent(&env, &DataKey::ServiceMetadata(service_id.clone()));
        env.events().publish(
            (symbol_short!("owner_chg"),),
            (service_id, old_owner, new_owner),
        );
    }

    /// Admin-gated. Remove a service's metadata (description + owner).
    /// Idempotent — clearing an absent entry is a no-op. After clearing,
    /// `get_service_metadata` reads back `None`. Registration and usage
    /// history live in independent slots and are untouched. Emits
    /// `meta_clr(service_id)` (topic shortened to satisfy the 9-char
    /// `symbol_short!` limit).  The entry is deleted, so no TTL
    /// extension is performed.
    pub fn clear_service_metadata(env: Env, service_id: Symbol) {
        require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::ServiceMetadata(service_id.clone()));
        env.events()
            .publish((symbol_short!("meta_clr"),), service_id);
    }

    /// Read the on-chain schema version, or `1` (the implicit
    /// pre-migration default) if absent.
    pub fn get_schema_version(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1)
    }

    /// Return all global contract settings in a single read.
    ///
    /// Pure read — no `require_auth`, no pause gate. Values are identical to
    /// what the individual getters return for the same storage state:
    /// `is_paused`, `is_allowlist_enabled`, `is_service_registration_required`,
    /// `get_max_requests_per_call`, `get_min_requests_per_call`,
    /// `get_max_requests_per_window`, `get_rate_window_seconds`,
    /// `get_schema_version`, and `get_admin`. The per-field getters remain
    /// available; this is a convenience snapshot only.
    pub fn get_contract_config(env: Env) -> ContractConfig {
        ContractConfig {
            paused: Self::is_paused(env.clone()),
            allowlist_enabled: Self::is_allowlist_enabled(env.clone()),
            require_service_registration: Self::is_service_registration_required(env.clone()),
            max_requests_per_call: Self::get_max_requests_per_call(env.clone()),
            min_requests_per_call: Self::get_min_requests_per_call(env.clone()),
            max_requests_per_window: Self::get_max_requests_per_window(env.clone()),
            window_seconds: Self::get_rate_window_seconds(env.clone()),
            schema_version: Self::get_schema_version(env.clone()),
            admin: Self::get_admin(env),
        }
    }

    /// Get the version of the contract for compatibility checks.
    ///
    /// v2 adds pause/unpause, two-step admin handover, service registry,
    /// per-call min/max bounds, an agent allowlist, lifetime usage
    /// counters, settlement-time tracking, and a stored schema version.
    pub fn version(env: Env) -> u32 {
        let _ = env;
        2
    }

    /// Open a dispute for an `(agent, service_id)` pair.
    ///
    /// Any caller may contest a charge by flagging the pair; the agent
    /// does not need admin rights to initiate a dispute. Panics with
    /// [`EscrowError::DisputeAlreadyOpen`] when a dispute is already open
    /// for this pair — callers should check [`Escrow::has_open_dispute`]
    /// first to avoid a wasted call. Honours the pause gate and emits a
    /// `dispute` event with `("open", agent, service_id)`.
    ///
    /// Dispute lifecycle:
    /// 1. `open_dispute` — agent/caller flags the pair. Note: `settle` does
    ///    **not** check this flag and will still drain and bill the pair
    ///    while a dispute is open; the flag is advisory for off-chain
    ///    tooling only (see `docs/escrow/storage.md`).
    /// 2. `resolve_dispute` (admin only) — admin subtracts contested usage
    ///    (or zero for no refund) and clears the flag; `settle` unblocks.
    pub fn open_dispute(env: Env, agent: Address, service_id: Symbol) {
        ensure_not_paused(&env);
        agent.require_auth();
        let key = DataKey::Dispute(agent.clone(), service_id.clone());
        if read_flag(&env, &key) {
            panic_with_error!(&env, EscrowError::DisputeAlreadyOpen);
        }
        write_flag(&env, &key, true);
        env.events().publish(
            (events::TOPIC_DISPUTE,),
            (events::TOPIC_OPEN, agent, service_id),
        );
    }

    /// Return the service ids for an agent that currently have open disputes.
    ///
    /// Pure read — no auth, no pause gate. The method reuses the same
    /// per-agent service index backing as [`Escrow::get_agent_services`] so it
    /// iterates the active service list in the same order and stops after
    /// [`MAX_BATCH_READ`] entries to keep the read bounded.
    pub fn list_open_disputes(env: Env, agent: Address) -> Vec<Symbol> {
        let index: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::AgentServiceIndex(agent.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        let mut disputes: Vec<Symbol> = Vec::new(&env);
        for service_id in index.iter() {
            if disputes.len() >= MAX_BATCH_READ {
                break;
            }
            if read_flag(&env, &DataKey::Dispute(agent.clone(), service_id.clone())) {
                disputes.push_back(service_id);
            }
        }
        disputes
    }

    /// Returns `true` iff there is currently an open dispute for the
    /// given `(agent, service_id)` pair. Pure read — no auth, no pause gate.
    pub fn has_open_dispute(env: Env, agent: Address, service_id: Symbol) -> bool {
        read_flag(&env, &DataKey::Dispute(agent, service_id))
    }

    /// Admin-only: resolve a dispute for an `(agent, service_id)` pair.
    ///
    /// Subtracts `refund_requests` from the accumulated usage counter
    /// (clamping at zero), then clears the dispute flag so `settle` can
    /// proceed. Panics with:
    /// - [`EscrowError::NoOpenDispute`] when no dispute is open for the pair.
    /// - [`EscrowError::RefundExceedsUsage`] when `refund_requests` exceeds
    ///   the current usage (prevents double-refunds and negative counters).
    ///
    /// Pass `refund_requests = 0` to acknowledge and dismiss the dispute
    /// without adjusting usage. Honours the pause gate and emits a
    /// `dispute` event with `("resolve", agent, service_id, refund_requests)`.
    ///
    /// Security notes:
    /// - Admin-gated: agents cannot self-resolve (`admin.require_auth()`).
    /// - No double-refund: `RefundExceedsUsage` enforces `refund <= usage`.
    /// - Dispute must be open: `NoOpenDispute` prevents spurious calls.
    pub fn resolve_dispute(env: Env, agent: Address, service_id: Symbol, refund_requests: u32) {
        ensure_not_paused(&env);
        require_admin(&env);
        let dispute_key = DataKey::Dispute(agent.clone(), service_id.clone());
        if !read_flag(&env, &dispute_key) {
            panic_with_error!(&env, EscrowError::NoOpenDispute);
        }
        if refund_requests > 0 {
            let usage_key = DataKey::Usage(agent.clone(), service_id.clone());
            let current: u32 = env.storage().persistent().get(&usage_key).unwrap_or(0);
            if refund_requests > current {
                panic_with_error!(&env, EscrowError::RefundExceedsUsage);
            }
            env.storage()
                .persistent()
                .set(&usage_key, &(current - refund_requests));
        }
        // Clear the dispute flag so settle can proceed.
        write_flag(&env, &dispute_key, false);
        env.events().publish(
            (events::TOPIC_DISPUTE,),
            (events::TOPIC_RESOLVE, agent, service_id, refund_requests),
        );
    }

    /// Admin-only batch entrypoint that resolves disputes for multiple
    /// services of one agent, zeroing the full disputed usage in a single
    /// transaction.
    ///
    /// Accepts a bounded `Vec<Symbol>` of service ids. For each service:
    /// - Skips the service if no dispute is open (no-op).
    /// - Reads the current accumulated usage and uses that as the refund
    ///   amount (the counter is then reset to zero).
    /// - Clears the dispute flag.
    /// - Emits a `dispute` event with
    ///   `("resolve", agent, service_id, refunded)`.
    ///
    /// Rejects oversized batches (more than [`MAX_BATCH_READ`] entries) with
    /// [`EscrowError::BatchTooLarge`]. Honours the pause gate.
    pub fn refund_batch(env: Env, agent: Address, services: Vec<Symbol>) {
        ensure_not_paused(&env);
        require_admin(&env);
        if services.len() > MAX_BATCH_READ {
            panic_with_error!(&env, EscrowError::BatchTooLarge);
        }
        for service_id in services.iter() {
            let dispute_key = DataKey::Dispute(agent.clone(), service_id.clone());
            if !read_flag(&env, &dispute_key) {
                continue;
            }
            let usage_key = DataKey::Usage(agent.clone(), service_id.clone());
            let current: u32 = env.storage().persistent().get(&usage_key).unwrap_or(0);
            // Zero the usage counter.
            env.storage().persistent().set(&usage_key, &0u32);
            // Clear the dispute flag so settle can proceed.
            write_flag(&env, &dispute_key, false);
            env.events().publish(
                (events::TOPIC_DISPUTE,),
                (events::TOPIC_RESOLVE, agent.clone(), service_id.clone(), current),
            );
        }
    }
}

#[cfg(test)]
mod test;
