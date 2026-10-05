# Contributing to AgentPay Contracts

Thanks for contributing! This guide documents the conventions that keep the
`escrow` contract's on-chain interface stable for downstream client SDKs.
Please read it before opening a pull request that touches contract code.

## The CI gate

Every PR must pass the same checks CI runs, from the workspace root:

```bash
cargo fmt --all -- --check   # formatting (no diff allowed)
cargo build                  # compiles clean
cargo test                   # all unit tests green
```

A PR that does not pass all three locally will not pass CI and cannot be
merged. Run them before pushing.

### Coverage and campaign expectations

- **95% test coverage** is the target for contract logic. New entrypoints and
  new branches (error paths, idempotency, slot independence) should ship with
  tests in `contracts/escrow/src/test.rs`.
- Contributions are organized as a **96-hour campaign**: scope your PRs so
  each one addresses a single issue and can be reviewed and merged within the
  campaign window. Keep PRs focused — one issue per branch.

## Error codes are append-only

`EscrowError` (in `contracts/escrow/src/lib.rs`) is annotated with
`#[contracterror]` and `#[repr(u32)]`. The numeric codes are part of the
contract's public ABI: client SDKs match on them, so they are **append-only**.

**Rules:**

- **Never renumber** an existing variant.
- **Never reuse** a retired code for a different meaning.
- Add new variants only at the **end**, with the next unused integer.
- Removing a variant is a breaking change; prefer deprecating it in docs and
  leaving the code permanently reserved.

### Current code table

| Code | Variant | Meaning |
|-----:|---------|---------|
| 1 | `AlreadyInitialized` | `init` was already called and the admin is stored. |
| 2 | `RequestsMustBePositive` | `record_usage` called with `requests == 0` (also reused for a negative price in `set_service_price`). |
| 3 | `NotInitialized` | Admin-gated entrypoint invoked before `init`. |
| 4 | `ContractPaused` | State-changing entrypoint called while `Paused` is `true`. |
| 5 | `NoPendingAdminTransfer` | `accept_admin_transfer` called but no pending admin is set. |
| 6 | `NotPendingAdmin` | `accept_admin_transfer` called by the wrong address. |
| 7 | `ServiceNotRegistered` | `record_usage` referenced an unregistered service while strict registration is enabled. |
| 8 | `RequestsExceedsMaxPerCall` | `record_usage` exceeded `MaxRequestsPerCall` cap. |
| 9 | `RequestsBelowMinPerCall` | `record_usage` below `MinRequestsPerCall` floor. |
| 10 | `AgentNotAllowed` | `record_usage` for an agent not on the allowlist while allowlisting is enabled. |
| 11 | `MigrationVersionMismatch` | `migrate_v1_to_v2` called on a non-v1 schema. |
| 12 | `ServiceDisabled` | `record_usage` referenced a disabled service. |
| 13 | `ServiceMetadataNotFound` | A metadata-scoped entrypoint referenced a service with no `ServiceMetadata` set. |
| 14 | `InvalidAdminProposal` | `propose_admin_transfer` called with the current admin as the proposed new admin. |
| 15 | `RateLimitExceeded` | `record_usage` would push an agent's per-window count above `MaxRequestsPerWindow`. |
| 16 | `BatchTooLarge` | `get_usage_batch` called with more than `MAX_BATCH_READ` pairs. |
| 17 | `AgentBlocked` | `record_usage` for an agent on the per-agent blocklist. |
| 18 | `InvalidPriceTiers` | Malformed tier schedule passed to `set_price_tiers`. |
| 19 | `SettleAllTooLarge` | `settle_all` agent service index exceeds `MAX_SETTLE_ALL`. |
| 20 | `DisputeAlreadyOpen` | `open_dispute` called but a dispute is already open for the pair. |
| 21 | `NoOpenDispute` | `resolve_dispute` called but no dispute is open. |
| 22 | `RefundExceedsUsage` | `resolve_dispute` `refund_requests` exceeds current usage. |
| 23 | `InvalidRequestBounds` | Minimum request count exceeds the configured maximum, or vice versa. |
| 24 | `PriceOutOfBounds` | Service price is outside the configured bounds. |
| 25 | `InvertedPriceBand` | Minimum price exceeds maximum price. |
| 26 | `Unauthorized` | Authenticated caller lacks contract-domain authority, including settlement by a non-owner. |
| 27 | `InvalidOwnerTransfer` | Proposed owner is already the service owner. |
| 28 | `InsufficientCreditBalance` | Projected usage bill exceeds positive prepaid credit. |

The next new error must use code **29**.

Settlement rejects an authenticated caller who is neither admin nor service
owner with #26 `Unauthorized`. Code #6 `NotPendingAdmin` is reserved for
admin handover. Assert the exact contract error in negative-path tests.

See the [full error reference](docs/escrow/errors.md) for trigger conditions,
entrypoints, and notes on overloaded codes.

## Event conventions

### Topic names use `symbol_short!` (≤ 9 characters)

Event topics are published with `symbol_short!`, which only accepts symbols of
**9 characters or fewer**. Longer literals fail to compile. Keep topic names
short and stable.

### Events are additive-only

