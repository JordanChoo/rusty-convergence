# Rusty Convergence

Rusty Convergence is the API-first implementation of
[Automated Plan Reviser Pro (APRP)](https://github.com/Dicklesworthstone/automated_plan_reviser_pro).
It is a Rust Cloudflare Worker that runs iterative specification review rounds
against direct LLM APIs, stores results in Cloudflare KV, and computes
convergence signals so callers can see when a plan is becoming stable.

The system is designed for coding agents, CI jobs, scripts, and other
automation. It does not drive a browser, depend on a ChatGPT web session, or
write to a local filesystem. A caller uploads source documents, defines a
workflow, asks the Worker to run round N, and receives a normalized JSON record
containing the review output, metrics, provider usage, and convergence data.

## Why This Exists

Complex technical plans rarely become good in one pass. Security protocols,
software architecture documents, migration plans, API designs, and product specs
usually improve through repeated review:

- early rounds find large architectural flaws and missing requirements
- middle rounds refine interfaces, invariants, and failure modes
- later rounds produce smaller edits as the design settles

Rusty Convergence automates that loop. Instead of a one-off prompt pasted into
a chat window, each review becomes a durable API workflow with history, retry
semantics, document bundling, provider abstraction, and convergence analytics.

Use it for:

- programmatic review rounds from a CLI, CI job, or agent orchestrator
- direct OpenAI and Anthropic API execution rather than browser automation
- durable round history without operating a database
- reproducible document bundles and prompt templates
- objective signals for whether another review round is likely to be useful
- a small deployable artifact that can run on Cloudflare Workers

## Relationship to the Original APR

The original
[Automated Plan Reviser Pro](https://github.com/Dicklesworthstone/automated_plan_reviser_pro)
is a Bash CLI tool that automates iterative specification review using GPT Pro
Extended Reasoning via Oracle browser automation. It runs on a developer's
machine, saves round outputs to the local filesystem, and relies on a human
(typically using Claude Code) to apply review feedback to source documents
between rounds.

Rusty Convergence reimplements that workflow as a deployed API service with
several structural differences:

| Aspect | Original APR | Rusty Convergence |
| --- | --- | --- |
| Runtime | Bash script, local machine | Rust Cloudflare Worker, deployed edge |
| LLM access | Browser automation (Oracle → ChatGPT) | Direct API (OpenAI + Anthropic) |
| State | Local filesystem (`.apr/` directory) | Cloudflare KV |
| Interface | Interactive TUI + JSON robot mode | HTTP API (JSON + SSE) |
| Providers | GPT Pro only | OpenAI and Anthropic, switchable per round |
| Batch execution | One round at a time | Auto-run with convergence stop, duration budgets |
| Document integration | Manual (human + Claude Code) | Automatic (Claude API) or manual (human via API) |
| Convergence | Same algorithm, same weights | Same algorithm, same weights |

The convergence algorithm (three weighted signals at 35/35/30) is identical.
The core insight that specifications converge through iterative review like a
numerical optimizer settling into a minimum is the same. The execution model
changed: from a local tool that drives a browser to a stateless service that
calls APIs directly.

## Core Concepts

### Workflow

A workflow is the saved configuration for a review loop. It names the LLM
provider and model, optional system prompt, provider-specific parameters, the
document roles to include, and the templates used to build prompts.

Workflows are stored in KV under `config::<workflow>`.

### Document

A document is raw markdown or text uploaded to a workflow under a role such as
`readme`, `spec`, or `impl`. The Worker stores documents in KV and substitutes
them into templates using `{{placeholder}}` syntax.

Documents are stored under `doc::<workflow>::<role>`.

### Round

A round is one LLM review execution for a workflow. Round 1 has no convergence
score because there is no previous output to compare against. Round 2 and later
are compared with earlier completed rounds to estimate whether the review loop
is converging.

Rounds are stored under `round::<workflow>::<number>`.

### Stats

Stats are the cached convergence view for a workflow. The Worker updates stats
after each completed round and can rebuild them from saved rounds if needed.

Stats are stored under `stats::<workflow>`.

### Integration Prompt

The integration endpoint wraps a completed round in instructions suitable for a
coding agent. The Worker stays focused on review generation; callers handle
source edits.

### Iteration Models

The Worker supports two complementary iteration models for multi-round review:

**Chained output** (`integration_mode: "none"`, the default): Documents stay
fixed in KV. Each round's output is fed into the next round via the
`{{previous_round}}` template placeholder. Iteration happens inside the prompt
context. This is simple, fast, and self-contained, with no extra API calls
between rounds.

**Document mutation** (`integration_mode: "claude"` or `"human"`): After each
round, source documents in KV are updated to reflect the review's improvements.
The next round reads the improved documents directly. Iteration happens through
actual document improvement. This matches the original APR workflow where a
human or coding agent applies feedback to source files between review sessions.

Both models use the same convergence algorithm. Chained output is better for
quick, low-cost iteration. Document mutation is better when the source material
should genuinely improve between rounds, or when multiple callers need to see
the latest version of the documents.

## Architecture

```
Client or agent
    |
    | HTTP + csvkey
    v
Cloudflare Worker (Rust)
    |
    | validate auth, route, method, names, round number
    v
KV-backed workflow and document loader
    |
    | render template with {{document}} placeholders
    v
Provider adapter
    |
    | OpenAI Chat Completions API or Anthropic Messages API
    v
Provider SSE parser
    |
    | normalized text, usage, completion status
    v
Round recorder
    |
    | metrics, convergence, stats, metadata
    v
Cloudflare KV
```

The Worker is stateless. All durable state is in a single KV namespace bound as
`APRP`. The code is organized around small modules:

```
src/
|-- lib.rs              # Worker entry point and route dispatch
|-- validation.rs       # csvkey auth, input validation, constant-time compare
|-- error.rs            # JSON response envelope and error helpers
|-- storage.rs          # KV key patterns, JSON/text helpers, locks
|-- prompt.rs           # Template parsing and document substitution
|-- metrics.rs          # Word/line/byte/heading metrics
|-- convergence.rs      # Three-signal convergence algorithm
|-- types.rs            # Workflow, Round, Stats, Usage, and related structs
|-- providers/
|   |-- mod.rs          # Shared SSE parsing and provider errors
|   |-- openai.rs       # OpenAI request building and stream parsing
|   `-- anthropic.rs    # Anthropic request building and stream parsing
`-- routes/
    |-- health.rs
    |-- workflows.rs
    |-- documents.rs
    |-- run.rs
    |-- auto.rs
    |-- rounds.rs
    |-- stats.rs
    `-- integrate.rs
```

## Design Principles

### Direct APIs, No Browser Automation

The Worker calls provider APIs directly. OpenAI calls use the Chat Completions
endpoint. Anthropic calls use the Messages endpoint. There is no dependency on
web UI state, cookies, browser profiles, local Node processes, or SSH access to
a machine running an interactive session.

### API-Only Execution

Every operation is available over HTTP. The Worker does not commit code, edit
documents, manage branches, or operate an interactive TUI. Callers can be shell
scripts, coding agents, scheduled CI jobs, dashboards, or one-off curl commands.

### Durable State, Stateless Compute

KV stores workflow config, source documents, round outputs, metadata, locks,
and stats. The Worker can be redeployed or scaled without moving state.

### Provider Isolation

Provider-specific request shapes and SSE formats are isolated behind adapter
modules. The route layer deals with normalized chunks:

- `Text`
- `Usage`
- `Done`

OpenAI and Anthropic use the same workflow logic, locking, persistence,
metrics, and convergence calculations.

### Conservative Overwrite Rules

Completed rounds cannot be overwritten. Failed or stale rounds can be retried.
Sequential enforcement requires round N-1 to be complete before round N, unless
the caller explicitly sets `skip_sequence_check: true`.

### Lightweight Convergence Signals

Convergence is intentionally simple enough to compute at the edge. It uses word
counts and word-set similarity rather than expensive semantic embeddings. The
score is a practical stop-or-continue signal for deciding whether another review
round is worth the cost.

### Separating Review from Integration

When document integration is active, the review round and the integration step
are architecturally separated. The review LLM (OpenAI or Anthropic, caller's
choice) produces analysis or a revised document. In Claude integration mode,
a separate Claude API call applies that output to the source documents. This
separation means the review provider and the integration provider can be
different models, the review prompt and the integration prompt serve different
purposes, and either step can be swapped independently. In human integration
mode, the human replaces the integration LLM entirely.

## Authentication

Every route except `GET /health` requires a `csvkey` query parameter.

```
GET /workflows?csvkey=<secret>
POST /run/my-spec/1?csvkey=<secret>
```

The expected value is stored as a Cloudflare Worker secret named `CSVKEY`.
Comparison uses a constant-time XOR-and-fold check to avoid early-exit timing
leaks.

`GET /health` remains public for version and KV checks. Supplying a valid
`csvkey` adds authenticated diagnostics for Worker secret presence; missing
provider secrets appear as response warnings without exposing secret values.

Treat this as single-tenant internal-tool authentication. Because the key is in
the URL, public or multi-tenant deployments need a different auth model.

## Response Envelope

JSON responses use one envelope shape:

```json
{
  "ok": true,
  "code": "ok",
  "data": {},
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": {
    "version": "0.2.0",
    "ts": "2026-06-02T14:30:00Z"
  }
}
```

Errors use the same shape with `ok: false`, `data: null`, a stable `code`, and
a human-readable `error`.

Common error codes:

| Code | HTTP | Meaning |
| --- | ---: | --- |
| `missing_csvkey` | 401 | Auth query parameter is absent or empty |
| `unauthorized` | 401 | Auth key does not match `CSVKEY` |
| `missing_config` | 500 | Required Worker secret is absent |
| `bad_request` | 400 | Malformed JSON, invalid fields, invalid provider, or invalid identifiers |
| `not_found` | 404 | Workflow, document, round, or route is missing |
| `conflict` | 409 | Round already complete or workflow is locked |
| `validation_failed` | 422 | Valid JSON, but not runnable as requested |
| `provider_error` | 502 | OpenAI or Anthropic returned or caused an error |
| `internal_error` | 500 | Unexpected Worker-side failure |
| `method_not_allowed` | 405 | Route exists but method is not accepted |

## API Reference

| Method | Route | Auth | Purpose |
| --- | --- | --- | --- |
| `GET` | `/health` | optional | Version and KV binding availability check; with a valid `csvkey`, includes provider secret diagnostics |
| `GET` | `/workflows` | yes | List workflow configs |
| `POST` | `/workflows` | yes | Create or update a workflow config |
| `GET` | `/workflows/:name` | yes | Read one workflow with derived metadata |
| `DELETE` | `/workflows/:name` | yes | Delete workflow state |
| `PUT` | `/documents/:workflow/:role` | yes | Upload one source document |
| `GET` | `/documents/:workflow/:role` | yes | Retrieve one source document as markdown |
| `POST` | `/run/:workflow/:round` | yes | Execute one review round |
| `GET` | `/rounds/:workflow` | yes | List rounds for a workflow |
| `GET` | `/rounds/:workflow/:round` | yes | Retrieve one round |
| `GET` | `/stats/:workflow` | yes | Read cached convergence analytics |
| `POST` | `/stats/:workflow/rebuild` | yes | Recompute stats from completed rounds |
| `POST` | `/integrate/:workflow/:round` | yes | Generate a coding-agent integration prompt |
| `POST` | `/auto/:workflow` | yes | Execute multiple review rounds with convergence stop |
| `POST` | `/auto/:workflow/resume` | yes | Resume a human-mode auto-run after document updates |

## Quick Start With Curl

Set the deployment URL and auth key:

```bash
export WORKER_URL="https://rusty-convergence.<your-subdomain>.workers.dev"
export CSVKEY="<your-shared-secret>"
```

Check health:

```bash
curl "$WORKER_URL/health"
```

Check authenticated provider secret diagnostics without making provider calls:

```bash
curl "$WORKER_URL/health?csvkey=$CSVKEY"
```

Upload source documents before creating the workflow. Workflow creation
validates that the referenced document roles already exist in KV.

```bash
curl -X PUT \
  -H "Content-Type: text/markdown" \
  --data-binary @README.md \
  "$WORKER_URL/documents/demo/readme?csvkey=$CSVKEY"

curl -X PUT \
  -H "Content-Type: text/markdown" \
  --data-binary @prd/prd.md \
  "$WORKER_URL/documents/demo/spec?csvkey=$CSVKEY"
```

Create an OpenAI workflow:

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{
    "name": "demo",
    "description": "Iterative review for the demo specification",
    "provider": "openai",
    "model": "<openai-model-id>",
    "system_prompt": "You are a careful systems architect and security reviewer.",
    "provider_params": {
      "reasoning_effort": "high",
      "max_completion_tokens": 4000
    },
    "documents": {
      "readme": "readme",
      "spec": "spec"
    }
  }' \
  "$WORKER_URL/workflows?csvkey=$CSVKEY"
```

Run round 1:

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{}' \
  "$WORKER_URL/run/demo/1?csvkey=$CSVKEY"
```

Run round 2 after round 1 is complete:

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{}' \
  "$WORKER_URL/run/demo/2?csvkey=$CSVKEY"
```

Inspect the result:

```bash
curl "$WORKER_URL/rounds/demo/2?csvkey=$CSVKEY"
curl -H "Accept: text/markdown" "$WORKER_URL/rounds/demo/2?csvkey=$CSVKEY"
curl "$WORKER_URL/stats/demo?csvkey=$CSVKEY"
```

Generate an integration prompt:

```bash
curl -X POST "$WORKER_URL/integrate/demo/2?csvkey=$CSVKEY"
```

## Anthropic Workflows

Anthropic is selected with `"provider": "anthropic"` and any Anthropic model id
enabled for your account.

For a separate Anthropic workflow, upload the same source documents under that
workflow name first:

```bash
curl -X PUT \
  -H "Content-Type: text/markdown" \
  --data-binary @README.md \
  "$WORKER_URL/documents/demo-claude/readme?csvkey=$CSVKEY"

curl -X PUT \
  -H "Content-Type: text/markdown" \
  --data-binary @prd/prd.md \
  "$WORKER_URL/documents/demo-claude/spec?csvkey=$CSVKEY"
```

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{
    "name": "demo-claude",
    "provider": "anthropic",
    "model": "<anthropic-model-id>",
    "system_prompt": "You are a careful systems architect and security reviewer.",
    "provider_params": {
      "max_tokens": 4000,
      "thinking": {
        "type": "enabled",
        "budget_tokens": 2000
      }
    },
    "documents": {
      "readme": "readme",
      "spec": "spec"
    }
  }' \
  "$WORKER_URL/workflows?csvkey=$CSVKEY"
```

The Anthropic adapter sends:

- `x-api-key: <ANTHROPIC_API_KEY>`
- `anthropic-version: 2023-06-01`
- `stream: true`
- a `system` field when configured
- one user message containing the rendered prompt

Thinking blocks are parsed but not stored as round content. Only text blocks are
captured in the saved round output.

## Per-Run Provider Overrides

The `POST /run/:workflow/:round` request body can override provider settings
without changing the saved workflow:

```json
{
  "provider": "anthropic",
  "model": "<anthropic-model-id>",
  "include_impl": true,
  "skip_sequence_check": false,
  "system_prompt": "Review this as a production readiness gate.",
  "provider_params": {
    "max_tokens": 4000
  }
}
```

Use overrides to compare OpenAI and Anthropic on the same documents, try a
higher reasoning budget for one round, or force implementation context into a
specific review.

Provider params are passed through with protected fields blocked:

| Provider | Protected fields |
| --- | --- |
| OpenAI | `model`, `stream`, `stream_options`, `messages` |
| Anthropic | `model`, `stream`, `messages`, `system` |

The route layer owns those fields, which prevents callers from replacing the
selected model or rendered document bundle by accident.

## Auto-Run: Multi-Round Batch Execution

`POST /auto/:workflow` runs multiple review rounds sequentially with automatic
convergence detection, duration budgets, and optional cross-model rotation.

### Request Body

```json
{
  "rounds": 5,
  "min_rounds": 3,
  "stop_on_convergence": true,
  "convergence_threshold": 0.90,
  "max_duration_seconds": 300,
  "integration_mode": "claude",
  "integration_model": "claude-sonnet-4-6",
  "include_integration": true,
  "provider_rotation": [
    { "provider": "openai", "model": "o3", "provider_params": { "reasoning_effort": "high" } },
    { "provider": "anthropic", "model": "claude-opus-4-6" }
  ]
}
```

| Field | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `rounds` | integer | yes | — | Rounds to execute (1–MAX_AUTO_ROUNDS) |
| `min_rounds` | integer | no | `1` | Minimum rounds before convergence check |
| `stop_on_convergence` | bool | no | `true` | Stop when score >= threshold |
| `convergence_threshold` | float | no | `0.90` | Score threshold for early stop |
| `max_duration_seconds` | integer | no | — | Wall-clock budget in seconds (>= 30) |
| `include_integration` | bool | no | `false` | Include integration prompt in response |
| `integration_mode` | string | no | `"none"` | Document integration between rounds: `none`, `claude`, or `human` |
| `integration_model` | string | no | `"claude-sonnet-4-6"` | Claude model for document integration (when `integration_mode` is `claude`) |
| `provider_rotation` | array | no | — | Cycle providers per round |
| `include_impl` | bool | no | — | Same as POST /run override |
| `system_prompt` | string | no | — | Same as POST /run override |

`provider_rotation` is mutually exclusive with top-level `provider`, `model`,
and `provider_params`. Use one or the other.

Note: `include_integration` and `integration_mode` are unrelated despite
similar names. `include_integration` controls whether a coding-agent prompt is
appended to the response (via the `/integrate` endpoint logic).
`integration_mode` controls whether source documents are mutated between
rounds.

### Response Format

The default response uses Server-Sent Events (SSE). Each round emits
`round_start` and `round_complete` events. A final `done` event contains the
aggregate result. Request with `Accept: application/json` for a buffered JSON
response instead.

SSE events:

```
event: round_start
data: {"round":1,"started_at":"2026-06-03T10:05:00Z"}

event: round_complete
data: {"round":1,"words":4000,"score":null,"recommendation":null,"duration_seconds":45,"provider":"openai","model":"o3"}

event: integration_start
data: {"round":2}

event: integration_complete
data: {"round":2,"documents_updated":["readme","spec"],"duration_seconds":12}

event: done
data: {"rounds_completed":3,"stopped_reason":"convergence","final_round":{...},"rounds_summary":[...],"total_usage":{...},"total_duration_seconds":118}
```

Integration events (`integration_start`, `integration_complete`,
`integration_error`) appear only when `integration_mode` is `"claude"`.

`stopped_reason` values: `completed`, `convergence`, `duration_limit`, `error`,
`awaiting_integration` (human mode paused for document updates),
`integration_error` (Claude integration call failed).

### Starting Round Detection

The endpoint automatically detects where to resume. It reads the workflow
metadata and verifies against actual round records. If metadata is stale or
missing, it scans forward to find the correct starting point.

### Examples

Basic auto-run:

```bash
curl -N -X POST \
  -H "Content-Type: application/json" \
  -d '{"rounds": 5}' \
  "$WORKER_URL/auto/demo?csvkey=$CSVKEY"
```

JSON response with convergence stop and integration prompt:

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"rounds": 10, "min_rounds": 3, "convergence_threshold": 0.85, "include_integration": true}' \
  "$WORKER_URL/auto/demo?csvkey=$CSVKEY"
```

Cross-model rotation with duration budget:

```bash
curl -N -X POST \
  -H "Content-Type: application/json" \
  -d '{
    "rounds": 10,
    "max_duration_seconds": 300,
    "provider_rotation": [
      {"provider": "openai", "model": "o3"},
      {"provider": "anthropic", "model": "claude-opus-4-6"}
    ]
  }' \
  "$WORKER_URL/auto/demo?csvkey=$CSVKEY"
```

## Document Integration

Auto-run supports two document integration modes that mutate source documents
between rounds, following the iteration model from the original APR project.
When integration is active, the review output from each round is used to
improve the actual source documents in KV before the next round begins. This
means each subsequent round reviews better material instead of chained LLM
output.

### Why Document Mutation Matters

Without integration, iterative refinement happens entirely inside the prompt.
The `{{previous_round}}` placeholder feeds each round's output into the next
round's context, but the source documents (the README, spec, and any
implementation doc) stay fixed at their originally uploaded versions. The LLM
sees the same original spec every round, even after it has already produced an
improved version. Its improvements live only in the round output chain.

