-- Per-pairing long-lived shared secret for transports that cannot authenticate
-- a sender themselves (email). Hash only, never plaintext. NULL means "this
-- pairing needs no token", which is every pre-existing row — so Matrix
-- behaviour is unchanged.
ALTER TABLE pairings ADD COLUMN token_sha256 TEXT;
