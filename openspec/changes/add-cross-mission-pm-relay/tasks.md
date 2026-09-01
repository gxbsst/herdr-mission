## 1. Additive Schema And Contracts

- [x] 1.1 Add focused RED tests for exact peer schema creation, idempotent reopen, malformed same-name tables and frozen coordination v3 shape.
- [x] 1.2 Implement `mission_peer_identity`, `mission_peers`, `mission_peer_routes` and `mission_peer_messages` additive migration with strict shape validation and no new dependency.
- [x] 1.3 Add typed peer IDs, routes, payloads, receipts, inbox rows, states and error mappings with body/envelope size limits.

## 2. Local Cross-Mission Relay

- [x] 2.1 Add RED tests proving same-Mission PM self-send and non-PM/invalid-kind calls fail closed with zero Team and peer writes.
- [x] 2.2 Add RED tests for atomic local PM-to-PM delivery, exact PM generation provenance, no cross-Mission Assignment and reopen-visible target inbox.
- [x] 2.3 Implement local relay send, exact duplicate/conflict handling, PM inbox projection and target-PM-only acknowledge.

## 3. Peer Configuration And Remote Receive

- [x] 3.1 Add RED tests for local identity, safe SSH destination, exact Mission-pair route binding and disabled/missing route rejection.
- [x] 3.2 Implement idempotent peer identity/config/route APIs and CLI commands; reject identity changes while durable peer messages exist.
- [x] 3.3 Add RED tests for bounded typed stdin, unknown fields, forced peer mismatch, target peer/Mission mismatch, digest mismatch, accepted receipt, duplicate receipt and ID conflict.
- [x] 3.4 Implement `peer receive` so inbound commit precedes receipt and all invalid envelopes preserve the pre-call database snapshot.

## 4. Durable SSH Delivery

- [x] 4.1 Add RED tests for outbound-before-network ordering, body-only-on-stdin transport, exact receipt validation, remote-commit/response-loss retry and independent peer failure.
- [x] 4.2 Implement injectable `PeerTransport`, shell-free `SystemSshPeerTransport`, durable outbound claim/retry and acknowledged receipt transition.
- [x] 4.3 Implement `peer send`, `peer deliver` and the explicit `send --target pm --target-mission [--peer]` shortcut without changing ordinary Team ACL.

## 5. PM Init, Wake And Lifecycle

- [x] 5.1 Add RED tests that `init` returns unhandled peer provenance, ack removes it after reopen, and wake failure preserves a pending inbox.
- [x] 5.2 Implement target PM best-effort wake with persisted `notified_at`; retry on `peer deliver`, `reconcile` and daemon without treating prompt as handled.
- [x] 5.3 Update PM role guidance with the cross-Mission delegation/result flow and the rule that remote messages must be decomposed into local Assignments.

## 6. Verification

- [x] 6.1 Run focused peer, CLI, bootstrap, kernel, daemon and adjacent Memory/Project regression suites.
- [x] 6.2 Run `cargo fmt --check`, focused Clippy with warnings denied, focused and all OpenSpec strict validation, Git diff checks and untracked no-index whitespace checks.
- [x] 6.3 Review the real diff for credential leakage, shell interpolation, frozen schema/API drift and unrelated Memory WIP changes; record platform/build gaps.