With document integration, the source documents themselves improve after each
round. The README, spec, and implementation doc in KV are updated to reflect
the latest improvements. The next round reads better source material, so the
LLM can focus on further refinement rather than re-deriving improvements
already captured in prior rounds. This is the same pattern that made the
original APR workflow effective: review, integrate, review the improved
version, integrate again.

### Claude Integration Mode

When `integration_mode` is `"claude"`, the Worker calls the Anthropic Messages
API (non-streaming) after each round to apply the review improvements to every
document in the workflow. The integration uses a separate Claude call per
document with a focused prompt that instructs the model to apply relevant
improvements and return the complete updated document.

The integration model defaults to `claude-sonnet-4-6` for speed and cost
efficiency, since document integration is a focused editing task rather than
open-ended reasoning. Override with `integration_model` if needed.

```bash
curl -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{
    "rounds": 5,
    "integration_mode": "claude",
    "integration_model": "claude-sonnet-4-6"
  }' \
  "$WORKER_URL/auto/demo?csvkey=$CSVKEY"
```

The response includes `integration_usage` with the token counts consumed by
integration calls, separate from the review round usage in `total_usage`. If an
integration call fails, the auto-run stops with `stopped_reason:
"integration_error"` and all completed rounds are preserved.

Claude integration SSE events:

