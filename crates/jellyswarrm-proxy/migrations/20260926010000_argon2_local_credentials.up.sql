ALTER TABLE users RENAME COLUMN local_credential_kind TO old_credential_kind;
ALTER TABLE users ADD COLUMN local_credential_kind TEXT NOT NULL DEFAULT 'password'
    CHECK (local_credential_kind IN ('password', 'passwordless', 'argon2id'));
UPDATE users SET local_credential_kind = old_credential_kind;
ALTER TABLE users DROP COLUMN old_credential_kind;
