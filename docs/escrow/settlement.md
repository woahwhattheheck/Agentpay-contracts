# Settlement model

## Storage

| Key | Type | Description |
| --- | --- | --- |
| `DataKey::Usage(agent, service_id)` | `u32` | Accumulated, unsettled request count for the pair. Zeroed by `settle`/`settle_all`. |
| `DataKey::ServicePrice(service_id)` | `i128` | Flat per-request price in stroops. |
| `DataKey::PriceTiers(service_id)` | `Vec<PriceTier>` | Optional volume-discount schedule; when present, `settle`/`compute_billing`/`get_billing_summary` use it instead of the flat price. `settle_all` always uses the flat price (see caveat below). |
| `DataKey::LastSettlement(agent, service_id)` | `u64` | Ledger timestamp of the pair's most recent drain. Absent until the pair is settled at least once. |
| `DataKey::SettlementVersion(agent, service_id)` | `u64` | Optimistic-concurrency version for the pair. Defaults to 0 and increments after every successful direct or batched settlement. |
| `DataKey::TotalSettledByAgent(agent)` | `i128` | Lifetime settled amount across all of an agent's services. Never reset by settlement. |
| `DataKey::TotalSettledAllTime` | `i128` | Protocol-wide lifetime settled amount. Saturates at `i128::MAX`. |
| `DataKey::AgentServiceIndex(agent)` | `Vec<Symbol>` | The agent's active-service index — services with usage `settle_all` should sweep. Capped at `MAX_AGENT_SERVICE_INDEX` (== `MAX_SETTLE_ALL`, 256). |

## Invariants

- **Direct settlement is version-guarded.** Read `get_settlement_version(agent, service_id)`, then pass that value as `expected_version` to `settle`. A mismatch is rejected with `VersionConflict` before billing, credit debit, usage drain, totals, timestamps, or events are mutated. Each successful `settle` bumps the pair version atomically. `settle_all` also bumps every swept pair so a version read before a batch sweep becomes stale.
  - `VersionConflict` uses the contract's existing Soroban `#[contracterror]` convention and stable code 30. Soroban v22 contract errors carry the numeric code rather than an attached payload, so callers obtain the authoritative current version from `get_settlement_version` after a conflict.
- **`settle` and `settle_all` are owner-or-admin gated**, not admin-only: `caller` must be the contract admin **or** the `ServiceMetadata.owner` of the service(s) being settled. `settle` panics with `NotPendingAdmin` on rejection; `settle_all` and `transfer_service_ownership` panic with `Unauthorized` for the same underlying check — see [`docs/escrow/admin.md`](./admin.md) for the shared `is_owner_or_admin` helper and why the error codes differ.
- **Billing engines diverge between `settle` and `settle_all`.** `settle` (and `get_billing_summary`) use `compute_billing_for_requests`, which prefers a `PriceTiers` schedule when one is set for the service, falling back to the flat `ServicePrice`. `settle_all` always uses the flat `ServicePrice` directly and does **not** consult `PriceTiers`. Call `settle` per service if tiered pricing must be honored.
- **`settle` deindexes on completion; `settle_all` does not.** `settle` removes the service from `AgentServiceIndex` once drained, since a fully-settled service has nothing left for `settle_all` to sweep. `settle_all` intentionally leaves every swept service in the index — including ones it just zeroed — so a repeated `settle_all` call re-processes them (billing `0`, restamping `LastSettlement`) rather than silently skipping them.
- **Zero-usage services are still settled, not skipped.** Both `settle` and `settle_all` zero the usage counter and stamp `LastSettlement` even when the current usage is `0`, so callers can confirm a full sweep completed (`settle_all`'s return `Vec` includes every swept service, `billed: 0` for the zero-usage ones).
- **`settle_all` is bounded by `MAX_SETTLE_ALL`.** Panics with `SettleAllTooLarge` if the index exceeds it. In practice this can't be reached through the public API — `record_usage` already caps the index at the same constant — the guard exists for a hypothetical future migration that could write a larger index. See `test/settlement-01-boundaries` for coverage that exercises the guard directly.
- **Both drains emit events.** `settle` emits one `settled(agent, service_id, requests, billed)`. `settle_all` emits one `settled` event *per service* in its sweep, then a single `settl_all(agent, count, total_billed)` batch-summary event so indexers don't have to sum the per-service events themselves.
- **All settlement amounts saturate, never overflow-panic.** `billed`, `TotalSettledByAgent`, `TotalSettledAllTime`, and the `settl_all` event's `total_billed` all use saturating arithmetic, capping at `i128::MAX`.

## Entrypoints

| Entrypoint | Gate | Effect |
| --- | --- | --- |
| `settle(caller, agent, service_id, expected_version)` | owner-or-admin + exact pair version | Drains one pair. Emits `settle_v` and `settled`; stale versions fail with `VersionConflict`. |
| `settle_all(caller, agent)` | owner-or-admin per service | Drains every service in the agent's index. Emits one `settled` per service, then `settl_all`. |
| `get_last_settlement(agent, service_id)` | none (read) | `Option<u64>` — the pair's last drain timestamp. |
| `get_settlement_version(agent, service_id)` | none (read) | `u64` — current pair version, default 0. |
| `get_billing_summary(agent, service_id)` | none (read) | `{ requests, price_stroops, billed, last_settlement }` for one pair, tier-aware. |
| `get_agent_settlement_summary(agent)` | none (read) | `{ total_settled, outstanding_services, last_settlement }` across the agent's index — see caveat in the struct's doc comment about `settle`'s deindexing. |
| `get_total_settled_by_agent(agent)` | none (read) | Lifetime settled total for one agent. |
| `get_total_settled_all_time()` | none (read) | Protocol-wide lifetime settled total. |

## Worked example

```text
1. set_service_price(infer, 10)
2. record_usage(agent, infer, 4)          // Usage(agent, infer) = 4
3. record_usage(agent, storage, 2)        // AgentServiceIndex(agent) = [infer, storage]
4. settle(admin, agent, infer, 0)         // bills 40, zeroes Usage(agent, infer),
                                           // stamps LastSettlement, emits settled,
                                           // deindexes infer:
                                           // AgentServiceIndex(agent) = [storage]
5. settle_all(admin, agent)               // sweeps [storage] only (infer already gone):
                                           // emits settled(agent, storage, 2, billed),
                                           // then settl_all(agent, 1, billed)
                                           // storage stays indexed at usage = 0
6. settle_all(admin, agent)               // sweeps [storage] again, billed = 0 this
                                           // time; still emits both events
```