```
event: integration_start
data: {"round":2}

event: integration_complete
data: {"round":2,"documents_updated":["readme","spec"],"duration_seconds":12}
```

### Human Integration Mode

When `integration_mode` is `"human"`, the auto-run pauses after each round
with `stopped_reason: "awaiting_integration"`. The caller is expected to update
documents using the existing `PUT /documents` endpoint, then resume the
auto-run.

```bash
# Start auto-run with human integration
curl -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"rounds": 5, "integration_mode": "human"}' \
  "$WORKER_URL/auto/demo?csvkey=$CSVKEY"
```

The response includes a `next_round` field and a hint with the resume endpoint:

```json
{
  "ok": true,
  "data": {
    "stopped_reason": "awaiting_integration",
    "rounds_completed": 1,
    "next_round": 2,
    "integration_mode": "human"
  },
  "hint": "Update documents via PUT /documents/demo/{role}, then POST /auto/demo/resume to continue"
}
```

After updating documents:

```bash
# Update the spec based on the round's review output
curl -X PUT \
  -H "Content-Type: text/markdown" \
  --data-binary @updated-spec.md \
  "$WORKER_URL/documents/demo/spec?csvkey=$CSVKEY"

# Resume the auto-run
curl -X POST \
  -H "Accept: application/json" \
  "$WORKER_URL/auto/demo/resume?csvkey=$CSVKEY"
```

