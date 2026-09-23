# AI usage

## Tools and what I used them for

**First version.** I wrote most of the first version myself and used Kimi 2.5 for boilerplate and for questions along the way.

**kimi 2.5 open sourced, after the first version worked.** I used it for a lot from this point on, so here is the full list:

- Turning the assignment docx into a checklist, which I used to see what was still missing.
- Reviewing the code. It found real bugs, and I had it fix them. The ones that mattered most:
  - A temporary `409 payment_in_progress` was being saved against the idempotency key, so a client that retried with the same key after the other payment failed got the old 409 forever.
  - `GET /v1/events` could skip an event for good. Event ids are generated before commit, so a transaction that commits late can land behind a checkpoint the business already saved. The fix was a `seq` column assigned in commit order.
  - Revoking your last API key locked the business out with no way back in.
  - A NUL character in a name gave a 500 instead of a 422, because Postgres text can't store it.
  - Registering the same webhook URL twice doubled every delivery.
- Writing the Dockerfile and docker-compose.yml.
- First drafts of DESIGN.md, README.md and openapi.yaml.
- A step-by-step run sheet for the demo video.

Rust isn't installed on my machine, so every build and test ran inside Docker.

## Decisions I made myself

1. **Rust, with Axum, sqlx and Postgres.** The brief strongly prefers Rust, so I went with it and picked this stack myself.

2. **Testing the whole flow by hand before recording.** I ran every step in Git Bash against `docker compose up`. My second payment came back `409 invoice_already_paid`, and I worked out that I had sent it to the invoice I had just paid. That's the service correctly refusing to charge a paid invoice, which is case (e) in DESIGN.md. Then I checked the `tok_timeout` invoice straight away and it still said `processing`. I waited and watched it turn `paid` on its own about 35 seconds later. Grepping the logs, the line that settled it had no HTTP request prefix, which is how I saw for myself that the reconciler did it and not a request.

3. **Using the payment attempt's id as the PSP's idempotency key.** The obvious alternative was to generate a separate key for the PSP call, or to reuse the client's own `Idempotency-Key`. I reused the attempt id because it ties one row in our database to exactly one charge at the PSP. If the same charge gets sent twice, the PSP only takes the money once. And if we never hear back, because of a timeout or a crash, we can ask the PSP about that exact attempt later. That lookup is what the reconciler depends on.

## What the AI got wrong

- **An `i32` / `bigint` mismatch that only a test caught.** While fixing the event feed it wrote `match params.after { None => 0, Some(id) => query_scalar(...) }`. Rust inferred `i32` from the bare `0`, and sqlx then failed at runtime trying to decode the Postgres `bigint` column into it, so `?after=` returned a 500. It compiled and clippy was clean. The new integration test failed, and the fix was typing the value as `i64`.
- **A regression from its own fix.** To stop Docker's health checks from flooding the logs, it moved `/healthz` outside the logging layer. That also moved it outside the 405 handler, so `POST /healthz` returned an empty 405 instead of our JSON error format. A later review pass caught it, and a test covers it now.
- **An OpenAPI draft that didn't validate.** It referenced a `WebhookEndpoint` schema it never defined. The Redocly linter caught it.
- **The first DESIGN.md draft was too long**, around 1,850 words against the 800 to 1,500 the brief asks for. It was trimmed down.

## How I checked the work

- `cargo test`: 29 tests, including the three the brief requires (concurrency, idempotency, PSP failure), plus `cargo clippy -D warnings` and `cargo fmt --check`. All of it ran in Docker against a real Postgres.
- The Redocly linter on `openapi.yaml`.
- The full demo flow by hand against `docker compose up`: success, replay, decline, and the timeout settling to paid.
