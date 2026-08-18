# `herdr.mission.kernel.v1` scaffold protocol

The standalone binary advertises its build and protocol contract with:

```sh
herdr-mission --version
```

```json
{"binary":"herdr-mission","binary_version":"0.1.0","binary_contract":"herdr.mission.kernel.binary.v1","protocol":"herdr.mission.kernel.v1","operations":["handle","drive","inspect"]}
```

Each fixture request contains these required fields:

- `protocol`: exactly `herdr.mission.kernel.v1`.
- `binary_contract`: exactly `herdr.mission.kernel.binary.v1`.
- `request_id`: stable caller-provided request identity.
- `mission.mission_id`: authoritative Mission identity.
- `database`: path plus `read_only` or `read_write` access intent.
- `decision_context`: injected observation time, allocated IDs, and generations.
- `operation`: a tagged `handle`, `drive`, or `inspect` request.

`handle` accepts tagged `command`, `team_event`, `role_observation`, `effect_result`,
`tool_job_request`, and `tool_job_transition` inputs. Tool Job requests carry a typed Worker,
Assignment, argv, cwd, environment, mode, timeout, and output budget. Transitions are separately
identified and tagged as `started`, `cancel_requested`, or `completed`; completed transitions carry
a typed terminal state plus output paths, byte counts, truncation flags, checksums, exit code, and
error summary. Adapters must not infer Tool Job semantics from a generic Command body.

`drive` carries runtime owner and bounded effect/time budgets. `inspect` accepts `mission`, `status`,
`inbox`, `assignment_thread`, and `diagnostics` queries.

The library also defines versioned serde types for handle receipts, typed errors, drive reports,
Mission views, high-level effect intents, and generation-fenced effect results. Phase 2 deliberately
does not implement Mission state transitions: a recognized request returns the structured
`standalone_scaffold_only` error.

## Process contract

- stdin: exactly one JSON request document.
- stdout: exactly one JSON outcome document.
- stderr: diagnostics only.
- exit `64`: unsupported or mismatched operation.
- exit `65`: malformed JSON or invalid request schema.
- exit `66`: unknown protocol.
- exit `67`: incompatible binary contract.
- exit `70`: recognized request rejected by the standalone scaffold boundary.