Each resume runs one round, then pauses again for the next integration, unless
the round converges or the requested round count is reached. In either of those
cases the auto-run completes normally. The full loop state is persisted in KV under
`autorun::<workflow>` and cleaned up when the auto-run finishes.

Human mode requires `Accept: application/json` because pause/resume is
incompatible with SSE streaming.

### Integration Mode Comparison

| Aspect | `none` | `claude` | `human` |
| --- | --- | --- | --- |
| Documents between rounds | Fixed | Mutated by Claude API | Mutated by caller |
| Auto-run behavior | Continuous | Continuous with integration calls | Pause/resume per round |
| Extra API calls | None | One Claude call per document per round | None (caller updates docs) |
| Response format | JSON or SSE | JSON or SSE | JSON only |
| Resume endpoint | N/A | N/A | `POST /auto/:workflow/resume` |
| Best for | Fast iteration, chained output | Fully automated document improvement | Human-in-the-loop review |

## Prompt Templates

Templates use `{{placeholder}}` syntax. Placeholders refer to workflow document
roles, and roles map to uploaded document ids.

```json
{
  "documents": {
    "readme": "readme",
    "spec": "spec",
    "implementation": "impl"
  },
  "template": "Read the README:\n\n{{readme}}\n\nReview the spec:\n\n{{spec}}",
  "template_with_impl": "Read implementation notes:\n\n{{implementation}}\n\nReview the spec:\n\n{{spec}}",
  "impl_every_n": 4
}
```