Like error codes, the event surface is consumed off-chain (indexers,
dashboards, settlement loops). Treat it as **additive-only**:

- Do not rename an existing topic.
- Do not change the shape, order, or types of an existing event's payload.
- New information goes into a **new** event, not an altered existing one.

### Current event reference

| Topic | Payload | Emitted by |
|-------|---------|------------|
| `usage` | `(agent, service_id, requests, total)` | `record_usage` |
| `usage_hi` | `(agent, service_id, total)` | `record_usage` (edge-triggered when crossing threshold) |
| `usage_dec` | `(agent, service_id, amount, new_total)` | `decrement_usage` |
| `settled` | `(agent, service_id, requests, billed)` | `settle`, `settle_all` |
| `price_set` | `(service_id, price_stroops)` | `set_service_price` |
| `price_rm` | `(service_id)` | `remove_service_price` |
| `tiers_set` | `(service_id)` | `set_price_tiers` |
| `tiers_rm` | `(service_id)` | `remove_price_tiers` |
| `cfg_set` | `(config_key, value)` | `set_max_requests_per_call`, `set_min_requests_per_call`, `set_max_requests_per_window`, `set_rate_window_seconds` |
| `svc_reg` | `(service_id, owner)` | `register_service_with_metadata` |
| `paused` | `(bool)` | `pause`, `unpause` |
| `owner_chg` | `(service_id, old_owner, new_owner)` | `transfer_service_ownership` |
| `meta_clr` | `(service_id)` | `clear_service_metadata` |
| `dispute` | `(Symbol, agent, service_id[, refund_requests])` | `open_dispute`, `resolve_dispute` |

The `cfg_set` topic uses a secondary symbol to identify the config key: `max_call`, `min_call`, `max_win`, or `win_sec`.

The `dispute` topic uses a primary action symbol: `Symbol::new(&env, "open")` or `Symbol::new(&env, "resolve")` (these are not constrained to 9 chars because they are payload data, not topic keys).

## Getter-default convention: `unwrap_or`

Read-only getters return a sensible **default** for absent storage rather than
panicking, using `unwrap_or(...)`. This keeps clients from having to special-case
never-written slots. Examples:

- `get_usage` / `get_service_price` / `get_total_usage_by_agent` / `get_total_settled_by_agent` → `0`
- `get_max_requests_per_call` → `u32::MAX` (no cap)
- `get_max_requests_per_window` / `get_min_requests_per_call` / `get_rate_window_seconds` → `0`
- `is_paused` / `is_service_registered` / `is_service_disabled` / `is_agent_allowed` / `is_agent_blocked` / `is_allowlist_enabled` / `is_service_registration_required` → `false`
- `get_schema_version` → `1` (the implicit pre-migration default)

When the **absence** of a value is itself meaningful (e.g. "never settled" vs.
"settled at genesis"), return `Option<T>` instead — see `get_last_settlement`,
`get_service_metadata`, `get_price_tiers`, `get_admin`, and `get_pending_admin`,
which return `None`.

Boolean flags use a shared `read_flag` / `write_flag` pair that centralises
the `unwrap_or(false)` convention (`lib.rs:275-288`).

## Test conventions

### Panic assertions for typed errors

Tests that exercise an error path assert the exact contract error code with
`#[should_panic]` using the host's error-formatting string:

```rust
#[test]
#[should_panic(expected = "Error(Contract, #N)")]
fn test_some_rejection() {
    // ... trigger the panic_with_error! path ...
}
```

Substitute `N` with the numeric code from the table above (for example,
`Error(Contract, #4)` for `ContractPaused`). This pins the test to the specific
error variant, so an accidental renumbering would fail the suite.

### Event and state assertions

`env.events().all()` only surfaces events from the **most recent** contract
invocation, so read events immediately after the call under test, before any
other contract call (including read-only getters). Compare topics against a
`Vec<Val>` built with `.into_val(&env)`, and decode payload data back into typed
tuples with `data.into_val(&env)`. When exact event matching is awkward, fall
back to asserting observable state plus that the event count increased.

### Snapshot tests

Tests with complex post-state (e.g. biling computation, allowlist interactions)
use `assert_snapshot!` from the `soroban_sdk` test utilities. Snapshots are
stored under `contracts/escrow/test_snapshots/` and must be regenerated with
`cargo test` when contract output changes intentionally. Commit updated
snapshots alongside the code change.

## Event-topic naming rule

All event topics MUST fit within the `symbol_short!` limit. When designing a
new event, confirm the topic name is ≤ 9 ASCII characters. If the natural name
is too long, abbreviate (e.g. `meta_clr` for "metadata clear", `owner_chg` for
"owner changed").

## Pull request checklist

- [ ] One issue per branch; branch from `main`.
- [ ] `cargo fmt --all -- --check`, `cargo build`, `cargo test` all pass.
- [ ] New / changed behavior is covered by tests (aim for 95% coverage).
- [ ] No renumbered/reused error codes; new codes appended only.
- [ ] No renamed/reshaped existing events; new info in new events.
- [ ] Getters default via `unwrap_or` (or return `Option` when absence matters).
- [ ] New events use `symbol_short!` with a ≤9 character topic name.
