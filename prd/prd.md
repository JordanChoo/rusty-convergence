# Automated Plan Reviser Pro (APRP) — Rust Cloudflare Worker

## Product Requirements Document

**Version:** 0.1.0
**Date:** 2026-06-02
**Status:** Draft

---

## 1. Problem Statement

Complex specifications — particularly security-focused protocols, system architectures, and API designs — require multiple rounds of AI review to converge on optimal architecture. Early rounds fix major structural flaws, middle rounds refine interfaces, and later rounds polish details.

The original APR (a ~6,900-line Bash CLI) automates this iterative refinement cycle using browser automation (Oracle) to drive ChatGPT's webapp. This approach is fragile: it depends on browser state, session cookies, webapp UI stability, and a Node.js runtime. It cannot be consumed programmatically by distributed systems, CI pipelines, or multi-agent orchestrations without SSH access to the machine running it.

APRP replaces the entire stack with a single Rust-based Cloudflare Worker that calls LLM APIs directly. No browser automation, no Node.js, no filesystem — just HTTP in, HTTP out, with durable storage via Cloudflare KV.

---

## 2. Goals

1. **Functional parity with APR's core loop** — bundle documents, send to an LLM with extended reasoning, capture the response, track round history, compute convergence.
2. **Direct API integration** — call OpenAI and Anthropic APIs directly via HTTP, eliminating Oracle/browser dependencies entirely.
3. **Durable and stateless** — all state lives in KV. The Worker itself is stateless and horizontally scalable.
4. **Programmatic-first** — JSON API designed for consumption by coding agents, CI pipelines, and automation scripts. Human-friendly output is a secondary concern handled by callers.
5. **Single deployable artifact** — one `wrangler deploy` puts the entire system live. No databases, queues, or external services beyond KV and the LLM APIs.
6. **Convergence analytics** — replicate APR's three-signal convergence algorithm so callers know when to stop iterating.

## 3. Non-Goals

1. **Interactive TUI/dashboard** — no terminal UI. Callers that want dashboards build them on top of the API.
2. **Git integration** — no commits, branches, or push operations. The caller manages their own repository.
3. **Document editing** — the Worker does not modify source documents. It produces revision suggestions that the caller applies.
4. **Multi-tenancy** — single-tenant. One shared key, one set of workflows. Multi-tenancy is a future concern.
5. **Prompt engineering** — the Worker uses caller-provided templates. It does not iterate on prompt quality.
6. **Diff rendering** — the Worker returns raw markdown for each round. Callers that want diffs compute them client-side.

---

## 4. Architecture Overview

```
                                    +---------------------------+
                                    |   Cloudflare Worker (Rust) |
                                    |                           |
  POST /run/spec/5?csvkey=K  ----->|  csvkey Validator          |
                                    |    |                      |
                                    |    v                      |
                                    |  Route Handler            |
                                    |    |                      |
                                    |    v                      |
                                    |  Doc Bundler              |
                                    |    | (reads docs from KV) |
                                    |    v                      |
                                    |  Prompt Builder           |
                                    |    | (applies template)   |
                                    |    v                      |
                                    |  LLM Adapter              |
                                    |    | (OpenAI or Anthropic) |
                                    |    | (streaming fetch)    |
                                    |    v                      |
                                    |  Stream-Through Response  |
                                    |    | (pipes to client)    |
                                    |    v                      |
                                    |  On-Complete Hook         |
                                    |    | (save to KV, compute)|
                                    |    v                      |
                                    |  Convergence Calculator   |
                                    |                           |
                                    +---------------------------+
                                              |
                                    +---------+---------+
                                    |                   |
                              +-----+-----+       +-----+-----+
                              |  KV Store |       |  LLM APIs |
                              |           |       |           |
                              | config::* |       | OpenAI    |
                              | doc::*    |       | Anthropic |
                              | round::*  |       |           |
                              | meta::*   |       +-----------+
                              | stats::*  |
                              +-----------+
```

### Runtime Model

The Worker runs on Cloudflare's edge network. Each request is handled by a single isolate. The paid Workers plan provides:

- **30 seconds CPU time** per invocation (I/O wait does not count)
- **128 MB memory**
- **25 MB max KV value size**
- **No wall-clock limit** for I/O-bound operations (streaming from LLM APIs)

LLM API calls are I/O-bound (streaming SSE responses over minutes). The Worker consumes negligible CPU time while waiting on the stream. This makes extended reasoning calls (10-60 minutes) feasible within a single Worker invocation.

### Streaming Model

The Worker uses **synchronous streaming**: the LLM's SSE stream is piped directly back to the client as a `text/event-stream` response. The Worker stays alive for the duration of the stream because the HTTP response is still being written. The client sees tokens arrive in real-time.

On stream completion, the Worker writes the buffered response to KV, computes convergence, and updates the stats cache — all before the response finalizes.

This avoids the `waitUntil` durability problem: Cloudflare does not guarantee that `waitUntil` keeps an isolate alive for 60 minutes after the response is sent. By keeping the response open, the Worker's lifetime is tied to the LLM stream itself.

**Tradeoff:** The client must hold the HTTP connection open for the full duration. If the client disconnects, the stream is lost. This is acceptable for v0.1's single-tenant, programmatic-caller use case. Callers that need fire-and-forget semantics should retry on disconnect.

**Stuck-run recovery:** If a client disconnects mid-stream and the round record was written with `status: "running"` before the stream started, it will appear stuck. The `GET /rounds` endpoint applies a heuristic: if a round has been `"running"` longer than the lock TTL (default: 60 minutes), it is reported as `status: "stale"`. The caller can then retry with `POST /run`, which is allowed to overwrite `"stale"` rounds (see section 5.4).

### API Versioning

v0.x is pre-stable. Breaking changes are expected between minor versions. The response envelope includes `"version": "0.1.0"` so callers can detect the deployed version. URL-based versioning (`/v1/`) is deferred to v1.0 when the API stabilizes. Callers should pin to a known Worker deployment, not assume URL stability.

---

## 5. API Design

### 5.1 Authentication

All requests require a `csvkey` query parameter, except `GET /health` which is unauthenticated.

