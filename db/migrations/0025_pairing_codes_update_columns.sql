-- 0025: narrow the runtime role's UPDATE on pairing_codes to the two columns
-- `claim_code` actually writes (security audit 2026-09-02, channel F2).
--
-- 0018 granted table-wide UPDATE, so a compromised core (or anything holding
-- the runtime role) could rewrite `code_sha256` / `expires_at` on any
-- historical row — turning a consumed or expired operator-issued code back
-- into a live one, or replacing its hash with one the attacker knows, and
-- then pairing itself in through the bus's compare-only carve-out. The claim
-- path only ever flips `consumed_at` + `consumed_by` on a row matched by
-- `code_sha256 … AND consumed_at IS NULL AND expires_at > now()`, so that is
-- all the role needs. Column-level GRANTs are additive; the table-level
-- REVOKE first is what removes the wider right.
REVOKE UPDATE ON pairing_codes FROM kastellan_runtime;
GRANT UPDATE (consumed_at, consumed_by) ON pairing_codes TO kastellan_runtime;
