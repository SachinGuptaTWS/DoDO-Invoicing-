# Invoice & Payment Service

A small invoicing backend: businesses create customers and invoices, customers pay through a PSP, and businesses get signed webhooks when invoices change state. Written in Rust (Axum, sqlx, PostgreSQL).

- [DESIGN.md](DESIGN.md): data model, state machine, payment failure modes, webhooks, API keys, what was cut
- [openapi.yaml](openapi.yaml): API reference (OpenAPI 3.1)
- [AI_USAGE.md](AI_USAGE.md): how AI tools were used

## Demo Video

https://drive.google.com/file/d/1g9pG71lfdFJV0Myp8Pim7BdB0UXxf9Ut/view?usp=sharing

## Time spent

Roughly 6 to 10 hours in total, which is over the 4 to 6 hour budget. Most of the time past the budget went on reviewing edge cases, the Docker setup, the docs, and testing the full flow by hand. How I used AI is in [AI_USAGE.md](AI_USAGE.md).

What I would build next, and what is missing before production, is in DESIGN.md §6 and §7.

## Running it

```bash
docker compose up --build
```

No other setup is needed. Migrations run when the service starts. The first build compiles the Rust workspace and takes a few minutes; later builds are cached.

| Service | Address | What it is |
|---|---|---|
| `invoicing` | http://localhost:8080 | The API |
| `webhook-sink` | http://localhost:9191/received | A webhook receiver for the demo. `GET /received` lists what it got. |
| `mock-psp` | `http://mock-psp:9090` (internal only) | The mock payment processor |
| `postgres` | internal only | PostgreSQL 16 |

To reset all data, run `docker compose down -v`.

## Walkthrough

The examples use bash (on Windows, Git Bash works). Responses are trimmed. Every response is JSON, and every error has the shape `{"error": {"code", "message", "details"?}}`.

**0. Create a business.** Signup is out of scope, so an operator endpoint creates a business along with its first API key. The key is shown only once.

```bash
curl -s -X POST localhost:8080/v1/admin/businesses \
  -H "Authorization: Bearer local-dev-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"name": "Acme Inc"}'
```
```json
{"business": {"id": "01a0cdfb-882e-...", "name": "Acme Inc", ...},
 "api_key": {"id": "01a0cdfb-8831-...", "display_prefix": "sk_CYh8md0m", "secret": "sk_CYh8md0mzm2x..."}}
```
```bash
export KEY=sk_...   # the api_key.secret from above
```

**1. Register a webhook endpoint** (here, the demo sink):

```bash
curl -s -X POST localhost:8080/v1/webhook_endpoints \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"url": "http://webhook-sink:9191/hooks/demo"}'
```
```json
{"id": "01a0cdfb-8d5a-...", "url": "http://webhook-sink:9191/hooks/demo", "disabled_at": null,
 "signing_secret": "whsec_4_tzCV3i8p..."}
```

**2. Create a customer**

```bash
curl -s -X POST localhost:8080/v1/customers \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"name": "Ada Lovelace", "email": "ada@example.com"}'
```
```json
{"id": "01a0cdfb-8fa8-7798-81e2-7f82f9c49951", "name": "Ada Lovelace", "email": "ada@example.com", "created_at": "..."}
```

**3. Create an invoice.** The server computes each line amount and the total, in integer cents. A client-sent `total_cents`, or a fractional amount like `19.99`, is rejected with `422 validation_failed`.

```bash
curl -s -X POST localhost:8080/v1/invoices \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{
    "customer_id": "01a0cdfb-8fa8-7798-81e2-7f82f9c49951",
    "due_date": "2026-12-31",
    "line_items": [
      {"description": "Pro plan seat", "quantity": 2, "unit_amount_cents": 2000},
      {"description": "Overage",       "quantity": 1, "unit_amount_cents": 998}
    ]
  }'
```
```json
{"id": "01a0cdfb-945d-756a-a62e-22079ef9366e", "status": "open", "currency": "USD", "total_cents": 4998,
 "line_items": [{"description": "Pro plan seat", "quantity": 2, "unit_amount_cents": 2000, "amount_cents": 4000}, ...],
 "payment_attempts": []}
```

**4. Pay it (success).** `Idempotency-Key` is required.

```bash
curl -s -i -X POST localhost:8080/v1/invoices/01a0cdfb-945d-756a-a62e-22079ef9366e/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: 3f1c7b1e-demo-1" \
  -d '{"card_token": "tok_success"}'
```
```
HTTP/1.1 200 OK
{"invoice": {"status": "paid", "paid_at": "...", "total_cents": 4998, ...},
 "payment_attempt": {"status": "succeeded", "amount_cents": 4998, "psp_ref": "931b0d9f-...", ...}}
```

Running the same command again returns the identical body with `idempotent-replayed: true`, and the PSP is not called a second time. Paying with a new key returns `409 invoice_already_paid`. Reusing the key with a different `card_token` returns `422 idempotency_key_reused`.

**5. Pay a second invoice (failure).** Create another invoice as in step 3, then:

```bash
curl -s -i -X POST localhost:8080/v1/invoices/<invoice id>/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: 9b2d-demo-3" \
  -d '{"card_token": "tok_card_declined"}'
```
```
HTTP/1.1 402 Payment Required
{"error": {"code": "card_declined", "message": "The card was declined",
           "details": {"invoice": {"status": "open", ...},
                       "payment_attempt": {"status": "failed", "failure_code": "card_declined", ...}}}}
```

The invoice goes back to `open`, so it can be paid again.

**6. See the webhooks**

```bash
curl -s localhost:9191/received
```

The sink shows `invoice.created`, `invoice.paid`, and `invoice.payment_failed`. Each carries a `Dodo-Signature` header and the exact raw body that was signed. Events can also be read back from `GET /v1/events`.

**The slow PSP.** `tok_timeout` makes the mock PSP wait 30 s. The endpoint gives up after 3 s and returns `202` with the invoice in `processing` and the attempt `pending`. About 30 s later the reconciler learns the result from the PSP and marks the invoice `paid`, which also sends `invoice.paid`. `tok_network_error` also returns `202`. Once it is certain the PSP never recorded the charge, the attempt fails with `psp_no_record` and the invoice returns to `open`. DESIGN.md §3 explains why neither case is reported as failed straight away.

### Mock PSP tokens

| `card_token` | Result |
|---|---|
| `tok_success` | Succeeds after ~100 ms |
| `tok_insufficient_funds` | Fails (`insufficient_funds`) after ~100 ms |
| `tok_card_declined` | Fails (`card_declined`) after ~100 ms |
| `tok_timeout` | Succeeds, but only after 30 s |
| `tok_network_error` | HTTP 500, nothing recorded |
| anything else | Fails (`invalid_card_token`) |

## Tests

The tests need a PostgreSQL server. `sqlx::test` creates a fresh database for each test.

With a local Rust toolchain (1.89+):

```bash
docker run -d --name invoicing-test-db -p 5433:5432 -e POSTGRES_PASSWORD=postgres postgres:16-alpine
DATABASE_URL=postgres://postgres:postgres@localhost:5433/postgres cargo test
```

Without Rust installed, everything runs in containers:

```bash
docker network create invoicing-test
docker run -d --name invoicing-test-db --network invoicing-test -e POSTGRES_PASSWORD=postgres postgres:16-alpine
docker run --rm --network invoicing-test -v "$PWD":/src -w /src \
  -e DATABASE_URL=postgres://postgres:postgres@invoicing-test-db/postgres rust:1.89 cargo test
```

In Git Bash on Windows, prefix the last command with `MSYS_NO_PATHCONV=1` and use `"$(pwd -W)"` in place of `"$PWD"`.

The three tests the assignment requires, none skipped:

| Requirement | Test |
|---|---|
| N concurrent `POST /pay` on one invoice: one success, no double charge | `invoicing/tests/payments_concurrency.rs`: 25 concurrent requests. It asserts exactly one 200, one PSP charge of the invoice total, one succeeded attempt, and a final state of `paid`. A second test sends 25 concurrent requests with the same key. |
| Same key retried: same response, no second PSP call | `invoicing/tests/payments_idempotency.rs`, which also covers declined replays, key reuse with a different body, and paying a paid invoice |
| PSP failure leaves no stuck invoice | `invoicing/tests/payments_psp_failures.rs`: `tok_timeout` returns `202` in under 1 s and settles to `paid`; `tok_network_error` settles back to `open` and can be paid again |

Beyond those: `webhooks.rs` (signatures, retry scheduling, duplicate endpoints), `api_keys.rs` (rotation and revocation), `events.rs` (the event feed never skips a late-committing event), and `validation.rs` (bad input is a 4xx, never a 500). The PSP-failure tests shorten the mock's 30 s delay to 2 s, so they take seconds rather than minutes; the code path is the same.

## Configuration

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | required | |
| `ADMIN_TOKEN` | required | At least 16 characters. Guards `/v1/admin/*`. |
| `PSP_BASE_URL` | required | |
| `BIND_ADDR` | `0.0.0.0:8080` | |
| `PSP_TIMEOUT_MS` | `3000` | How long `/pay` waits for the PSP before answering `202`. Must be under 30000. |
| `PSP_NOT_FOUND_GRACE_MS` | `30000` | How long the PSP may have no record of an attempt before it is failed. Must be greater than `PSP_TIMEOUT_MS`. |
| `WEBHOOK_TIMEOUT_MS` | `10000` | Per delivery attempt. Must be under 60000. |
| `WORKER_POLL_INTERVAL_MS` | `500` | Reconciler and webhook dispatcher poll interval |
| `RUST_LOG` | `info,sqlx=warn` | |

The service refuses to start if any timeout or the poll interval is zero.

## Layout

```
invoicing/            the service
  migrations/         SQL migrations (run on startup)
  src/payments.rs     POST /pay: claim, charge, settle
  src/reconciler.rs   resolves attempts whose PSP outcome was unknown
  src/invoices/       handlers, pricing, and the state machine
  src/webhooks/       endpoints, signing, dispatcher
  tests/              integration tests
harness/              mock PSP and webhook sink (library + binaries)
```