The expected key is stored as a Cloudflare Worker secret (`CSVKEY`). This follows the same pattern as [rusty-gateway](https://github.com/JordanChoo/rusty-gateway).

```
GET /workflows?csvkey=<secret>
POST /run/fcp-spec/5?csvkey=<secret>
```

**Validation flow (executed before any other processing):**

1. Extract `csvkey` from query parameters. If missing or empty, return `401` with code `missing_csvkey`.
2. Read the `CSVKEY` secret from the Worker environment. If the secret is not configured, return `500` with code `missing_config` and log `missing_secret: CSVKEY` (never log the value).
3. Compare the provided key against the expected key using **constant-time comparison** (XOR-and-fold) to prevent timing side-channel attacks. If mismatch, return `401` with code `unauthorized`.

```rust
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
```

This is query-parameter auth, not header-based bearer token auth. The key travels in the URL. This is acceptable for single-tenant internal tooling over HTTPS. Do not use this pattern for multi-tenant or public-facing APIs.

### 5.2 Response Envelope

All JSON responses use a consistent envelope:

```json
{
  "ok": true,
  "code": "ok",
  "data": { },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": {
    "version": "0.1.0",
    "ts": "2026-06-02T14:30:00Z"
  }
}
```

- `warnings` — non-fatal issues detected during processing (e.g., a configured document not referenced by the template). Always present. Empty array when there are no warnings.
- `hint` — optional actionable guidance for the caller. Present on error responses to suggest recovery steps. `null` on success unless there's something the caller should know. Examples: `"Run POST /workflows to create a workflow first"`, `"Use POST /run/:workflow/:round to retry this round"`.

Error responses:

```json
{
  "ok": false,
  "code": "not_found",
  "data": null,
  "warnings": [],
  "hint": "Use POST /run/fcp-spec/5 to create this round.",
  "error": "Round 5 does not exist for workflow 'fcp-spec'",
  "meta": {
    "version": "0.1.0",
    "ts": "2026-06-02T14:30:00Z"
  }
}
```

### 5.3 Error Codes

| Code | HTTP Status | Meaning |
|------|-------------|---------|
| `ok` | 200 | Success |
| `missing_csvkey` | 401 | The `csvkey` query parameter is missing or empty |
| `unauthorized` | 401 | The `csvkey` value does not match the expected secret |
| `missing_config` | 500 | A required Worker secret (`CSVKEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) is not configured |
| `bad_request` | 400 | Malformed request body, missing required fields, or invalid HTTP method |
| `not_found` | 404 | Workflow or round does not exist |
| `conflict` | 409 | Round already completed or another run is in progress |
| `validation_failed` | 422 | Pre-flight validation failed (missing docs, invalid config) |
| `provider_error` | 502 | LLM API returned an error |
| `provider_timeout` | 504 | LLM API stream timed out |
| `internal_error` | 500 | Unexpected failure |
| `method_not_allowed` | 405 | HTTP method not supported for this route |

All error responses use the standard envelope (section 5.2) with `ok: false` and a human-readable `error` message. The `code` field is machine-parseable and stable across versions.

### 5.4 HTTP Method Validation

Each route accepts only its documented HTTP method. Any other method returns `405 Method Not Allowed` with code `method_not_allowed`. This check happens before authentication — a `POST` to `GET /health` returns 405 without requiring `csvkey`.

| Route | Accepted Methods |
|-------|-----------------|
| `/health` | GET |
| `/workflows` | GET, POST |
| `/workflows/:name` | GET, DELETE |
| `/documents/:workflow/:role` | GET, PUT |
| `/run/:workflow/:round` | POST |
| `/rounds/:workflow` | GET |
| `/rounds/:workflow/:round` | GET |
| `/stats/:workflow` | GET |
| `/stats/:workflow/rebuild` | POST |
| `/integrate/:workflow/:round` | POST |

Unmatched paths return `404 Not Found` with code `not_found`.

### 5.5 Routes

---

#### `GET /health`

Health check endpoint. **No `csvkey` required.** This is the only unauthenticated route.

```
GET /health
```

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "version": "0.1.0",
    "kv_accessible": true
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": {
    "version": "0.1.0",
    "ts": "2026-06-02T14:30:00Z"
  }
}
```

The `kv_accessible` field is set by attempting a lightweight KV read. If KV is unreachable, the endpoint still returns `200 OK` but with `"kv_accessible": false`.

---

#### `POST /workflows`

Create or update a workflow configuration.

When creating, the Worker validates that all document roles referenced in the `documents` map exist in KV. If any are missing, the request fails with `validation_failed` and lists the missing roles. When updating, existing documents are re-validated.

**Request:**

```json
{
  "name": "fcp-spec",
  "description": "Flywheel Connector Protocol specification refinement",
  "provider": "openai",
  "model": "o3",
  "system_prompt": "You are an expert systems architect and security engineer reviewing a protocol specification. Focus on correctness, security, performance, and API ergonomics.",
  "provider_params": {
    "reasoning_effort": "high",
    "max_completion_tokens": 32000,
    "stream_options": { "include_usage": true }
  },
  "documents": {
    "readme": "readme",
    "spec": "spec",
    "implementation": "impl"
  },
  "template": "First, read this README:\n\n```\n{{readme}}\n```\n\n---\n\nNOW: Carefully review this entire plan...\n\n```\n{{spec}}\n```",
  "template_with_impl": "First, read this README:\n\n```\n{{readme}}\n```\n\n---\n\nHere is the implementation:\n\n```\n{{implementation}}\n```\n\n---\n\nNOW: Carefully review...\n\n```\n{{spec}}\n```",
  "impl_every_n": 4
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique workflow identifier (kebab-case, alphanumeric/hyphens/underscores, max 64 chars) |
| `description` | string | no | Human-readable description |
| `provider` | string | yes | `"openai"` or `"anthropic"` |
| `model` | string | yes | Model identifier (e.g., `"o3"`, `"claude-opus-4-6"`) |
| `system_prompt` | string | no | System-level instructions prepended to every request. Sent as a `system` message (Anthropic) or `system` role message (OpenAI). If absent, no system message is sent. |
| `provider_params` | object | no | Provider-specific parameters passed through to the API. For OpenAI, include `stream_options: { include_usage: true }` to capture token counts. |
| `documents` | object | yes | Map of document role names (e.g., `"readme"`, `"spec"`, `"impl"`). Values are role identifiers — the KV key is auto-constructed as `doc::{workflow}::{role}`. |
| `template` | string | no | Prompt template with `{{placeholder}}` variables matching document role names. If omitted, the built-in default template is used (see section 8.5). |
| `template_with_impl` | string | no | Alternate template used when implementation is included. If omitted and `template` is also omitted, the built-in default impl template is used. |
| `impl_every_n` | integer | no | Auto-include implementation document every N rounds (0 or omitted = never) |

**Response:** `200 OK` with the saved config in `data`.

**Validation on save:**
- All document roles referenced in the `template` must exist in the `documents` map
- All document roles referenced in `template_with_impl` (if provided) must exist in the `documents` map
- All document roles in the `documents` map must have corresponding documents uploaded to KV (`doc::{workflow}::{role}`)
- If a document role is defined in `documents` but not referenced by any template, a warning is returned: `"Document role 'implementation' is configured but not referenced by any template"`

---

#### `GET /workflows`

List all workflow configurations.

**Query parameters:**

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `limit` | integer | 100 | Max workflows to return (max: 100) |
| `cursor` | string | none | Pagination cursor from a previous response |

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflows": [
      {
        "name": "fcp-spec",
        "description": "Flywheel Connector Protocol specification refinement",
        "provider": "openai",
        "model": "o3",
        "round_count": 7,
        "latest_round": 7,
        "convergence_score": 0.72
      }
    ],
    "cursor": null
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

`cursor` is `null` when there are no more results. If non-null, pass it as `?cursor=...` on the next request to get the next page.

---

#### `GET /workflows/:name`

Get a single workflow configuration with full detail.

**Response:** `200 OK`

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "name": "fcp-spec",
    "description": "Flywheel Connector Protocol specification refinement",
    "provider": "openai",
    "model": "o3",
    "system_prompt": "You are an expert systems architect...",
    "provider_params": { "reasoning_effort": "high", "max_completion_tokens": 32000 },
    "documents": { "readme": "readme", "spec": "spec", "implementation": "impl" },
    "template": "First, read this README:\n\n...",
    "template_with_impl": "First, read this README:\n\n...",
    "impl_every_n": 4,
    "round_count": 7,
    "latest_round": 7,
    "convergence_score": 0.72,
    "created_at": "2026-05-28T10:00:00Z",
    "updated_at": "2026-06-02T14:42:17Z"
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

The response merges the stored config with derived metadata from the meta record (`round_count`, `latest_round`, `convergence_score`, `created_at`, `updated_at`). If the meta record doesn't exist yet (no rounds run), those fields are `0`, `null`, `null`, the config creation time, and the config creation time respectively.

---

#### `DELETE /workflows/:name`

Delete a workflow and all associated rounds, documents, metrics, and stats.

Deletion is best-effort: the config key is deleted first (making the workflow invisible to list/get operations), then round, document, meta, stats, and lock keys are deleted in a loop. If some secondary deletes fail, orphaned keys are harmless — they will never be referenced. The response includes the count of keys successfully deleted.

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "deleted": "fcp-spec",
    "keys_removed": 28
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `PUT /documents/:workflow/:role`

Upload a document to KV. The request body is the raw document content.

The KV key is auto-constructed as `doc::{workflow}::{role}`. The caller does not construct keys manually.

**Request:**

```
PUT /documents/fcp-spec/readme
Content-Type: text/markdown

# Flywheel Connector Protocol

FCP is a security-focused protocol...
```

**Validation:**
- `role` must be alphanumeric, hyphens, or underscores. Max 32 characters.
- Request body must not exceed `MAX_DOCUMENT_BYTES` (default: 1 MB).
- If the body is empty, the request fails with `bad_request`.
- If the body is less than 500 bytes, the upload succeeds but a warning is returned: `"Document is unusually small (N bytes). Verify this is the correct content."` This catches misconfigurations where a stub or placeholder file is uploaded instead of the real document.

**Response:** `200 OK`

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "role": "readme",
    "key": "doc::fcp-spec::readme",
    "bytes": 14523
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `GET /documents/:workflow/:role`

Retrieve a document from KV.

**If the document exists:** `200 OK` with `Content-Type: text/markdown` and the raw document body.

**If the document does not exist:** `404 Not Found` with the standard JSON envelope:

```json
{
  "ok": false,
  "code": "not_found",
  "data": null,
  "warnings": [],
  "hint": "Upload this document with PUT /documents/fcp-spec/readme",
  "error": "Document 'readme' does not exist for workflow 'fcp-spec'",
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `POST /run/:workflow/:round`

Execute a revision round. The Worker:

1. Validates the workflow exists and all referenced documents are in KV
2. **Sequential round enforcement:** If round > 1, verifies that `round::{workflow}::{N-1}` exists with `status: "complete"`. If the previous round is missing or not complete, returns `422 validation_failed` with message "Round N-1 must be completed before running round N." This prevents gaps in the convergence chain. Can be overridden with `"skip_sequence_check": true` in the request body.
3. Checks the *current* round's status:
   - `"complete"`: returns `409 Conflict` (round already finished; cannot overwrite)
   - `"running"` and within lock TTL: returns `409 Conflict` (another run is in progress)
   - `"running"` but past lock TTL: treated as `"stale"`, overwritten by this run
   - `"failed"` or `"stale"`: overwritten by this run (retry is allowed)
   - Does not exist: proceeds normally
4. Determines whether to include implementation (based on `impl_every_n` and round number)
5. Writes a round record with `status: "running"` and acquires the lock
6. Reads all documents from KV
7. Builds the prompt from the template
8. Begins streaming the LLM response back to the client as `text/event-stream`
9. On stream completion: writes the full buffered response to KV as a round record with `status: "complete"`, computes convergence metrics, updates the stats cache

**Request body (optional overrides):**

The request body is optional. If provided, it must be valid JSON with `Content-Type: application/json`. An empty body, missing body, or `{}` are all treated as "use workflow defaults."

```json
{
  "include_impl": true,
  "skip_sequence_check": false,
  "provider": "anthropic",
  "model": "claude-opus-4-6",
  "system_prompt": "You are reviewing a Rust protocol implementation...",
  "provider_params": {
    "thinking": { "type": "enabled", "budget_tokens": 32000 }
  }
}
```

All fields are optional. If omitted, the workflow's defaults apply. Per-run overrides let callers experiment with different providers/models without changing the workflow config.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `include_impl` | bool | auto | Override implementation inclusion. If omitted, determined by `impl_every_n`. |
| `skip_sequence_check` | bool | false | If true, skip the previous-round-exists check. Use for backfilling or non-sequential experimentation. |
| `provider` | string | workflow default | Override the LLM provider for this run. |
| `model` | string | workflow default | Override the model for this run. |
| `system_prompt` | string | workflow default | Override the system prompt for this run. |
| `provider_params` | object | workflow default | Override provider parameters for this run. |

**Response (streaming):**

The response uses `Content-Type: text/event-stream`. Events are forwarded from the LLM with a normalized format:

```
event: token
data: {"text": "The first architectural"}

event: token
data: {"text": " change I recommend"}

event: done
data: {"status": "complete", "round": 5, "words": 2847, "convergence_score": 0.72}
```

The `done` event is sent after the full response is saved to KV and convergence is computed. If the stream fails mid-way, an error event is sent:

```
event: error
data: {"code": "provider_error", "error": "stream disconnected after 4231 bytes"}
```

**Template warnings:** If any document roles in the workflow config are not referenced by the selected template, they are included in the `done` event's data as `"warnings": ["Document role 'implementation' is configured but not referenced by the selected template"]`.

**Non-streaming fallback:** If the client sends `Accept: application/json` (or no `Accept` header), the Worker buffers the entire response and returns it as a single JSON response after the LLM completes. This is simpler but the client must wait for the full duration with no progress indication. To get streaming, the client must explicitly send `Accept: text/event-stream`.

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "status": "complete",
    "content": "# Round 5 Revisions\n\n...",
    "metrics": {
      "words": 2847,
      "lines": 198,
      "characters": 17420,
      "headings": 12
    },
    "convergence": {
      "score": 0.72,
      "output_trend": 0.68,
      "change_velocity": 0.81,
      "similarity_trend": 0.65,
      "estimated_remaining_rounds": 3,
      "recommendation": "continue"
    },
    "usage": {
      "input_tokens": 12450,
      "output_tokens": 3201,
      "reasoning_tokens": 8920
    },
    "provider": "openai",
    "model": "o3",
    "started_at": "2026-06-02T14:30:00Z",
    "completed_at": "2026-06-02T14:42:17Z",
    "duration_seconds": 737
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `GET /rounds/:workflow/:round`

Retrieve a round's output.

**Status mapping:**

| Stored Status | Age vs. Lock TTL | Reported Status |
|---------------|-----------------|-----------------|
| `"running"` | Within TTL | `"running"` |
| `"running"` | Past TTL | `"stale"` |
| `"complete"` | — | `"complete"` |
| `"failed"` | — | `"failed"` |

**If the round is running:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "status": "running",
    "started_at": "2026-06-02T14:30:00Z"
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

**If the round is stale:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "status": "stale",
    "started_at": "2026-06-02T13:00:00Z",
    "stale_reason": "Round has been running for 91 minutes (lock TTL: 60 minutes). The stream likely disconnected. Retry with POST /run/:workflow/:round."
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

**If the round is complete:**

When called with `Accept: text/markdown`, returns the raw markdown body with no envelope.

When called with `Accept: application/json` (or no preference):

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "status": "complete",
    "content": "# Round 5 Revisions\n\n## Architecture Change...",
    "metrics": {
      "words": 2847,
      "lines": 198,
      "characters": 17420,
      "headings": 12
    },
    "convergence": {
      "score": 0.72,
      "output_trend": 0.68,
      "change_velocity": 0.81,
      "similarity_trend": 0.65,
      "estimated_remaining_rounds": 3,
      "recommendation": "continue"
    },
    "usage": {
      "input_tokens": 12450,
      "output_tokens": 3201,
      "reasoning_tokens": 8920
    },
    "provider": "openai",
    "model": "o3",
    "started_at": "2026-06-02T14:30:00Z",
    "completed_at": "2026-06-02T14:42:17Z",
    "duration_seconds": 737
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

**If the round failed:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "status": "failed",
    "error": "provider_error: 429 rate limit exceeded",
    "partial_content": "# Round 5 Revisions\n\nThe first change I recommend is...",
    "partial_bytes": 4231,
    "started_at": "2026-06-02T14:30:00Z",
    "failed_at": "2026-06-02T14:31:02Z"
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `GET /rounds/:workflow`

List all rounds for a workflow.

**Query parameters:**

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `status` | string | all | Filter by status: `running`, `stale`, `complete`, `failed` |
| `limit` | integer | 100 | Max rounds to return (max: 100) |
| `cursor` | string | none | Pagination cursor |

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "rounds": [
      { "round": 1, "status": "complete", "words": 4201, "convergence_score": null, "completed_at": "..." },
      { "round": 2, "status": "complete", "words": 3856, "convergence_score": 0.31, "completed_at": "..." },
      { "round": 3, "status": "complete", "words": 3102, "convergence_score": 0.58, "completed_at": "..." },
      { "round": 4, "status": "running", "words": null, "convergence_score": null, "completed_at": null }
    ],
    "cursor": null
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `GET /stats/:workflow`

Convergence analytics for a workflow. Reads from the pre-computed `stats::{workflow}` cache in KV — does not recompute from round records.

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "total_rounds": 7,
    "convergence": {
      "score": 0.72,
      "output_trend": 0.68,
      "change_velocity": 0.81,
      "similarity_trend": 0.65,
      "estimated_remaining_rounds": 3,
      "recommendation": "continue"
    },
    "rounds": [
      { "round": 1, "words": 4201, "delta_words": null, "similarity": null, "score": null },
      { "round": 2, "words": 3856, "delta_words": 1847, "similarity": 0.42, "score": 0.31 },
      { "round": 3, "words": 3102, "delta_words": 1203, "similarity": 0.61, "score": 0.58 },
      { "round": 4, "words": 2987, "delta_words": 802, "similarity": 0.71, "score": 0.64 },
      { "round": 5, "words": 2847, "delta_words": 541, "similarity": 0.78, "score": 0.69 },
      { "round": 6, "words": 2790, "delta_words": 387, "similarity": 0.83, "score": 0.71 },
      { "round": 7, "words": 2756, "delta_words": 298, "similarity": 0.87, "score": 0.72 }
    ]
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

#### `POST /stats/:workflow/rebuild`

Rebuild the stats cache from scratch by reading all completed round records for the workflow. This is the equivalent of APR's `apr backfill` command.

**When to use:**
- After manually inserting or modifying round records in KV
- If the stats cache becomes corrupted or out of sync
- After importing rounds from another system

The Worker:
1. Lists all `round::{workflow}::*` keys from KV
2. Reads each completed round record
3. Recomputes all metrics, deltas, similarities, and convergence scores from scratch
4. Writes the rebuilt stats cache to `stats::{workflow}`
5. Updates the meta record

**Request body:** None required.

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "rounds_processed": 7,
    "convergence_score": 0.72
  },
  "warnings": [],
  "hint": null,
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

If the workflow has no completed rounds, `rounds_processed` is 0, `convergence_score` is `null`, and the stats cache is written as empty.

---

#### `POST /integrate/:workflow/:round`

Generate an integration prompt suitable for pasting into a coding agent (e.g., Claude Code). This takes the round output and wraps it in a hardcoded preamble with instructions for applying the revisions.

The integration template is hardcoded in v0.1:

```
The following are revision suggestions from round {N} of iterative specification
review for the "{workflow}" specification. Apply each suggested change to the
specification document, preserving the existing structure where possible. For each
change, explain what you modified and why.

---

{round_content}
```

Customizable integration templates are deferred to v0.2.

**Important:** The integration prompt contains only the round's revision suggestions wrapped in a preamble. It does **not** include the current source documents (README, spec, implementation). The caller is responsible for providing the current spec and README to the coding agent alongside this prompt. This keeps the endpoint simple and avoids duplicating document retrieval logic — the caller already has the documents or can fetch them via `GET /documents/:workflow/:role`.

**Response:**

```json
{
  "ok": true,
  "code": "ok",
  "data": {
    "workflow": "fcp-spec",
    "round": 5,
    "prompt": "The following are revision suggestions from round 5 of iterative specification review for the \"fcp-spec\" specification. Apply each suggested change to the specification document, preserving the existing structure where possible. For each change, explain what you modified and why.\n\n---\n\n# Round 5 Revisions\n\n..."
  },
  "warnings": [],
  "hint": "Provide the current spec and README alongside this prompt to give the coding agent full context.",
  "error": null,
  "meta": { "version": "0.1.0", "ts": "..." }
}
```

---

## 6. Storage Design (KV Schema)

All state lives in a single KV namespace (`APRP`). Keys use `::` as a delimiter.

### 6.1 Key Patterns

| Pattern | Value Type | Description |
|---------|-----------|-------------|
| `config::{workflow}` | JSON | Workflow configuration |
| `doc::{workflow}::{role}` | text | Source document (readme, spec, impl) |
| `round::{workflow}::{N}` | JSON | Round result (status, content, metrics, convergence, usage). `N` is an unpadded integer (e.g., `round::fcp-spec::5`, not `round::fcp-spec::005`). Listing and sorting by round number is done in application code, not by KV key ordering. |
| `meta::{workflow}` | JSON | Workflow-level metadata (round count, latest convergence) |
| `stats::{workflow}` | JSON | Pre-computed convergence analytics (per-round metrics array, latest score) |
| `lock::{workflow}` | JSON | Run lock (prevents concurrent runs on same workflow) |

### 6.2 Lock Record

```json
{
  "round": 5,
  "started_at": "2026-06-02T14:30:00Z",
  "expires_at": "2026-06-02T15:30:00Z"
}
```

Locks use KV's built-in `expirationTtl` so they auto-delete after the TTL (default: 3600 seconds). Before starting a run, the Worker reads the lock key. If it exists (and KV hasn't expired it yet), the request gets `409 Conflict`.

**Known limitation:** KV does not provide compare-and-swap. Two exactly simultaneous `POST /run` requests could both read "no lock" and both start a run. This race is acceptable for v0.1's single-tenant use case. The window is milliseconds, and the consequence (two LLM calls for the same round, last writer wins on the KV save) is wasteful but not data-corrupting.

### 6.3 Round Record

```json
{
  "workflow": "fcp-spec",
  "round": 5,
  "status": "complete",
  "content": "# Round 5 Revisions\n\n...",
  "partial_content": null,
  "metrics": {
    "words": 2847,
    "lines": 198,
    "characters": 17420,
    "headings": 12
  },
  "convergence": {
    "score": 0.72,
    "output_trend": 0.68,
    "change_velocity": 0.81,
    "similarity_trend": 0.65,
    "estimated_remaining_rounds": 3,
    "recommendation": "continue"
  },
  "usage": {
    "input_tokens": 12450,
    "output_tokens": 3201,
    "reasoning_tokens": 8920
  },
  "provider": "openai",
  "model": "o3",
  "include_impl": false,
  "started_at": "2026-06-02T14:30:00Z",
  "completed_at": "2026-06-02T14:42:17Z",
  "duration_seconds": 737
}
```

For failed rounds, `content` is null and `partial_content` contains whatever was received before the failure. For complete rounds, `partial_content` is null.

### 6.4 Stats Record

Pre-computed convergence data, updated after each completed round. This is what `GET /stats` reads — it never recomputes from individual round records.

```json
{
  "workflow": "fcp-spec",
  "total_rounds": 7,
  "latest_score": 0.72,
  "latest_word_set": ["abstract", "api", "architecture", "..."],
  "rounds": [
    { "round": 1, "words": 4201, "delta_words": null, "similarity": null, "score": null },
    { "round": 2, "words": 3856, "delta_words": 1847, "similarity": 0.42, "score": 0.31 },
    { "round": 3, "words": 3102, "delta_words": 1203, "similarity": 0.61, "score": 0.58 }
  ],
  "updated_at": "2026-06-02T14:42:17Z"
}
```

The `latest_word_set` is the deduplicated, lowercased, punctuation-stripped word set from the most recent completed round. It is stored so that the next round can compute Jaccard similarity without re-reading the previous round's full content from KV. Only the latest round's word set is stored — not the full history. A typical word set of 2,000 unique words serializes to ~20 KB, well within KV limits.

### 6.5 Meta Record

```json
{
  "workflow": "fcp-spec",
  "round_count": 7,
  "latest_round": 7,
  "latest_convergence": 0.72,
  "created_at": "2026-05-28T10:00:00Z",
  "updated_at": "2026-06-02T14:42:17Z"
}
```

### 6.6 Size Budget

| Item | Typical Size | KV Limit |
|------|-------------|----------|
| Workflow config | 2-5 KB | 25 MB |
| Source document | 30-80 KB | 25 MB |
| Round output | 10-50 KB | 25 MB |
| Round record (with content) | 15-60 KB | 25 MB |
| Stats record (typical) | 1-5 KB | 25 MB |
| Stats record (large word set, 8K+ unique words) | up to 100 KB | 25 MB |
| Meta record | < 1 KB | 25 MB |

All items are well within KV's 25 MB value limit. A workflow with 20 rounds would use ~20 keys for rounds, 3-4 for documents, 1 for config, 1 for meta, 1 for stats, 1 for lock — approximately 27 keys total.

---

## 7. LLM Integration

### 7.1 Provider Adapter Trait

```rust
trait LlmProvider {
    async fn stream_completion(
        &self,
        system_prompt: Option<&str>,
        user_prompt: &str,
        params: &ProviderParams,
    ) -> Result<impl Stream<Item = Result<StreamChunk, ProviderError>>, ProviderError>;
}

enum StreamChunk {
    Text(String),
    Usage(UsageStats),
    Done,
}

struct UsageStats {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}
```

Two implementations: `OpenAiProvider` and `AnthropicProvider`. The workflow config's `provider` field selects which adapter to use. Per-run overrides can switch providers for a single round.

### 7.2 OpenAI Integration

**Endpoint:** `POST https://api.openai.com/v1/chat/completions`

**Auth:** `Authorization: Bearer <OPENAI_API_KEY>` (Worker secret)

**Request shape:**

```json
{
  "model": "o3",
  "stream": true,
  "stream_options": { "include_usage": true },
  "messages": [
    { "role": "system", "content": "<system_prompt if configured>" },
    { "role": "user", "content": "<bundled prompt>" }
  ],
  "reasoning_effort": "high",
  "max_completion_tokens": 32000
}
```

If no `system_prompt` is configured, the `system` message is omitted entirely.

The `stream_options.include_usage` field causes OpenAI to include token usage in the final SSE event. This is captured in the round record's `usage` field.

**Stream format:** Server-Sent Events (SSE). Each event contains a JSON payload:

- **Content chunks:** `choices[0].delta.content` — the next piece of text. Append to buffer.
- **Usage (final event):** `usage.prompt_tokens`, `usage.completion_tokens`, `usage.completion_tokens_details.reasoning_tokens` — captured in the round record.
- **`[DONE]`:** Signals stream completion.

**Extended reasoning models** (o3, o4-mini, etc.) use the `reasoning_effort` parameter rather than `temperature`. The `provider_params` object in the workflow config passes these through directly.

### 7.3 Anthropic Integration

**Endpoint:** `POST https://api.anthropic.com/v1/messages`

**Auth:** `x-api-key: <ANTHROPIC_API_KEY>` (Worker secret)

**Required headers:**
- `anthropic-version: 2023-06-01`
- `content-type: application/json`

**Request shape:**

```json
{
  "model": "claude-opus-4-6",
  "max_tokens": 32000,
  "stream": true,
  "system": "<system_prompt if configured>",
  "thinking": {
    "type": "enabled",
    "budget_tokens": 32000
  },
  "messages": [
    { "role": "user", "content": "<bundled prompt>" }
  ]
}
```

If no `system_prompt` is configured, the `system` field is omitted entirely.

**Stream format:** SSE with multiple event types. The Worker must handle:

| Event Type | Action |
|-----------|--------|
| `message_start` | Extract `message.id`. Ignore otherwise. |
| `content_block_start` | Check `content_block.type`. If `"text"`, subsequent deltas are appended. If `"thinking"`, subsequent deltas are discarded. |
| `content_block_delta` | If current block is `"text"`: extract `delta.text` and append to buffer. If current block is `"thinking"`: discard. |
| `content_block_stop` | Reset current block type. |
| `message_delta` | Extract `usage.output_tokens` for the round record. |
| `message_stop` | Stream is complete. |
| `error` | Set round to `"failed"` with the error message. |
| Any other type | Ignore silently. |

**Extended thinking** content blocks are discarded — only the final `text` blocks are captured as the round output. The `usage` from `message_start` (input tokens) and `message_delta` (output tokens) are combined in the round record.

### 7.4 Stream Consumption

The Worker streams the LLM response through to the client:

1. Open the LLM connection and begin receiving SSE events
2. For each text chunk: append to an in-memory buffer AND forward to the client as `event: token`
3. Track usage stats as they arrive
4. On stream completion (`[DONE]` for OpenAI, `message_stop` for Anthropic):
   a. Compute document metrics on the buffered content
   b. Read the previous round's word set from the stats cache
   c. Compute convergence (see section 9)
   d. Write the round record to KV with `status: "complete"`
   e. Update the stats cache in KV
   f. Update the meta record in KV
   g. Release the lock (delete the lock key)
   h. Send `event: done` to the client with summary data

Steps (a)-(h) are synchronous after the stream ends. They involve only KV writes and in-memory computation — total CPU time is well under 1 second.

### 7.5 Error Handling

| Scenario | Behavior |
|----------|----------|
| LLM returns 429 (rate limit) | Write round record with `status: "failed"`, release lock, send `event: error` to client. |
| LLM returns 500/503 | Write round record with `status: "failed"`, release lock, send `event: error`. |
| LLM returns 400 (bad request) | Write round record with `status: "failed"`. Include the provider's error message. |
| Stream disconnects mid-response | Write round record with `status: "failed"` and `partial_content` (whatever was buffered). Release lock. |
| LLM returns empty response | Write round record with `status: "failed"`. |
| No data received for 5 minutes | Abort stream, write `status: "failed"`, release lock. |
| Client disconnects mid-stream | Worker cannot detect this reliably. The LLM stream continues. If the LLM completes, the round is saved to KV as `"complete"` (the Worker is still alive because the LLM fetch is still in progress). If the LLM stream also fails, the round is saved as `"failed"`. |

Automatic retry is intentionally omitted. LLM calls are expensive (time and money). The caller should decide whether to retry by checking the round status and resubmitting `POST /run`.

### 7.6 API Key Management

All secrets are stored as Cloudflare Worker secrets:

```
CSVKEY=<shared-authentication-key>
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
```

Set via `wrangler secret put <NAME>`. Never stored in code, config files, or KV.

---

## 8. Prompt Building

### 8.1 Template Syntax

Templates use `{{placeholder}}` syntax. Placeholders map to document role names defined in the workflow config's `documents` map.

```
First, read this README:

\`\`\`
{{readme}}
\`\`\`

---

NOW: Carefully review this entire plan for me and come up with your best
revisions in terms of better architecture, new features, changed features,
etc. to make it better, more robust/reliable, more performant, more
compelling/useful, etc.

For each proposed change, give me your detailed analysis and
rationale/justification for why it would make the project better along
with the git-diff style change versus the original plan shown below:

\`\`\`
{{spec}}
\`\`\`
```

### 8.2 Template Selection

The Worker selects the template based on whether implementation is included:

1. If `include_impl` is true (either explicitly or via `impl_every_n`): use `template_with_impl`
2. Otherwise: use `template`
3. If `template_with_impl` is not defined but impl is requested: use `template` (impl document is still available as `{{implementation}}` if referenced in the base template)

### 8.3 Implementation Auto-Inclusion

When `impl_every_n` is set (e.g., `4`), the Worker automatically includes the implementation document on rounds 4, 8, 12, etc. The check:

```
include_impl = (impl_every_n > 0) && (round % impl_every_n == 0)
```

This can be overridden per-run via the `include_impl` field in the `POST /run` request body.

### 8.4 Document Bundling

1. Parse the selected template for `{{placeholder}}` tokens
2. For each placeholder, look up the role in the workflow's `documents` map
3. Read the corresponding document from KV (`doc::{workflow}::{role}`)
4. Replace the placeholder with the document content
5. If a placeholder references a role not in the `documents` map: fail with `validation_failed` and name the missing role
6. If a placeholder references a role whose document doesn't exist in KV: fail with `validation_failed` and name the missing document

**Warning detection:** After template rendering, check if any roles in the `documents` map were NOT referenced by placeholders in the selected template. For each unreferenced role, emit a warning: `"Document role '{role}' is configured but not referenced by the selected template"`. Warnings are non-fatal — the run proceeds.

The assembled prompt is sent as the user message content. If `system_prompt` is configured (on the workflow or as a per-run override), it is sent as a separate system message.

### 8.5 Default Templates

When a workflow config omits `template`, the Worker uses this built-in default:

```
First, read this README:

\`\`\`
{{readme}}
\`\`\`

---

NOW: Carefully review this entire plan for me and come up with your best
revisions in terms of better architecture, new features, changed features,
etc. to make it better, more robust/reliable, more performant, more
compelling/useful, etc.

For each proposed change, give me your detailed analysis and
rationale/justification for why it would make the project better along
with the git-diff style change versus the original plan shown below:

\`\`\`
{{spec}}
\`\`\`
```

When `template_with_impl` is also omitted, the default impl template is:

```
First, read this README:

\`\`\`
{{readme}}
\`\`\`

---

And here is a document detailing the implementation that follows the
specification document given below; you should also keep the implementation
in mind as you think about the specification, since ultimately the
specification needs to be translated into code:

\`\`\`
{{implementation}}
\`\`\`

---

NOW: Carefully review this entire plan for me and come up with your best
revisions in terms of better architecture, new features, changed features,
etc. to make it better, more robust/reliable, more performant, more
compelling/useful, etc.

For each proposed change, give me your detailed analysis and
rationale/justification for why it would make the project better along
with the git-diff style change versus the original plan shown below:

\`\`\`
{{spec}}
\`\`\`
```

When using default templates, the `documents` map defaults to `{"readme": "readme", "spec": "spec"}` (plus `"implementation": "impl"` if `impl_every_n > 0` or `template_with_impl` is provided). The caller must still upload these documents via `PUT /documents/:workflow/:role` before running.

---

## 9. Convergence Analytics

### 9.1 Algorithm

Convergence is computed incrementally after each completed round. The Worker reads the existing stats cache from KV, appends the new round's data, and recomputes the score.

```
score = (0.35 * output_trend) + (0.35 * change_velocity) + (0.30 * similarity_trend)
```

Each signal is normalized to a 0.0–1.0 range where 1.0 = fully converged.

### 9.2 Output Size Trend (weight: 0.35)

Measures whether LLM responses are getting shorter (fewer revisions needed = convergence).

**Algorithm:**

```
ratio = 1.0 - (latest_words / max_words_across_all_rounds)
output_trend = clamp(ratio, 0.0, 1.0)
```

Where `latest_words` is the word count of the most recent round and `max_words_across_all_rounds` is the highest word count seen in any completed round.

**Example:** If the max was 4201 (round 1) and the latest is 2756 (round 7): `1.0 - (2756 / 4201) = 0.344`. This signals moderate convergence — the response is ~34% shorter than the peak.

### 9.3 Change Velocity (weight: 0.35)

Measures whether the magnitude of changes between consecutive rounds is decreasing.

**Algorithm:**

```
1. For each consecutive pair of completed rounds (N, N+1):
   a. delta = abs(words_N - words_N+1)
2. Collect deltas into an ordered list
3. velocity = 1.0 - (latest_delta / max_delta_across_all_pairs)
4. Clamp to [0.0, 1.0]
```

This uses the simple absolute difference in word counts — not set operations. It's cheap to compute and correlates well with the intuition that "smaller changes = convergence."

**Example:** Deltas are [345, 754, 115, 140, 57, 34]. Max delta is 754. Latest delta is 34. `1.0 - (34 / 754) = 0.955`. High convergence signal — the latest change is tiny compared to the peak.

### 9.4 Content Similarity (weight: 0.30)

Measures whether consecutive rounds are producing increasingly similar output.

**Algorithm (Jaccard similarity on word sets):**

```
1. Tokenize: lowercase the content, split on whitespace, strip leading/trailing punctuation from each token
2. Build a word SET (deduplicated) for each round
3. similarity = |intersection(set_N, set_N+1)| / |union(set_N, set_N+1)|
4. The signal value is the similarity between the two most recent rounds
5. Already in [0.0, 1.0] — no normalization needed
```

**Incremental computation:** To avoid re-reading the full content of previous rounds, the stats cache stores a serialized word set for the most recent round. When a new round completes, the Worker:
1. Tokenizes the new round's content into a word set
2. Reads the previous round's word set from the stats cache
3. Computes Jaccard similarity
4. Replaces the stored word set with the new one

This means only the latest round's content is in memory — not the entire history.

**Example:** Round 6 has 1,847 unique words. Round 7 has 1,812 unique words. They share 1,623 words. Union is 2,036. `1623 / 2036 = 0.797`. Good similarity — ~80% word overlap.

### 9.5 Estimated Remaining Rounds

A rough extrapolation based on convergence score:

| Score Range | Estimated Remaining | Recommendation |
|-------------|-------------------|----------------|
| >= 0.90 | 0 | `"stop"` — Spec has converged. Further rounds will produce diminishing returns. |
| >= 0.75 | 1-2 | `"almost"` — Approaching convergence. 1-2 more rounds recommended. |
| >= 0.50 | 3-5 | `"continue"` — Making progress but significant revisions still occurring. |
| < 0.50 | 5+ | `"early"` — Still in early iterations. Major changes likely. |

This is intentionally imprecise — it's a signal, not a prediction.

### 9.6 Edge Cases

- **Round 1:** No previous round to compare. All three signals are `null`. Convergence score is `null`. Recommendation is `null`.
- **Round 2:** All three signals are computable (one pair of rounds, two data points for output trend). Score is computed normally.
- **Non-sequential rounds:** If rounds 1, 2, 5 exist (3 and 4 missing), compute over available consecutive pairs (1→2, 2→5). The gap in round numbers doesn't invalidate the signals — the math operates on the ordered sequence of completed rounds, not round numbers.
- **All rounds have same word count:** Output trend = 0.0, change velocity = 1.0 (delta is 0, which is 1.0 - 0/max but max is also 0 — handle division by zero: if max_delta is 0, velocity = 1.0). Similarity will be high. This is correctly interpreted as convergence.
- **Word count increases:** Output trend goes negative, clamped to 0.0. This correctly signals non-convergence — the spec is getting larger, not smaller.

---

## 10. Document Metrics

Collected for every completed round and stored in the round record.

| Metric | Description | Algorithm |
|--------|-------------|-----------|
| `words` | Word count | Split content on whitespace, count non-empty tokens |
| `lines` | Line count | Count newline characters + 1 (empty content = 0) |
| `characters` | Character count | Length of the content string in UTF-8 bytes |
| `headings` | Markdown heading count | Count lines matching regex `^#{1,6}\s` |

These are cheap to compute and provide the raw data for convergence analytics.

**Note:** The original APR also tracks a `sections` count. This is omitted because it is effectively redundant with `headings` — markdown sections are delimited by headings.

---

## 11. Security

### 11.1 Authentication

Single shared key (`CSVKEY`) passed as a query parameter and compared using constant-time equality. Checked on every request before any processing, except `GET /health`. See section 5.1 for the full validation flow.

### 11.2 API Key Isolation

LLM API keys are Worker secrets — never logged, never returned in API responses, never stored in KV.

### 11.3 Input Validation

- Workflow names: regex `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$`. Must start with alphanumeric.
- Document roles: regex `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,31}$`.
- Round numbers: positive integers, range 1–999.
- Document content: max 1 MB per document (enforced at upload time via `MAX_DOCUMENT_BYTES`).
- Template content: max 100 KB.
- System prompt: max 10 KB.
- Request body (JSON payloads): max 2 MB total.
- Provider: must be exactly `"openai"` or `"anthropic"`.

### 11.4 No Credential Storage

The Worker never stores or forwards user credentials for LLM services beyond the API keys. There are no user accounts, sessions, or cookies.

### 11.5 No CORS Headers

No CORS headers are sent. The Worker is designed for server-to-server and CLI callers, not browsers. Browser-based callers will encounter CORS errors. CORS support is deferred to a future version if browser-based tooling is needed.

---

## 12. Configuration (wrangler.toml)

```toml
name = "aprp"
main = "build/worker/shim.mjs"
compatibility_date = "2026-06-01"

[build]
command = "curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable && . \"$HOME/.cargo/env\" && rustup target add wasm32-unknown-unknown && cargo install -q worker-build && worker-build --release"

[vars]
ENVIRONMENT = "production"
DEFAULT_LOCK_TTL_SECONDS = "3600"
MAX_DOCUMENT_BYTES = "1048576"

[[kv_namespaces]]
binding = "APRP"
id = "<namespace-id>"

# Secrets (set via `wrangler secret put`):
# CSVKEY
# OPENAI_API_KEY
# ANTHROPIC_API_KEY
```

### 12.1 Environment Variables

| Variable | Source | Description | Default |
|----------|--------|-------------|---------|
| `CSVKEY` | Secret | Shared authentication key — must match the `csvkey` query param on every request | (required) |
| `OPENAI_API_KEY` | Secret | OpenAI API key | (required if using OpenAI) |
| `ANTHROPIC_API_KEY` | Secret | Anthropic API key | (required if using Anthropic) |
| `ENVIRONMENT` | Var | `"production"` or `"development"` | `"production"` |
| `DEFAULT_LOCK_TTL_SECONDS` | Var | Lock expiry duration in seconds | `"3600"` |
| `MAX_DOCUMENT_BYTES` | Var | Max document upload size in bytes | `"1048576"` |

---

## 13. Deployment

### 13.1 Prerequisites

- Rust toolchain (stable)
- `wrangler` CLI v3+
- Cloudflare account with Workers paid plan
- KV namespace created

### 13.2 Setup

```bash
# Create KV namespace
wrangler kv namespace create APRP

# Update wrangler.toml with the namespace ID from the output above

# Set secrets
wrangler secret put CSVKEY
wrangler secret put OPENAI_API_KEY
wrangler secret put ANTHROPIC_API_KEY

# Deploy
wrangler deploy

# Verify (health check — no csvkey required)
curl https://aprp.<your-subdomain>.workers.dev/health

# Verify auth works
curl "https://aprp.<your-subdomain>.workers.dev/workflows?csvkey=YOUR_KEY"
```

### 13.3 Local Development

```bash
wrangler dev
```

Uses local KV emulation. LLM API calls hit real endpoints. Set secrets locally via a `.dev.vars` file:

```
CSVKEY=test-key-for-dev
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
```

This file must be in `.gitignore`.

---

## 14. Testing Strategy

### 14.1 Unit Tests

| Area | What to test |
|------|-------------|
| Template parsing | Valid placeholders extracted; escaped braces `\{\{` ignored; nested braces rejected; empty template returns empty list |
| Placeholder replacement | All placeholders replaced; missing role fails; unreferenced roles produce warnings |
| Convergence algorithm | All three signals with known inputs and expected outputs; division by zero (all same word count); single round (null); two rounds; 20 rounds |
| Document metrics | Word count on empty string (0), single word (1), multiple whitespace, Unicode; line count with trailing newline; heading regex edge cases (`#no-space`, `##`, `# valid`) |
| KV key generation | Correct format for all key types; round numbers zero-padded or not (decide and test) |
| csvkey auth | Missing `csvkey` → 401 `missing_csvkey`; wrong value → 401 `unauthorized`; correct value → proceeds; constant-time comparison (verify both branches take similar time); missing `CSVKEY` secret → 500 `missing_config` |
| Input validation | Workflow name regex accepts/rejects; round number boundaries (0 rejected, 1 accepted, 999 accepted, 1000 rejected); document size at limit |
| HTTP method validation | Wrong method for each route → 405 `method_not_allowed`; unmatched path → 404 `not_found` |
| Lock logic | Fresh lock; expired lock (stale); active lock (conflict); lock TTL parsing |
| Sequential round check | Round 1 skips check; round 2 requires round 1 complete; missing previous → error; `skip_sequence_check` bypasses |
| Default template | No template → built-in used; no `documents` → defaults to readme+spec; with `impl_every_n` → default includes implementation role |
| Error code mapping | Every `ProviderError` variant maps to the correct HTTP status and error code |

### 14.2 Integration Tests

| Scenario | What to verify |
|----------|---------------|
| Full round lifecycle | Create workflow → upload docs → run round → poll round → verify content, metrics, convergence |
| OpenAI SSE parsing | Mock SSE stream with `choices[0].delta.content`, `usage`, and `[DONE]`. Verify buffer and usage extraction. |
| Anthropic SSE parsing | Mock SSE stream with `content_block_start(thinking)`, `content_block_delta(thinking)`, `content_block_stop`, `content_block_start(text)`, `content_block_delta(text)`, `message_delta(usage)`, `message_stop`. Verify thinking blocks are discarded and text blocks are captured. |
| Error handling | Simulated 429, 500, stream disconnect, empty response. Verify round status is `"failed"` with correct error message and partial content. |
| Lock contention | Two concurrent `POST /run` requests. Verify at most one succeeds or both complete without data corruption. |
| Stale round recovery | Create a round with `"running"` status and `started_at` older than lock TTL. Verify `GET /rounds` reports `"stale"`. Verify `POST /run` overwrites it. |
| Failed round retry | Create a round with `"failed"` status. Verify `POST /run` overwrites it. Verify `"complete"` rounds cannot be overwritten (409). |
| Convergence accuracy | Run 5 rounds with known word counts and content. Verify convergence score matches hand-calculated expected value. |
| Stats cache | After each round, verify `GET /stats` returns data consistent with the round records without re-reading them. |
| Workflow deletion | Create workflow with 10 rounds. Delete workflow. Verify GET returns 404. Verify orphaned keys don't appear in lists. |
| Document validation | Create workflow referencing doc role "readme". Don't upload the doc. Verify `POST /run` returns `validation_failed`. Upload the doc. Verify `POST /run` succeeds. |
| csvkey auth flow | Request without `csvkey` → 401. Request with wrong `csvkey` → 401. Request with correct `csvkey` → proceeds to route handler. `GET /health` without `csvkey` → 200. |
| Sequential round enforcement | `POST /run` for round 3 when round 2 doesn't exist → `validation_failed`. With `skip_sequence_check: true` → proceeds. Round 1 never checks previous. |
| Stats rebuild | Create 5 rounds manually in KV. `POST /stats/:workflow/rebuild` produces a stats cache identical to what incremental computation would produce. |
| Default template | Create workflow with no `template` field. `POST /run` uses the built-in default. Verify the prompt contains the README and spec content. |
| Small document warning | `PUT /documents` with 50-byte body → 200 with warning. With 1000-byte body → 200 with no warning. With empty body → 400. |

### 14.3 Manual/E2E Tests

- Real LLM API call (OpenAI o3) with a small document (~1 KB spec)
- Real LLM API call (Anthropic claude-opus-4-6) with a small document
- Full 3-round workflow with convergence tracking — verify score increases
- SSE stream viewed in browser/curl (`curl -N`) to verify real-time token delivery
- `wrangler dev` local testing before deploy
- Health check returns 200 from deployed Worker

---

## 15. Crate Dependencies

| Crate | Purpose |
|-------|---------|
| `worker` | Cloudflare Workers Rust bindings (routing, KV, Fetch, Response) |
| `serde` / `serde_json` | JSON serialization/deserialization |
| `chrono` | Timestamp formatting (ISO 8601) |
| `console_error_panic_hook` | Forwards Rust panics to `console.error` for debugging in Workers |

Minimal dependency tree. No async runtime needed (Workers provide their own). No HTTP client crate needed (`worker::Fetch` handles outbound requests).

---

## 16. Module Structure

```
src/
├── lib.rs              # Worker entry point, router (method check, route dispatch)
├── validation.rs       # Query param parsing, csvkey auth (constant-time eq), input validation
├── routes/
│   ├── mod.rs
│   ├── health.rs       # GET /health
│   ├── workflows.rs    # CRUD for workflow configs
│   ├── documents.rs    # Upload/retrieve documents
│   ├── run.rs          # Execute rounds (POST /run) — streaming + non-streaming
│   ├── rounds.rs       # Retrieve round results, stale detection
│   ├── stats.rs        # Convergence analytics (reads cache)
│   └── integrate.rs    # Integration prompt generation
├── providers/
│   ├── mod.rs          # Provider trait, StreamChunk enum
│   ├── openai.rs       # OpenAI adapter (SSE parsing, usage extraction)
│   └── anthropic.rs    # Anthropic adapter (SSE parsing, thinking block filtering)
├── convergence.rs      # Three-signal convergence algorithm
├── metrics.rs          # Document metrics (words, lines, characters, headings)
├── prompt.rs           # Template parsing, placeholder replacement, warning detection
├── storage.rs          # KV read/write helpers, key patterns, lock logic
├── error.rs            # Error types, json_error() builder, error code → HTTP status mapping
└── types.rs            # Shared types (Workflow, Round, Meta, Stats, Usage, etc.)
```

14 files. Each under 300 lines. No abstraction layers beyond the provider trait.

---

## 17. Future Considerations (Out of Scope for v0.1)

These are explicitly deferred. They should not influence v0.1 design decisions.

- **Fire-and-forget runs via Durable Objects** — `POST /run` returns `202 Accepted` immediately, a Durable Object manages the stream and writes to KV on completion. Removes the requirement for the client to hold the connection open.
- **Multi-tenancy** — multiple users with separate workflows and API keys
- **Webhook notifications** — POST to a URL when a round completes
- **R2 storage** — for round outputs exceeding KV's 25 MB limit (unlikely but possible)
- **Diff computation** — server-side diff between rounds
- **Prompt library** — shared templates for common specification patterns
- **Cost tracking** — estimate and record LLM API costs per round (usage data is already captured in v0.1)
- **Batch runs** — execute rounds 5 through 10 in sequence with a single request
- **Dashboard UI** — web-based round browser (separate project, consumes this API)
- **Rate limiting** — per-client rate limits beyond the shared key
- **Audit log** — record all API calls for debugging
- **Customizable integration templates** — caller-defined templates for `POST /integrate`
- **Rich integration prompts** — include source documents (README, spec) in the integration prompt, not just the round output
- **Stats export formats** — CSV and Markdown export from `GET /stats`
- **CORS headers** — for browser-based callers and dashboard UIs
- **URL-based API versioning** — `/v1/` prefix when the API stabilizes

---

## 18. Success Criteria

v0.1 is complete when all of the following are demonstrated:

### 18.1 Functional (must pass in integration tests)

1. **Workflow CRUD:** A caller can create, read, update, list, and delete workflows via the API. Workflow creation validates that all referenced document roles exist in KV.
2. **Document upload:** A caller can upload and retrieve documents via `PUT/GET /documents/:workflow/:role`.
3. **Round execution (OpenAI):** `POST /run` with `provider: "openai"` streams tokens to the client and saves the complete response to KV with correct metrics and convergence data.
4. **Round execution (Anthropic):** `POST /run` with `provider: "anthropic"` streams tokens to the client, discards thinking blocks, and saves only text output to KV.
5. **Round retrieval:** `GET /rounds/:workflow/:round` returns the round with correct status, content, metrics, convergence, and usage data.
6. **Failed round retry:** A round with status `"failed"` or `"stale"` can be retried via `POST /run`. A round with status `"complete"` returns `409 Conflict`.
7. **Convergence accuracy:** Given 5 rounds with known content, the convergence score computed by the Worker matches the hand-calculated expected value (within 0.01 tolerance).
8. **Stats cache:** `GET /stats` returns data consistent with completed rounds and does not trigger KV reads of individual round records.
9. **Stats rebuild:** `POST /stats/:workflow/rebuild` recomputes the stats cache from all completed round records and produces results identical to the incrementally-computed cache.
10. **Sequential round enforcement:** `POST /run/fcp-spec/5` fails with `validation_failed` if round 4 is not complete. Succeeds when `skip_sequence_check: true` is set.
11. **Health check:** `GET /health` returns `200 OK` with version and KV accessibility status, without requiring `csvkey`.
12. **Auth enforcement:** Requests without `csvkey` return `401 missing_csvkey`. Requests with wrong `csvkey` return `401 unauthorized`. `GET /health` works without `csvkey`.

### 18.2 Error handling (must pass in integration tests)

13. **All error codes exercised:** Each of the 12 error codes in section 5.3 is returned under its documented condition in at least one test.
14. **Provider errors captured:** A simulated LLM 429/500 response results in a round with `status: "failed"` and a descriptive error message.
15. **Partial content preserved:** A simulated mid-stream disconnect saves `partial_content` in the failed round record.
16. **Lock enforcement:** A second `POST /run` on the same workflow while a run is in progress returns `409 Conflict`.
17. **Stale detection:** A round that has been `"running"` longer than the lock TTL is reported as `"stale"` by `GET /rounds`.
18. **Missing secret handling:** If `CSVKEY` is not configured as a Worker secret, all authenticated routes return `500 missing_config`.

### 18.3 Operational (must pass manually)

19. **Single-command deploy:** `wrangler deploy` produces a working Worker with no additional manual steps beyond initial secret setup.
20. **Real OpenAI round:** A round against OpenAI's o3 model with a ~1 KB document completes successfully and the SSE stream is viewable in real-time via `curl -N "https://aprp.example.dev/run/test/1?csvkey=KEY" -d '{}'`.
21. **Real Anthropic round:** A round against Anthropic's claude-opus-4-6 with extended thinking completes successfully, thinking output is not present in the saved round content.
22. **60-minute stream tolerance:** A round with a large document that triggers 30+ minutes of extended reasoning completes without timeout or data loss. (Tested manually with a real LLM call.)
