-- 0007: WebAuthn passkey credentials (Phase 7e).
--
-- Stores every registered WebAuthn credential per user. Used as a
-- *second* factor: the user still presents their password; if the
-- user has any rows in this table, the login route returns
-- `webauthn_required` instead of issuing tokens, and the SPA must
-- complete the assertion ceremony before tokens land.
--
-- `credential_id` is the raw bytes the authenticator returns
-- (variable length; per spec ≤ 1023 bytes). We store it base64-url
-- encoded as TEXT so it can serve as the primary key without binary
-- comparison surprises.
--
-- `passkey_json` is the serialised `webauthn_rs::Passkey` blob —
-- contains the public key, counter, AAGUID, and attestation metadata.
-- Treated as opaque from execlaw's side; webauthn-rs round-trips it.
--
-- `last_used_at` tracks when the credential was last successfully
-- used to authenticate — surfaced in the Settings → Profile UI so
-- the operator can spot dead credentials before removing them.
CREATE TABLE IF NOT EXISTS state_webauthn_credentials (
    credential_id  TEXT    PRIMARY KEY,    -- base64url(authenticator-returned id)
    user_id        TEXT    NOT NULL,
    label          TEXT    NOT NULL,       -- operator-supplied nickname ("YubiKey 5C", "MacBook TouchID")
    passkey_json   TEXT    NOT NULL,       -- serde_json(webauthn_rs::Passkey)
    counter        INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL,
    last_used_at   INTEGER,
    FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_webauthn_credentials_user_id
    ON state_webauthn_credentials(user_id);
