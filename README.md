# AgentPay Contracts

[![CI](https://github.com/Agentpay-Org/Agentpay-contracts/actions/workflows/ci.yml/badge.svg)](https://github.com/Agentpay-Org/Agentpay-contracts/actions/workflows/ci.yml)

Soroban smart contracts for the AgentPay protocol: escrow, usage recording, and payment settlement on Stellar.

## CI

Every push and pull request runs the following gates automatically:

| Step       | Command                                                 |
| ---------- | ------------------------------------------------------- |
| Formatting | `cargo fmt --all -- --check`                            |
| Linting    | `cargo clippy --all-targets -- -D warnings`             |
| Build      | `cargo build`                                           |
| Tests      | `cargo test`                                            |
| Wasm build | `cargo build --target wasm32-unknown-unknown --release` |

The Rust toolchain is pinned via `rust-toolchain.toml` (stable channel with `wasm32-unknown-unknown` target). Cargo registry and build artefacts are cached between runs to keep CI fast.

## Overview

- **escrow** — Records usage and supports settlement logic for machine-to-machine payments.

## Documentation

- [CHANGELOG](CHANGELOG.md) — versioned history of entrypoints, events, and error codes; contribution conventions.
- [EscrowError code table](docs/escrow/errors.md) — full reference for all 28 error codes: trigger conditions, overloaded codes, and the entrypoints that raise each code.
- [Escrow: Pricing Model](docs/escrow/pricing.md) — flat-rate vs. tiered volume-discount billing, tier-boundary semantics with worked examples, and the global price-bounds invariants.

### Service ownership handover

A service's `ServiceMetadata` carries a `description` and an `owner`. The
current owner (or the admin) can reassign the `owner` via
`transfer_service_ownership(caller, service_id, new_owner)` without touching the
`description`. The call honours the pause gate and emits `owner_chg` for
indexers.

### Service metadata vs. registration

A service's metadata (`description` + `owner`) and its registration flag live in
independent storage slots. `clear_service_metadata` (admin-gated, idempotent)
removes only the metadata; the registration flag and per-(agent, service) usage
history are untouched.

### Service price removal: zero vs. removed

`set_service_price(service_id, 0)` and `remove_service_price(service_id)` both
make `get_service_price(service_id)` and `compute_billing(agent, service_id)`
read back `0`, but they are not identical:

- `set_service_price(_, 0)` stores an explicit zero price in `DataKey::ServicePrice`
- `remove_service_price(_)` removes the price slot entirely, reclaiming storage
- `remove_service_price(_)` also emits `price_rmv(service_id)` so indexers can
  distinguish a removal from a write-to-zero

Use removal when a service is being retired or reset to a truly unpriced state;
use `0` only when you intentionally want to keep an explicit free-service price
record on chain.

### Admin proposal validation

`propose_admin_transfer` rejects proposing the current admin as the new admin
(panics with `InvalidAdminProposal`). This surfaces no-op handovers as caller
mistakes rather than silently storing a pending entry equal to the active admin.

### Correction flow: `decrement_usage`

When a metering client over-reports (e.g. double-counts a batch), the
admin can call `decrement_usage(env, agent, service_id, amount)` to
subtract the erroneous delta from the per-pair counter without discarding
the legitimate remainder. The decrement uses saturating arithmetic (clamps
at zero, never underflows) and emits a distinct `usage_dec` event so
corrections are auditable and distinguishable from `record_usage` and
`settle`.

#### Lifetime-counter policy

`TotalUsageByAgent` and `TotalRequestsAllTime` are **not** adjusted by
`decrement_usage`. These counters track the raw reported figure for
analytics; correcting the per-pair balance should not retroactively distort
the lifetime signal. Off-chain billing pipelines that need the corrected
view should subtract the decrement event amount from the lifetime counter
when processing the `usage_dec` event.

### Prepaid agent credits

Agents can now be funded with prepaid credit balances via `credit_agent(agent, amount)`.
The balance is stored in stroops and is drawn down by `settle` as usage is
settled. `record_usage` rejects a write when the prepaid balance cannot cover
that service's projected bill for the updated usage total, so prepaid accounts
can enforce solvency before work is accepted.

`get_agent_credit(agent)` reads the current balance. A successful settlement
emits `credit_debited(agent, debit, new_balance)` so off-chain systems can
track balance consumption in real time.

### Lifetime settled-amount counters

In addition to lifetime request counters, the escrow contract tracks lifetime
settled value in stroops:

- `get_total_settled_by_agent(agent)` returns the cross-service amount ever
  settled for one agent.
- `get_total_settled_all_time()` returns the protocol-wide amount ever settled.

These counters are updated by settlement drains, use saturating arithmetic, and
are never reset or decremented by later `settle` calls. They default to `0`
before the first billable settlement, so dashboards can read them without
special-casing new agents or fresh deployments.

### Per-call request bounds: `min ≤ max` invariant

`record_usage` enforces an inclusive per-call floor and ceiling on the `requests`
argument via two admin settings:

- `set_min_requests_per_call(min)` — floor; `record_usage` rejects values below
  this with `RequestsBelowMinPerCall` (#9). Defaults to `0` (no floor).
- `set_max_requests_per_call(max)` — ceiling; `record_usage` rejects values
  above this with `RequestsExceedsMaxPerCall` (#8). Defaults to `u32::MAX` (no
  cap).

#### Consistency guard: `InvalidRequestBounds` (#23)

Both setters enforce the invariant **`min ≤ max`** at write time:

- `set_min_requests_per_call(min)` rejects a `min` that exceeds the
  currently-stored `MaxRequestsPerCall` (defaulting to `u32::MAX`).
- `set_max_requests_per_call(max)` rejects a `max` that falls below the
  currently-stored `MinRequestsPerCall` (defaulting to `0`).

A contradictory range (`min > max`) would make every `record_usage` call
unsatisfiable — any supplied value would trip either #8 or #9 — silently
bricking metering until an operator noticed and corrected the configuration.
The cross-bound check prevents this state from ever being stored.

`min == max` is explicitly allowed and enforces an **exact per-call request
count**: every `record_usage` call must supply precisely that many requests.
This is useful for forcing callers to bundle a fixed number of requests per
write to amortise per-transaction ledger costs.

#### Recommended operator ordering

When both bounds need to change, set the **ceiling first** and then the
**floor**:

```
set_max_requests_per_call(new_max);  // step 1: raise or set ceiling
set_min_requests_per_call(new_min);  // step 2: set floor (checked against new_max)
```

Setting the floor first risks a transient `InvalidRequestBounds` rejection if
the new floor temporarily exceeds the old (not-yet-updated) ceiling.

### Per-agent rate limiting (fixed window)

`record_usage` supports an optional per-agent rate limit anchored to
`env.ledger().timestamp()`. It is configured by two admin settings and is
**disabled by default** (both default to `0`):

- `set_max_requests_per_window(max)` — max `requests` an agent may accumulate
  per window (`get_max_requests_per_window`).
- `set_rate_window_seconds(seconds)` — the **fixed** window length
  (`get_rate_window_seconds`).

The limiter is active only when **both** are non-zero. Semantics are a
**fixed window** (not sliding): the window opens at an agent's first in-window
call and rolls forward as a whole once `now >= window_start + window_seconds`,
resetting the count. A call that would push the in-window count above the cap
is rejected with `RateLimitExceeded` (#15). State is per-agent
(`DataKey::RateWindow(agent)`), and an agent can never reset its own window
early — `window_start` only advances. Window arithmetic is saturating.

### Operator override: `reset_rate_window`

`reset_rate_window(env, agent)` is an admin-gated, pause-respecting entrypoint
that clears the per-agent `RateWindow` storage slot, so the next `record_usage`
call opens a fresh window with a zero count. This lets an operator lift a
throttle immediately — for example, when a misconfigured cap has been raised,
or a legitimate burst the operator wants to forgive.

**Idempotent:** resetting an agent that has no stored rate window is a no-op.
The configured cap and window length are **not** changed — only the agent's
accumulated count for the current window is cleared.

Emits a `rate_rst(agent)` event so the override is auditable.
#### Reading Rate-Window State

To inspect an agent's current rate-limit state without triggering a new request:

- `get_rate_window(env, agent)` — returns `(window_start, count)` (the raw stored state).
- `get_remaining_in_window(env, agent)` — returns the remaining capacity as a `u32`,
  accounting for window expiration. Returns the full cap if the window has
  expired or the rate limiter is disabled.

Both are **pure reads** and do not mutate state or roll the window forward.

### Schema version: fresh v2 init vs. legacy v1→v2 migration

`init` stamps the current storage schema version (v2) directly, so a freshly
deployed contract reports `get_schema_version() == 2` without ever running a
migration. A legacy contract deployed before this change carries the implicit v1
default and must call `migrate_v1_to_v2()` to reach v2; calling that migration on
a fresh v2 deploy panics with `MigrationVersionMismatch`.

### Global configuration snapshot: `get_contract_config`

`get_contract_config()` returns a `ContractConfig` struct containing all global
settings in a single read. It is a pure read — no `require_auth`, no pause gate
— and is available even before `init` (in which case `admin` is `None` and all
other fields carry their defaults).

The struct fields and their defaults when the storage slot is absent:

| Field                          | Type              | Default             | Individual getter                  |
| ------------------------------ | ----------------- | ------------------- | ---------------------------------- |
| `paused`                       | `bool`            | `false`             | `is_paused`                        |
| `allowlist_enabled`            | `bool`            | `false`             | `is_allowlist_enabled`             |
| `require_service_registration` | `bool`            | `false`             | `is_service_registration_required` |
| `max_requests_per_call`        | `u32`             | `u32::MAX` (no cap) | `get_max_requests_per_call`        |
| `min_requests_per_call`        | `u32`             | `0` (no floor)      | `get_min_requests_per_call`        |
| `max_requests_per_window`      | `u32`             | `0` (disabled)      | `get_max_requests_per_window`      |
| `window_seconds`               | `u64`             | `0` (disabled)      | `get_rate_window_seconds`          |
| `schema_version`               | `u32`             | `1` (pre-migration) | `get_schema_version`               |
| `admin`                        | `Option<Address>` | `None`              | `get_admin`                        |

The per-field getters remain available and always return values identical to
the corresponding fields in this struct. `ContractConfig` is a convenience
snapshot only and does not replace any existing getter.

### Combined billing snapshot: `get_billing_summary`

`get_billing_summary(agent, service_id)` returns a `BillingSummary` struct
containing usage, price, and the computed bill for an `(agent, service_id)`
pair in a single round-trip. This is a pure read — no `require_auth`, no pause
gate — that provides a coherent snapshot from the same ledger state, preventing
race conditions where separate reads could return inconsistent values (e.g., a
usage value from one ledger and a price from another).

The struct fields and their defaults when the storage slot is absent:

| Field | Type | Default | Description |
|---|---|---|---|
| `requests` | `u32` | `0` | Accumulated request count for the pair |
| `price_stroops` | `i128` | `0` | Per-request price in stroops |
| `billed` | `i128` | `0` | Computed bill: `requests * price_stroops` with saturating arithmetic |
| `last_settlement` | `Option<u64>` | `None` | Ledger timestamp of the last `settle` call, or `None` if never settled |

The `billed` field uses the same saturating arithmetic as `compute_billing`:
- When a tier schedule is configured, the bill uses the tier-aware computation
- Otherwise, the bill is `requests * price_stroops` with saturation at `i128::MAX`
- For unknown pairs (no usage, no price), all numeric fields default to zero

This combined read is particularly useful for off-chain dashboards that need to
render a single agent-service row, as it replaces three separate host invocations
(`get_usage`, `get_service_price`, and `compute_billing`) with one atomic read.
### Configuration-change events: `cfg_set`

Every rate-limit and per-call bound setter publishes a `cfg_set` event
after the storage write succeeds, so indexers and security monitors can
observe policy changes on-chain instead of only inferring them from
storage diffs. All six setters share one decodable schema: topic
`(symbol_short!("cfg_set"),)`, data `(name: Symbol, value)`.

| Setter                             | `name`      | `value` type |
| ---------------------------------- | ----------- | ------------ |
| `set_max_requests_per_call`        | `max_call`  | `u32`        |
| `set_min_requests_per_call`        | `min_call`  | `u32`        |
| `set_max_requests_per_window`      | `max_win`   | `u32`        |
| `set_rate_window_seconds`          | `win_sec`   | `u64`        |
| `set_allowlist_enabled`            | `allowlist` | `bool`       |
| `set_require_service_registration` | `req_reg`   | `bool`       |

A single subscriber can decode every config event with one schema:
match on the first tuple element (`Symbol`) to route to the right
handler, then decode the second element as `u32`, `u64`, or `bool`
per the table above.

Notes:

- Events fire even when the new value equals the current stored value
  — setters are not short-circuited by an equality check, so every
  call is observable.
- This is purely additive: `price_set`, `paused`, and all other
  existing event payloads are unchanged.
- Events expose no more information than was already readable via the
  corresponding getter (`get_max_requests_per_call`,
  `is_allowlist_enabled`, etc.) — `cfg_set` only makes an existing,
  publicly-readable state change observable in real time.

### Global price bounds for `set_service_price`

Admins can configure a global **price band** `[min_stroops, max_stroops]`
that every subsequent `set_service_price` call must respect. By default the
band is unbounded (`0` to `i128::MAX`), so existing behaviour is unchanged.

#### Entrypoints

| Entrypoint              | Signature                                | Description                                                            |
| ----------------------- | ---------------------------------------- | ---------------------------------------------------------------------- |
| `set_price_bounds`      | `(min_stroops: i128, max_stroops: i128)` | Admin-gated. Persist the floor and ceiling. Emits `bnd_set(min, max)`. |
| `get_min_service_price` | `() → i128`                              | Read the floor; returns `0` if never set.                              |
| `get_max_service_price` | `() → i128`                              | Read the ceiling; returns `i128::MAX` if never set.                    |

#### How the check works

After passing the existing negative-price gate and the registration/disabled
gates, `set_service_price` reads `MinServicePrice` (default `0`) and
`MaxServicePrice` (default `i128::MAX`) and rejects any price outside
`[floor, ceiling]` with `PriceOutOfBounds` (#23).

#### Zero-is-free semantics

A price of `0` means **free service** — usage is still recorded but settlement
bills nothing. The price bounds interact with this as follows:

- When `min_stroops == 0` (the default), a zero price is permitted as usual.
- When `min_stroops > 0`, free services are **explicitly forbidden**:
  `set_service_price(svc, 0)` is rejected with `PriceOutOfBounds` until the
  floor is lowered back to `0`.

This is intentional policy: a positive floor expresses that all services in
the band must have a non-zero cost. Admins who want to allow free services
alongside bounded paid services should keep `min_stroops = 0`.

#### Error codes (new, append-only)

| Code  | Variant             | Trigger                                                     |
| ----- | ------------------- | ----------------------------------------------------------- |
| `#23` | `PriceOutOfBounds`  | `set_service_price` price falls outside `[floor, ceiling]`. |
| `#24` | `InvertedPriceBand` | `set_price_bounds` called with `min_stroops > max_stroops`. |

#### Security

- `set_price_bounds` is admin-gated: a non-admin call panics with
  Soroban's host auth error before any storage is touched.
- `get_min_service_price` / `get_max_service_price` are pure reads (no auth
  required), so dashboards can query the current band without signing.
- An inverted band (`min > max`) is rejected immediately, so the stored
  bounds are always a valid interval `min ≤ max`.

### Persistent storage TTL management

Soroban persistent entries have a TTL (measured in ledgers). Without periodic
extension, entries that are not frequently rewritten will eventually expire
and be archived off-chain, breaking reads until restored. The escrow contract
manages TTL automatically for price-tier and service-metadata entries:

- **Shared constants:** `LEDGERS_TTL_THRESHOLD` (100 800 ledgers, ~7 days)
  and `LEDGERS_TTL_EXTEND_TO` (201 600 ledgers, ~14 days) define the
  bump policy.
- **Shared helper:** `bump_persistent(env, key)` extends the TTL of any
  persistent entry whose remaining TTL is at or below the threshold.
- **Reads:** `get_price_tiers` and `get_service_metadata` call
  `bump_persistent` after reading, keeping actively-queried entries alive.
- **Writes:** `set_price_tiers`, `set_service_metadata`,
  `register_service_with_metadata`, and `transfer_service_ownership` call
  `bump_persistent` after writing.
- **Deletes:** `remove_price_tiers` and `clear_service_metadata` delete the
  entry outright — no TTL extension is performed.

When the current TTL is above the threshold the `extend_ttl` call is a
host-level no-op, so there is negligible cost for frequently-accessed entries.

## Prerequisites

- [Rust](https://rustup.rs/) (stable, with `rustfmt`)
- [Stellar Soroban CLI](https://soroban.stellar.org/docs) (optional, for deployment)

## Setup for contributors

1. **Clone the repo** (or add remote and pull):

   ```bash
   git clone <repo-url> && cd agentpay-contracts
   ```

2. **Install Rust** (if needed):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup component add rustfmt
   ```

3. **Verify setup**:
   ```bash
   cargo fmt --all -- --check
   cargo build
   cargo test
   ```

## Project structure

```
agentpay-contracts/
├── Cargo.toml              # Workspace root
├── contracts/
│   └── escrow/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs      # Contract logic
│           └── test.rs     # Unit tests
└── .github/workflows/
    └── ci.yml              # CI: fmt, build, test
```

## Commands

| Command                      | Description           |
| ---------------------------- | --------------------- |
| `cargo fmt --all`            | Format code           |
| `cargo fmt --all -- --check` | Check formatting (CI) |
| `cargo build`                | Build                 |
| `cargo test`                 | Run tests             |

## Documentation

- [Escrow: Build, Test, and Deploy Guide](docs/escrow/build-deploy.md) — build the release WASM, run the test suite, and deploy to testnet with the Stellar/Soroban CLI.
- [Escrow: Schema Versioning & Migration](docs/escrow/migrations.md) — the difference between `version()` and `SchemaVersion`, the double-run guard, and the migration runbook.
- [Escrow: Storage DataKey Reference](docs/escrow/storage.md) — complete map of every `DataKey` variant: stored value type, default when absent, which entrypoints write it, and whether it is drained by `settle`. Explains why everything uses `persistent()` and the per-pair vs per-agent vs singleton key cardinality.
- [Escrow: Events Reference](docs/escrow/events.md) — every event topic, its payload shape, and which entrypoint emits it, including the shared `cfg_set`/`dispute` topics and the four setters that currently emit no event at all.

## CI/CD

On push/PR to `main`, GitHub Actions runs:

- Format check (`cargo fmt --all -- --check`)
- Build (`cargo build`)
- Tests (`cargo test`)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide, including the
append-only error-code table, event conventions, and the test/coverage gate.

1. Fork the repo and create a branch.
2. Make changes; ensure `cargo fmt`, `cargo build`, and `cargo test` pass locally.
3. Open a pull request. CI must pass before merge.

## License

MIT

### Agent authorization on `record_usage`

`record_usage` now requires the recorded `agent` to authorize the call via
`agent.require_auth()`. This closes a usage-forgery vector where any party
could inflate a competitor agent's counters — and therefore its bill on the
next `settle` — with no signature from the agent.

#### Validation chain position

Auth is checked at **step 0**, before the pause gate:

| Step | Check                  | Error                          |
| ---- | ---------------------- | ------------------------------ |
| 0    | `agent.require_auth()` | Soroban host auth error        |
| 1    | Contract paused        | `#4 ContractPaused`            |
| 2    | `requests == 0`        | `#2 RequestsMustBePositive`    |
| 3    | `requests > max`       | `#8 RequestsExceedsMaxPerCall` |
| 4    | `requests < min`       | `#9 RequestsBelowMinPerCall`   |
| 5    | Service not registered | `#7 ServiceNotRegistered`      |
| 6    | Service disabled       | `#12 ServiceDisabled`          |
| 7    | Agent on blocklist     | `#17 AgentBlocked`             |
| 8    | Agent not allowed      | `#10 AgentNotAllowed`          |

#### Operator override (metering loop migration)

Soroban's auth tree supports sub-invocation authorization — an agent can
pre-authorize a trusted metering operator to call `record_usage` on its
behalf by having the operator's call appear as a sub-invocation of an
agent-signed outer call. This means existing off-chain settlement loops
can continue to operate without requiring every agent to sign each
individual `record_usage` call directly, as long as the operator is
authorized via the auth tree.

**Migration path for existing metering operators:**

1. The agent signs an outer transaction that authorizes the operator's
   contract call via Soroban's `authorize_as_current_contract` or
   sub-invocation auth.
2. The operator's metering loop submits `record_usage` as a
   sub-invocation within that authorized context.
3. Alternatively, agents can sign each `record_usage` call directly
   (standard path) if the metering loop supports it.
