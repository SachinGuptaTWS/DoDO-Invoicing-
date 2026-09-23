-- A payment attempt's id is also the idempotency key we send to the PSP.
-- That is what makes "PSP charged us but we crashed" recoverable: we can ask
-- the PSP about this exact attempt, and re-sending it can never charge twice.
CREATE TABLE payment_attempts (
    id               uuid PRIMARY KEY,
    invoice_id       uuid        NOT NULL REFERENCES invoices (id),
    business_id      uuid        NOT NULL REFERENCES businesses (id),
    status           text        NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed')),
    amount_cents     bigint      NOT NULL CHECK (amount_cents > 0),
    psp_ref          text,
    failure_code     text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    settled_at       timestamptz,
    -- Reconciler bookkeeping; only meaningful while pending.
    reconcile_after  timestamptz,
    reconcile_count  int         NOT NULL DEFAULT 0,
    CHECK ((status = 'succeeded') = (psp_ref IS NOT NULL)),
    CHECK ((status = 'failed') = (failure_code IS NOT NULL)),
    CHECK ((status = 'pending') = (settled_at IS NULL)),
    CHECK ((status = 'pending') = (reconcile_after IS NOT NULL))
);

-- Backstops for the invoice status compare-and-set. If application logic
-- ever regresses, the database still refuses a second in-flight or a second
-- successful attempt for the same invoice.
CREATE UNIQUE INDEX payment_attempts_one_in_flight_idx ON payment_attempts (invoice_id) WHERE status = 'pending';
CREATE UNIQUE INDEX payment_attempts_one_success_idx   ON payment_attempts (invoice_id) WHERE status = 'succeeded';
CREATE INDEX payment_attempts_invoice_idx              ON payment_attempts (invoice_id, id);
CREATE INDEX payment_attempts_reconcile_due_idx        ON payment_attempts (reconcile_after) WHERE status = 'pending';

-- A row is only ever visible with either a stored response (request finished)
-- or a payment_attempt_id (money may be moving; response pending). Both are
-- written in the same transaction that reserves the key.
CREATE TABLE idempotency_keys (
    business_id          uuid        NOT NULL REFERENCES businesses (id),
    key                  text        NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    request_fingerprint  bytea       NOT NULL CHECK (length(request_fingerprint) = 32),
    payment_attempt_id   uuid        REFERENCES payment_attempts (id),
    response_status      smallint,
    response_body        jsonb,
    created_at           timestamptz NOT NULL DEFAULT now(),
    completed_at         timestamptz,
    PRIMARY KEY (business_id, key),
    CHECK ((response_status IS NULL) = (response_body IS NULL)),
    CHECK ((response_status IS NULL) = (completed_at IS NULL))
);

CREATE UNIQUE INDEX idempotency_keys_attempt_idx ON idempotency_keys (payment_attempt_id);