### Default Template Design

If `template` is omitted, the Worker uses built-in templates designed for
iterative document refinement. The default template instructs the LLM to:

1. Read the README for project context
2. Read the original specification as a reference baseline
3. Read the current working version (via `{{previous_round}}`)
4. Produce a complete, improved version of the document

The templates ask for the **entire revised document** rather than a list of
suggestions or diffs. Each round's output is therefore a self-contained
specification that can be stored, compared, and used directly.
Convergence metrics are computed against the full output text, and the document
can be fed back into the next round without manual assembly.

The default implementation template follows the same pattern but adds the
implementation document as additional context, with instructions to keep the
specification implementable.

Both templates reference `{{readme}}`, `{{spec}}`, and `{{previous_round}}`.
The implementation template also references `{{implementation}}`.

For the simplest workflows, pass an explicit `documents` map as shown above. If
`documents` is omitted, the Worker derives default role mappings from the
built-in templates. The built-in implementation template is available for
implementation rounds, so the default map can include `"implementation":
"impl"`. Upload `/documents/<workflow>/impl` as well, or provide an explicit
`documents` map when a workflow only needs README and spec documents.

Implementation context is selected when:

- `impl_every_n` divides the round number, such as rounds 4, 8, and 12
- `include_impl: true` is supplied to `POST /run`

