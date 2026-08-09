ALTER TABLE users
ADD COLUMN local_credential_kind TEXT NOT NULL DEFAULT 'password'
CHECK (local_credential_kind IN ('password', 'passwordless'));

UPDATE users
SET local_credential_kind = 'passwordless'
WHERE original_password_hash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';
