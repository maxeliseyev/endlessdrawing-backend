CREATE TABLE admin_accounts (
    id                  uuid PRIMARY KEY,
    login               text NOT NULL UNIQUE CHECK (login ~ '^[a-z0-9][a-z0-9_.-]{2,63}$'),
    password_hash       text NOT NULL,
    enabled             boolean NOT NULL DEFAULT true,
    auth_version        bigint NOT NULL DEFAULT 1,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE admin_credentials (
    credential_id       bytea PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES admin_accounts(id),
    credential          jsonb NOT NULL,
    counter             bigint NOT NULL CHECK (counter BETWEEN 0 AND 4294967295),
    label               text NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    revoked_at          timestamptz
);
CREATE INDEX admin_credentials_account ON admin_credentials(account_id) WHERE revoked_at IS NULL;

CREATE TABLE admin_enrollments (
    token_digest        bytea PRIMARY KEY CHECK (octet_length(token_digest) = 32),
    account_id          uuid NOT NULL REFERENCES admin_accounts(id),
    auth_version        bigint NOT NULL,
    remaining           smallint NOT NULL DEFAULT 2 CHECK (remaining BETWEEN 0 AND 2),
    expires_at          timestamptz NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);

-- Ceremony state is server-side only. The cookie is a random opaque lookup token.
CREATE TABLE admin_preauth (
    token_digest        bytea PRIMARY KEY CHECK (octet_length(token_digest) = 32),
    origin              text NOT NULL,
    policy_digest       bytea NOT NULL CHECK (octet_length(policy_digest) = 32),
    expires_at          timestamptz NOT NULL,
    kind                text CHECK (kind IN ('login', 'enrollment')),
    account_id          uuid REFERENCES admin_accounts(id),
    auth_version        bigint,
    challenge_state     jsonb,
    challenge_expires_at timestamptz,
    enrollment_digest   bytea,
    label               text
);
CREATE INDEX admin_preauth_expiry ON admin_preauth(expires_at);

CREATE TABLE admin_sessions (
    token_digest        bytea PRIMARY KEY CHECK (octet_length(token_digest) = 32),
    account_id          uuid NOT NULL REFERENCES admin_accounts(id),
    auth_version        bigint NOT NULL,
    origin              text NOT NULL,
    policy_digest       bytea NOT NULL CHECK (octet_length(policy_digest) = 32),
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_seen_at        timestamptz NOT NULL DEFAULT now(),
    expires_at          timestamptz NOT NULL,
    revoked_at          timestamptz
);
CREATE INDEX admin_sessions_account ON admin_sessions(account_id);
CREATE INDEX admin_sessions_expiry ON admin_sessions(expires_at);

CREATE TABLE moderation_operations (
    id                  uuid PRIMARY KEY,
    actor_id            uuid NOT NULL REFERENCES admin_accounts(id),
    payload_digest      bytea NOT NULL CHECK (octet_length(payload_digest) = 32),
    reason              text NOT NULL CHECK (length(reason) BETWEEN 1 AND 1000 AND octet_length(reason) <= 4000),
    result              jsonb NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE moderation_operation_strokes (
    operation_id        uuid NOT NULL REFERENCES moderation_operations(id),
    stroke_id           uuid NOT NULL REFERENCES strokes(id),
    outcome             text NOT NULL CHECK (outcome IN ('hidden', 'already_hidden')),
    PRIMARY KEY (operation_id, stroke_id)
);
CREATE TABLE admin_security_audit (
    seq                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id          uuid REFERENCES admin_accounts(id),
    event               text NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);