Warnings are returned when a configured role is not referenced by the selected
template. Missing referenced roles or missing uploaded documents are hard
validation errors.

### Iterative Refinement with {{previous_round}}

The built-in default templates use `{{previous_round}}` to enable iterative
refinement. Each round receives the prior round's output as the "current
working version" of the document, producing a progressively improved revision
rather than independent samples. The LLM is asked to output the entire revised
document (not a list of suggestions or diffs), so each round's output is a
complete, self-contained specification.

On round 1, `{{previous_round}}` substitutes the original spec document
content. On round N, it injects round N-1's completed output. This means
round 1 improves the original spec, round 2 improves round 1's output, and so
on.

`{{previous_round}}` is a synthetic placeholder; it does not require a
documents map entry. It works with both `POST /run` and `POST /auto`.

Custom templates can omit `{{previous_round}}` if independent-sample behavior
is preferred:

```json
{
  "template": "Read the README:\n\n{{readme}}\n\nReview the spec:\n\n{{spec}}"
}
```

## Round Lifecycle

`POST /run/:workflow/:round` performs these steps:

1. Validate workflow name and round number.
2. Load the workflow config from KV.
3. Parse optional run overrides.
4. Enforce sequential rounds unless `skip_sequence_check` is true.
5. Check whether the target round is complete, running, failed, or stale.
6. Acquire a workflow lock with `DEFAULT_LOCK_TTL_SECONDS`.
7. Select the template and implementation inclusion mode.
8. Render the prompt by reading documents from KV.
9. Write a `running` round record.
10. Call the selected provider API with `stream: true`.
11. Parse provider SSE events into normalized text and usage.
12. Reject empty or incomplete provider responses.
13. Compute document metrics.
14. Compute convergence against cached stats.
15. Save the completed round.
16. Update stats and workflow metadata.
17. Release the lock.
18. Return the normalized JSON response.

