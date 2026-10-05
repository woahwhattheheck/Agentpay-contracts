# EscrowError Code Table

All error codes emitted by `contracts/escrow`. Codes are **append-only** —
existing numbers never change so that client SDKs and off-chain decoders remain
stable across upgrades.

## Error Code Reference

| Code | Variant | Trigger condition | Entrypoint(s) that panic |
|------|---------|-------------------|--------------------------|
| 1 | `AlreadyInitialized` | `init` was called but an admin address is already stored in persistent storage. | `init` |
| 2 | `RequestsMustBePositive` | `requests == 0` **or** a negative/zero price is supplied (overloaded — see note below). | `record_usage`, `set_price` |
| 3 | `NotInitialized` | An admin-gated entrypoint was invoked before `init` has stored the admin address. | `settle`, `settle_all`, `record_usage`, `set_*` admin setters, `pause`, `unpause`, `accept_admin_transfer`, `propose_admin_transfer`, `migrate_v1_to_v2` |
| 4 | `ContractPaused` | A state-changing entrypoint was called while the `Paused` flag is `true`. | `settle`, `settle_all`, `record_usage`, `set_service_metadata`, `clear_service_metadata`, `transfer_service_ownership`, `register_service`, `deregister_service`, `enable_service`, `disable_service` |
| 5 | `NoPendingAdminTransfer` | `accept_admin_transfer` was called but no pending admin address has been stored. | `accept_admin_transfer` |
| 6 | `NotPendingAdmin` | `accept_admin_transfer` was called by an address that does not match the stored pending admin. | `accept_admin_transfer` |
| 7 | `ServiceNotRegistered` | `record_usage` referenced a `service_id` that has not been registered while strict registration mode is enabled. Also raised by `get_usage_batch` for unregistered services. | `record_usage`, `get_usage_batch` |
| 8 | `RequestsExceedsMaxPerCall` | `requests` supplied to `record_usage` exceeds the `MaxRequestsPerCall` cap (when the cap is non-zero). | `record_usage` |
| 9 | `RequestsBelowMinPerCall` | `requests` supplied to `record_usage` is below the `MinRequestsPerCall` floor (when the floor is non-zero). | `record_usage` |
| 10 | `AgentNotAllowed` | `record_usage` was called by or for an agent not present on the allowlist while strict allowlisting is enabled. | `record_usage` |
| 11 | `MigrationVersionMismatch` | `migrate_v1_to_v2` was called on a contract that is not at schema version 1 (e.g. already at v2 or freshly initialized). | `migrate_v1_to_v2` |
| 12 | `ServiceDisabled` | `record_usage` referenced a service that is registered but has been disabled via `disable_service`. | `record_usage`, `set_price` |
| 13 | `ServiceMetadataNotFound` | A metadata-scoped entrypoint, including non-admin settlement, referenced a `service_id` whose `ServiceMetadata` slot has never been set. | `settle`, `settle_all`, `set_service_metadata`, `clear_service_metadata`, `transfer_service_ownership` |
| 14 | `InvalidAdminProposal` | `propose_admin_transfer` was called with the current admin as the proposed new admin (no-op handover rejected to surface caller mistakes). | `propose_admin_transfer` |
| 15 | `RateLimitExceeded` | `record_usage` would push an agent's per-window request count above `MaxRequestsPerWindow` (active only when both `MaxRequestsPerWindow` and `RateWindowSeconds` are non-zero). | `record_usage` |
| 16 | `BatchTooLarge` | `get_usage_batch` was called with more pairs than the `MAX_BATCH_READ` constant allows. | `get_usage_batch` |
| 17 | `AgentBlocked` | `record_usage` was called by or for an agent on the per-agent blocklist. Takes precedence over the allowlist check (code 10). | `record_usage` |
| 18 | `InvalidPriceTiers` | `set_price_tiers` was called with a malformed schedule: empty, duplicate thresholds, or not strictly ascending. | `set_price_tiers` |
| 19 | `SettleAllTooLarge` | `settle_all` was called but the agent's service index exceeds `MAX_SETTLE_ALL`. | `settle_all` |
| 20 | `DisputeAlreadyOpen` | `open_dispute` was called but a dispute is already open for the given `(agent, service_id)`. | `open_dispute` |
| 21 | `NoOpenDispute` | `resolve_dispute` was called but no dispute is open for the given `(agent, service_id)`. | `resolve_dispute` |
| 22 | `RefundExceedsUsage` | `resolve_dispute` was called with `refund_requests` exceeding the current accumulated usage. | `resolve_dispute` |
| 23 | `InvalidRequestBounds` | `set_min_requests_per_call` was called with a `min` that exceeds `MaxRequestsPerCall`, or `set_max_requests_per_call` was called with a `max` below `MinRequestsPerCall`. | `set_min_requests_per_call`, `set_max_requests_per_call` |
| 24 | `PriceOutOfBounds` | `set_service_price` was called with a price outside the configured `[MinServicePrice, MaxServicePrice]` bounds. | `set_service_price` |
| 25 | `InvertedPriceBand` | `set_price_bounds` was called with `min_stroops > max_stroops`. | `set_price_bounds` |
| 26 | `Unauthorized` | An authenticated caller is neither the contract admin nor the service owner for a settlement, or lacks authority for another operation. | `settle`, `settle_all`, `transfer_service_ownership` |
| 27 | `InvalidOwnerTransfer` | `transfer_service_ownership` was called with a `new_owner` that matches the current owner. | `transfer_service_ownership` |
| 28 | `InsufficientCreditBalance` | The projected bill exceeds an agent's positive prepaid credit balance. | `record_usage` |

## Settlement rejection codes

`settle` and `settle_all` return contract error #3 before initialization and
#4 when paused. A non-admin caller with no service metadata receives #13;
an authenticated caller who is neither the admin nor that service's owner
receives #26. `settle_all` also returns #19 if its service index exceeds the
supported bound. Soroban signature failures from `require_auth` remain host
auth errors. Code #6 belongs only to admin handover and is never used to
reject a settlement caller.

## Notes on Overloaded Codes

### Code 2 — `RequestsMustBePositive`
Originally introduced for the `requests == 0` guard in `record_usage`, this code
is also raised when a price value is non-positive (zero or negative). SDK authors
should treat code 2 as "a required positive integer argument was zero or
negative" rather than purely a request-count error.



## Conventions for Future Codes

- Codes are **strictly append-only**. Never reassign an existing number.
- New variants are added at the end of the `EscrowError` enum in
  `contracts/escrow/src/lib.rs` with the next sequential integer.
- Every new variant must include a `///` doc comment on the enum variant **and**
  a new row in this table before the PR is merged.
- Overloads (one code covering multiple trigger conditions) are discouraged for
  new codes; prefer a dedicated code unless the semantic overlap is exact.
