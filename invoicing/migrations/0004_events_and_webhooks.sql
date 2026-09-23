CREATE TABLE webhook_endpoints (
    id              uuid PRIMARY KEY,
    business_id     uuid        NOT NULL REFERENCES businesses (id),
    url             text        NOT NULL CHECK (length(url) <= 2048),
    -- Plaintext because HMAC signing needs the raw secret. See DESIGN.md
    -- for why this is the one secret we cannot hash.
    signing_secret  text        NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    disabled_at     timestamptz
);

-- A URL registered twice would receive every event twice. Also serves the
-- fan-out lookup of a business's active endpoints.
CREATE UNIQUE INDEX webhook_endpoints_active_url_idx ON webhook_endpoints (business_id, url) WHERE disabled_at IS NULL;

-- The event log is the transactional outbox and the reconciliation source:
-- rows are written in the same transaction as the state change they describe.
--
-- `seq` is the feed cursor, not `id`. A UUIDv7 id is taken before commit, so
-- a transaction that commits later can hold a smaller id than one a consumer
-- already checkpointed past. `seq` is assigned while the business row is
-- locked (see events.rs), which makes its order the commit order.
CREATE TABLE events (
    id           uuid PRIMARY KEY,
    business_id  uuid        NOT NULL REFERENCES businesses (id),
    seq          bigint      GENERATED ALWAYS AS IDENTITY,
    event_type   text        NOT NULL,
    data         jsonb       NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX events_business_seq_idx ON events (business_id, seq);

CREATE TABLE webhook_deliveries (
    event_id              uuid        NOT NULL REFERENCES events (id),
    endpoint_id           uuid        NOT NULL REFERENCES webhook_endpoints (id),
    status                text        NOT NULL DEFAULT 'pending'
                                      CHECK (status IN ('pending', 'delivered', 'exhausted', 'cancelled')),
    attempt_count         int         NOT NULL DEFAULT 0,
    next_attempt_at       timestamptz NOT NULL DEFAULT now(),
    last_response_status  smallint,
    last_error            text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    delivered_at          timestamptz,
    PRIMARY KEY (event_id, endpoint_id)
);

CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