Provider APIs stream to the Worker. The Worker parses those SSE responses
internally, saves the round, and then returns the public `POST /run` response.

## Retry and Lock Semantics

Round behavior is deliberately conservative:

| Existing round status | `POST /run` behavior |
| --- | --- |
| missing | proceed |
| `complete` | `409 conflict` |
| `running` within TTL | `409 conflict` |
| `running` beyond TTL | retry allowed |
| `failed` | retry allowed |
| `stale` | retry allowed |

KV does not provide compare-and-swap, so locks are best-effort. In the
single-tenant use case, the lock prevents accidental duplicate runs and keeps
normal retry behavior predictable.

## Convergence Algorithm

### The Optimization Analogy

Iterative specification refinement behaves like a numerical optimizer
converging on a steady state. Early rounds produce large, sweeping changes:
architectural overhauls, security gap fixes, missing-section additions. As the
design stabilizes, each round produces smaller adjustments: tighter
definitions, edge case handling, polished abstractions. Eventually the changes
become negligible and further rounds add cost without value.

This is analogous to gradient descent settling into a minimum. The
specification is the parameter vector, each review round is a gradient step,
and convergence means the gradient magnitude has dropped below a useful
threshold. The convergence algorithm detects this transition point
automatically so callers know when to stop investing in additional rounds.

### Three-Signal Scoring

Convergence is computed after every completed round starting with round 2.

```
score = (0.35 * output_trend)
      + (0.35 * change_velocity)
      + (0.30 * similarity_trend)
```

Each signal measures a different aspect of stabilization:

**Output Trend (weight: 0.35)** measures whether the review output is getting
shorter relative to the longest output seen. Early rounds tend to produce
lengthy analyses because there is more to fix. As the specification improves,
the reviewer has less to say and outputs shrink. The formula is
`1.0 - (latest_words / max_words)`, clamped to `[0.0, 1.0]`.

**Change Velocity (weight: 0.35)** measures whether the magnitude of change
between consecutive rounds is decreasing. It computes the absolute word-count
delta between each pair of adjacent rounds and compares the latest delta to the
largest delta seen. The formula is `1.0 - (latest_delta / max_delta)`. When the
latest change is the smallest change so far, this signal approaches 1.0.

**Similarity Trend (weight: 0.30)** measures vocabulary overlap between
consecutive rounds using Jaccard similarity. Content is tokenized to lowercase
alphanumeric words (punctuation stripped, duplicates collapsed into a set), and
the Jaccard index is `|intersection| / |union|`. A score of 1.0 means
identical word sets; 0.0 means completely disjoint vocabulary. This catches
cases where word count stays stable but the content itself is churning.

### Why These Three Signals

No single signal is sufficient:

- Output length alone can be gamed by a terse model or an overly verbose one.
- Change velocity alone misses cases where a model rewrites content without
  changing total word count.
- Vocabulary similarity alone misses structural reorganization that uses the
  same words in different arrangements.

The weighted combination is more reliable than any single signal. All three
rising together is strong evidence that the document has genuinely stabilized. The weights (35/35/30)
give equal importance to the two quantitative signals and slightly less to the
set-based signal, which is noisier on short documents.

### Edge-Friendly Design

The algorithm is intentionally simple enough to run at the Cloudflare Workers
edge without external dependencies. It uses no vector embeddings, no semantic
similarity models, and no GPU. Word counts and set operations are O(n) in
document length and require no persistent compute beyond what KV provides.

The stats cache stores only the latest word set for the next similarity
calculation, not the full history. The Worker does not need to re-read all
prior round content after every run.

### Recommendation Thresholds

| Score | Estimated remaining rounds | Recommendation |
| ---: | --- | --- |
| `>= 0.90` | `0` | `stop` |
| `>= 0.75` | `1-2` | `almost` |
| `>= 0.50` | `3-5` | `continue` |
| `< 0.50` | `5+` | `early` |

