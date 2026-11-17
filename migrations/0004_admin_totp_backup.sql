-- Owner-approved policy: one UV passkey for primary login, optional password+TOTP backup.
-- Existing credentials are preserved. Old sessions/ceremonies are invalidated by policy v2.
ALTER TABLE admin_sessions ADD COLUMN auth_method text NOT NULL DEFAULT 'passkey_uv'
    CHECK (auth_method IN ('passkey_uv', 'totp'));

CREATE TABLE admin_totp (
    account_id uuid PRIMARY KEY REFERENCES admin_accounts(id),
    encrypted_secret bytea CHECK (octet_length(encrypted_secret) = 49),
    last_used_step bigint,
    enabled_at timestamptz,
    pending_secret bytea CHECK (octet_length(pending_secret) = 49),
    pending_expires_at timestamptz,
    pending_session_digest bytea CHECK (octet_length(pending_session_digest) = 32),
    CHECK ((encrypted_secret IS NULL) = (enabled_at IS NULL)),
    CHECK ((pending_secret IS NULL) = (pending_expires_at IS NULL)),
    CHECK ((pending_secret IS NULL) = (pending_session_digest IS NULL))
);
