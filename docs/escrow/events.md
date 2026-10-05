# Escrow Contract — Events Reference

This document catalogs every event topic published by the escrow contract
(`contracts/escrow/src/lib.rs`), its payload shape, and when it fires. All
topics are `symbol_short!` values, which are limited to 9 ASCII characters;
the macro fails to compile on any longer literal, so the limit is enforced
at build time rather than by a runtime test.

---

## Event catalog

| Topic | Payload | Emitted by | Fires when |
|---|---|---|---|
| `cred_deb` | `(agent, debit, new_balance)` | `debit_agent_credit` (internal, called from `record_usage` and `settle`) | An agent's prepaid credit balance is drawn down to cover a bill |
| `usage` | `(agent, service_id, requests, total)` | `record_usage` | Every successful usage record, one event per call |
| `usage_hi` | `(agent, service_id, total)` | `record_usage` | The per-pair usage total crosses `UsageAlertThreshold` from below to at/above, edge-triggered (see Gaps below — currently unreachable) |
| `usage_dec` | `(agent, service_id, amount, new_total)` | `decrement_usage` | Admin manually decrements a usage counter |
| `price_set` | `(service_id, price_stroops)` | `set_service_price` | Flat per-request price is set for a service |
| `price_rmv` | `service_id` | `remove_service_price` | Flat price is removed for a service |
| `tiers_set` | `service_id` | `set_price_tiers` | A volume-discount tier schedule is set for a service |
| `tiers_rm` | `service_id` | `remove_price_tiers` | A tier schedule is removed, reverting to flat pricing |
| `settled` | `(agent, service_id, requests, billed)` | `settle`, `settle_all` (once per service settled) | A `(agent, service_id)` pair is drained and billed |
| `settle_v` | `(agent, service_id, new_version)` | `settle`, `settle_all` | The pair's optimistic-concurrency settlement version is bumped after a successful drain |
| `settl_all` | `(agent, count, total_billed)` | `settle_all` | Batch summary after a full sweep — `count` includes zero-billed services |
| `bnd_set` | `(min_stroops, max_stroops)` | `set_price_bounds` | Global min/max service price bounds are changed |
| `cfg_set` | `(tag, value)` — see [Config tags](#config-tags) | `set_allowlist_enabled`, `set_min_requests_per_call`, `set_max_requests_per_call`, `set_max_requests_per_window`, `set_rate_window_seconds`, `set_require_service_registration` | A scalar admin config value is changed |
| `rate_rst` | `agent` | `reset_rate_window` | An agent's rate-limit window is manually cleared |
| `svc_add` | `service_id` | `register_service` | A service is registered (no metadata) |
| `svc_reg` | `(service_id, owner)` | `register_service_with_metadata` | A service is registered with metadata in one call |
| `svc_rm` | `service_id` | `unregister_service` | A service is unregistered |
| `svc_dis` | `(service_id, disabled)` | `set_service_disabled` | A service's disabled flag is toggled either way |
| `meta_set` | `(service_id, owner)` | `set_service_metadata` | Service metadata is set or overwritten |
| `meta_clr` | `service_id` | `clear_service_metadata` | Service metadata is cleared |
| `owner_chg` | `(service_id, old_owner, new_owner)` | `transfer_service_ownership` | A service's metadata owner changes |
| `admin_chg` | `(old_admin, new_admin)` | `accept_admin_transfer` | Step 2 of admin handover completes |
| `paused` | `bool` | `pause` (`true`), `unpause` (`false`) | The pause switch is toggled either way |
| `dispute` | `(action, agent, service_id[, refund_requests])` — see [Dispute actions](#dispute-actions) | `open_dispute`, `resolve_dispute`, `refund_batch` | A dispute is opened or resolved |

### Config tags

`cfg_set` is a shared topic for single-scalar admin config changes. The first
payload element is a fixed `symbol_short!` tag identifying which config value
changed; the second is the new value:

| Tag | Set by | Value type |
|---|---|---|
| `allowlist` | `set_allowlist_enabled` | `bool` |
| `min_call` | `set_min_requests_per_call` | `u32` |
| `max_call` | `set_max_requests_per_call` | `u32` |
| `max_win` | `set_max_requests_per_window` | `u32` |
| `win_sec` | `set_rate_window_seconds` | `u64` |
| `req_reg` | `set_require_service_registration` | `bool` |

### Dispute actions

The `dispute` topic is shared by the open and resolve paths; the action is
carried in the payload rather than the topic, keeping the topic count small
and giving indexers a single subscription for the whole dispute lifecycle:

| Action | Payload after `action` | Emitted by |
|---|---|---|
| `open` | `(agent, service_id)` | `open_dispute` |
| `resolve` | `(agent, service_id, refund_requests)` | `resolve_dispute`, `refund_batch` (once per resolved service) |

`refund_requests` is the amount subtracted from the usage counter — `0` for
`resolve_dispute` when the admin declines a refund, or the full prior usage
for `refund_batch`, which always zeroes the counter.

---

## Invariants

- **No topic collisions.** Every entrypoint that mutates admin-visible state
  publishes under a distinct topic, except the intentionally-shared `cfg_set`
  (disambiguated by its `tag` field) and `dispute` (disambiguated by its
  `action` field). No two *unrelated* state changes share a topic.
- **9-character limit.** `symbol_short!` topics must be ≤ 9 ASCII characters,
  enforced by a compile error on any longer literal — every topic in the
  table above already builds, so all satisfy the limit.
- **Events follow the storage write, not precede it.** Every `publish` call
  in `lib.rs` occurs after the corresponding `env.storage()` write, so a
  listener that reacts to an event by reading contract state will always see
  the state the event describes.
- **Events do not gate control flow.** Publishing is a side effect with no
  return value consulted by the contract; a hypothetical event-delivery
  failure cannot change whether a call succeeds or panics.

## Gaps (storage writes with no matching event)

Cross-referencing every `env.storage().persistent().set` / `.remove` call
against the table above surfaces four admin-facing state changes that do
**not** publish a dedicated event, unlike every other setter in the
contract:

- `set_agent_allowed` — writes `AgentAllowed(agent)`, no event.
- `set_agent_blocked` — writes `AgentBlocked(agent)`, no event.
- `propose_admin_transfer` — writes `PendingAdmin`, no event.
- `cancel_admin_transfer` — removes `PendingAdmin`, no event.

Additionally, `usage_hi` (documented above) can never fire on the current
contract because `UsageAlertThreshold` has no setter — see
[`docs/escrow/storage.md`](storage.md) for that gap.

These are documented here as known gaps for indexer authors, not fixed by
this PR — closing them is separate, scoped work.

## Testing

Events are asserted with `env.events().all()` in `test.rs`, typically via
helpers like `assert_latest_usage_event` and `assert_latest_pause_event`
that decode the most recent event and check topic + payload. Boundary and
payload-correctness tests capture events immediately after the mutating
call, per the project's contribution guidelines.
