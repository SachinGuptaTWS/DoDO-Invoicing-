CREATE TABLE customers (
    id           uuid PRIMARY KEY,
    business_id  uuid        NOT NULL REFERENCES businesses (id),
    name         text        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    email        text        NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- Target of the composite FK on invoices: the database guarantees an
    -- invoice only references a customer of the same business.
    UNIQUE (id, business_id)
);

CREATE INDEX customers_business_page_idx ON customers (business_id, id DESC);

-- status is text + CHECK rather than a Postgres ENUM: enum values cannot be
-- dropped or renamed without rewriting the type, and state machines evolve.
CREATE TABLE invoices (
    id           uuid PRIMARY KEY,
    business_id  uuid        NOT NULL REFERENCES businesses (id),
    customer_id  uuid        NOT NULL,
    status       text        NOT NULL CHECK (status IN ('open', 'processing', 'paid', 'void')),
    currency     text        NOT NULL DEFAULT 'USD' CHECK (currency = 'USD'),
    total_cents  bigint      NOT NULL CHECK (total_cents > 0),
    due_date     date        NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    paid_at      timestamptz,
    voided_at    timestamptz,
    FOREIGN KEY (customer_id, business_id) REFERENCES customers (id, business_id),
    CHECK ((status = 'paid') = (paid_at IS NOT NULL)),
    CHECK ((status = 'void') = (voided_at IS NOT NULL))
);

CREATE INDEX invoices_business_page_idx        ON invoices (business_id, id DESC);
CREATE INDEX invoices_business_status_page_idx ON invoices (business_id, status, id DESC);
CREATE INDEX invoices_customer_idx             ON invoices (customer_id);

-- Line items are immutable once written (no draft/edit flow), so a natural
-- key of (invoice, position) is enough; no surrogate id is needed.
CREATE TABLE invoice_line_items (
    invoice_id         uuid   NOT NULL REFERENCES invoices (id),
    position           int    NOT NULL CHECK (position >= 0),
    description        text   NOT NULL CHECK (length(description) BETWEEN 1 AND 500),
    quantity           bigint NOT NULL CHECK (quantity > 0),
    unit_amount_cents  bigint NOT NULL CHECK (unit_amount_cents >= 0),
    amount_cents       bigint NOT NULL CHECK (amount_cents = quantity * unit_amount_cents),
    PRIMARY KEY (invoice_id, position)
);