Round 1 returns `null` convergence fields because there is no prior round to
compare against.

## Document Metrics

Each completed round stores:

| Metric | Meaning |
| --- | --- |
| `words` | Whitespace-delimited token count |
| `lines` | Line count |
| `characters` | UTF-8 byte length |
| `headings` | Markdown headings matching `#` through `######` with a space |

These metrics feed convergence calculations and make round records easier to
inspect.

## Storage Schema

All keys live in the `APRP` KV namespace.

| Key pattern | Value |
| --- | --- |
| `config::<workflow>` | Workflow JSON |
| `doc::<workflow>::<role>` | Raw document text |
| `round::<workflow>::<N>` | Round JSON |
| `meta::<workflow>` | Round count, latest round, latest convergence |
| `stats::<workflow>` | Cached convergence analytics |
| `lock::<workflow>` | Active run lock |
| `autorun::<workflow>` | Human-mode auto-run state for pause/resume |

Document uploads are capped by `MAX_DOCUMENT_BYTES`, which defaults to
`1048576` bytes. Small documents under 500 bytes succeed with a warning because
that often indicates a stub or wrong file was uploaded.

## Deployment

Prerequisites:

- Rust stable toolchain
- `wasm32-unknown-unknown` target
- `worker-build`
- Wrangler CLI
- Cloudflare Workers account
- Cloudflare KV namespace

Create the KV namespace:

```bash
wrangler kv namespace create APRP
```

Put the returned namespace id into `wrangler.toml`:

```toml
[[kv_namespaces]]
binding = "APRP"
id = "<namespace-id>"
```

Set secrets:

```bash
wrangler secret put CSVKEY
wrangler secret put OPENAI_API_KEY
wrangler secret put ANTHROPIC_API_KEY
```

Deploy:

```bash
wrangler deploy
```

Verify the deployed Worker:

```bash
./scripts/verify-deploy.sh "https://rusty-convergence.<your-subdomain>.workers.dev" "$CSVKEY"
```

The verifier includes an authenticated health preflight that checks whether
`OPENAI_API_KEY` and `ANTHROPIC_API_KEY` are configured before any provider
calls are attempted.

## Local Development

Create `.dev.vars` for local use:

```env
CSVKEY=test-key-for-dev
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
```

Start Wrangler:

```bash
wrangler dev
```

Run local checks:

```bash
cargo fmt --check
cargo test --all-targets
worker-build --release
wrangler deploy --dry-run
```

The repository also includes deployed-worker smoke tests:

```bash
./scripts/verify-deploy.sh "$WORKER_URL" "$CSVKEY"
./tests/e2e_error_sweep.sh
CSVKEY="$CSVKEY" \
WORKER_URL="$WORKER_URL" \
ANTHROPIC_MODEL="<anthropic-model-id>" \
./tests/e2e_real_llm.sh
```

The real-LLM test first checks authenticated health diagnostics to confirm the
deployed Worker has `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` configured. It then
makes billable provider calls, creates separate OpenAI and Anthropic workflows,
runs live rounds, validates persisted round data, checks stats, and cleans up
the workflows at exit.

## Security Model

- Single shared `CSVKEY` authenticates all non-health routes.
- Provider keys are Worker secrets and are not stored in KV.
- User documents and LLM outputs are stored in the configured KV namespace.
- Workflow names, role names, and round numbers are validated before storage.
- Document size is bounded by `MAX_DOCUMENT_BYTES`.
- Provider errors are truncated before they are returned to callers.
- The service sends no CORS headers; it is intended for server-side and CLI
  callers rather than direct browser use.

## Operational Notes

- Upload documents before creating a workflow that references them.
- Use model ids enabled for the relevant provider account.
- Use `skip_sequence_check` only for backfills or intentional experiments.
- Rebuild stats with `POST /stats/:workflow/rebuild` if stats and rounds ever
  become inconsistent.
- Retrieve round markdown with `Accept: text/markdown` when you want only the
  LLM output without the JSON envelope.
- Delete workflows through the API rather than deleting individual KV keys by
  hand.

## What This Does Not Do

Rusty Convergence leaves these responsibilities to callers or companion tools:

- no browser automation
- no local project filesystem access at runtime
- no git commits, branches, pushes, or patches
- no dashboard or TUI
- no multi-user account model
- no prompt library management beyond saved workflow templates
- no automatic retry of expensive LLM calls
- no server-side diff rendering

The Worker stays small, durable, and easy to operate.

## License

This repository is currently developed as project infrastructure for Rusty
Convergence/APRP. Add a license file before distributing it outside the current
project context.
