# Settlement model

## Storage

| Key | Type | Description |
| --- | --- | --- |
| `DataKey::Usage(agent, service_id)` | `u32` | Accumulated, unsettled request count for the pair. Zeroed by `settle`/`settle_all`. |
| `DataKey::SettlementClaimed(agent, service_id)` | `bool` | Once-only claim for the current settlement cycle. Written before any settlement effect; removed only by successful positive `record_usage`. |
| `DataKey::ServicePrice(service_id)` | `i128` | Flat per-request price in stroops. |
| `DataKey::PriceTiers(service_id)` | `Vec<PriceTier>` | Optional volume-discount schedule; when present, `settle`/`compute_billing`/`get_billing_summary` use it instead of the flat price. `settle_all` always uses the flat price (see caveat below). |
| `DataKey::LastSettlement(agent, service_id)` | `u64` | Ledger timestamp of the pair's most recent drain. Absent until the pair is settled at least once. |
| `DataKey::TotalSettledByAgent(agent)` | `i128` | Lifetime settled amount across all of an agent's services. Never reset by settlement. |
| `DataKey::TotalSettledAllTime` | `i128` | Protocol-wide lifetime settled amount. Saturates at `i128::MAX`. |
| `DataKey::AgentServiceIndex(agent)` | `Vec<Symbol>` | The agent's active-service index — services with usage `settle_all` should sweep. Capped at `MAX_AGENT_SERVICE_INDEX` (== `MAX_SETTLE_ALL`, 256). |

## Invariants

- **`settle` and `settle_all` are owner-or-admin gated**, not admin-only: `caller` must be the contract admin **or** the `ServiceMetadata.owner` of the service(s) being settled. `settle` panics with `NotPendingAdmin` on rejection; `settle_all` and `transfer_service_ownership` panic with `Unauthorized` for the same underlying check — see [`docs/escrow/admin.md`](./admin.md) for the shared `is_owner_or_admin` helper and why the error codes differ.
- **Billing engines diverge between `settle` and `settle_all`.** `settle` (and `get_billing_summary`) use `compute_billing_for_requests`, which prefers a `PriceTiers` schedule when one is set for the service, falling back to the flat `ServicePrice`. `settle_all` always uses the flat `ServicePrice` directly and does **not** consult `PriceTiers`. Call `settle` per service if tiered pricing must be honored.
- **Settlement is once-only per positive-usage cycle.** `settle` and `settle_all` share `SettlementClaimed(agent, service_id)`. The claim is the first state mutation in the single path. The batch path first authorizes every service, then claims every cycle, and only then applies credit, counter, usage, timestamp, index, and event effects. A duplicate rejects with `SettlementAlreadyApplied` (#29). Transaction rollback makes a failed batch all-or-nothing.
- **Positive usage is the only re-arm.** Every successful `record_usage(..., requests > 0)` removes the pair's claim and begins a fresh cycle. Administrative decrements, dispute refunds, reads, and duplicate settlement attempts do not re-arm it.
- **`settle` deindexes on completion; `settle_all` does not.** `settle` removes the service from `AgentServiceIndex` once drained. `settle_all` leaves swept services indexed, but a repeated sweep now reaches the shared claim and rejects instead of restamping or re-emitting. New usage both re-arms and keeps/re-adds the service in the index.
- **A zero-usage cycle may be settled once, not repeatedly.** The first authorized drain still stamps `LastSettlement` and emits `settled` with billed amount `0`; another call without positive usage is a duplicate and rejects with #29.
- **`settle_all` is bounded by `MAX_SETTLE_ALL`.** Panics with `SettleAllTooLarge` if the index exceeds it. In practice this can't be reached through the public API — `record_usage` already caps the index at the same constant — the guard exists for a hypothetical future migration that could write a larger index. See `test/settlement-01-boundaries` for coverage that exercises the guard directly.
- **Both drains emit events.** `settle` emits one `settled(agent, service_id, requests, billed)`. `settle_all` emits one `settled` event *per service* in its sweep, then a single `settl_all(agent, count, total_billed)` batch-summary event so indexers don't have to sum the per-service events themselves.
- **All settlement amounts saturate, never overflow-panic.** `billed`, `TotalSettledByAgent`, `TotalSettledAllTime`, and the `settl_all` event's `total_billed` all use saturating arithmetic, capping at `i128::MAX`.

## Entrypoints

| Entrypoint | Gate | Effect |
| --- | --- | --- |
| `settle(caller, agent, service_id)` | owner-or-admin | Claims then drains one pair. Duplicate cycle: `SettlementAlreadyApplied` (#29). Emits `settled` only on success. |
| `settle_all(caller, agent)` | owner-or-admin per service | Preflights authorization, claims the full batch, then drains every indexed service. Any duplicate claim aborts the whole batch before effects. |
| `get_last_settlement(agent, service_id)` | none (read) | `Option<u64>` — the pair's last drain timestamp. |
| `get_billing_summary(agent, service_id)` | none (read) | `{ requests, price_stroops, billed, last_settlement }` for one pair, tier-aware. |
| `get_agent_settlement_summary(agent)` | none (read) | `{ total_settled, outstanding_services, last_settlement }` across the agent's index — see caveat in the struct's doc comment about `settle`'s deindexing. |
| `get_total_settled_by_agent(agent)` | none (read) | Lifetime settled total for one agent. |
| `get_total_settled_all_time()` | none (read) | Protocol-wide lifetime settled total. |

## Worked example

```text
1. set_service_price(infer, 10)
2. record_usage(agent, infer, 4)          // Usage(agent, infer) = 4
3. record_usage(agent, storage, 2)        // AgentServiceIndex(agent) = [infer, storage]
4. settle(admin, agent, infer)            // bills 40, zeroes Usage(agent, infer),
                                           // stamps LastSettlement, emits settled,
                                           // deindexes infer:
                                           // AgentServiceIndex(agent) = [storage]
5. settle_all(admin, agent)               // sweeps [storage] only (infer already gone):
                                           // emits settled(agent, storage, 2, billed),
                                           // then settl_all(agent, 1, billed)
                                           // storage stays indexed at usage = 0
6. settle_all(admin, agent)               // rejected with SettlementAlreadyApplied (#29):
                                           // storage has no new positive usage, so no
                                           // counters, stamps, credit, or events repeat
7. record_usage(agent, storage, 1)        // removes storage's claim; new cycle armed
8. settle_all(admin, agent)               // succeeds for the fresh storage cycle
```
