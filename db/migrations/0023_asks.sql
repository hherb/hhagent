-- 0023_asks.sql
--
-- The durable ask record (#564 slice 1a): a correlated, deadline-bounded
-- question the daemon raises for a human, plus the `awaiting_operator`
-- state a task sits in while one is outstanding.
--
-- Design: docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md
--
-- Three parts:
--   1. the `asks` table + its two indexes
--   2. `tasks_state_check` widened with 'awaiting_operator'
--   3. the `tasks_resumed` NOTIFY trigger (awaiting_operator -> pending)
--
-- `notify_task_completed` (0005, last replaced in 0012) is deliberately
-- NOT touched. 'awaiting_operator' is not terminal, so it must not appear
-- in that function's NEW.state list — and because it is also absent from
-- the OLD.state list, an expiry transition awaiting_operator -> failed
-- still fires `tasks_completed` exactly as it should.

BEGIN;

-- (1) The record. `nonce_sha256` is a HASH, never the nonce: the plaintext
-- is returned to the caller once by `db::asks::raise` and never stored, so
-- a DB read cannot recover a live token. Same posture as
-- `pairing_codes.code_sha256` in 0018.
--
-- `plan_digest` is nullable because it is meaningful only for kinds that
-- bind to a plan ('plan_approval' today). A future 'ask_user' kind binds to
-- no plan and stores NULL.
--
-- `resolution` is a CLOSED set: {choice} indexing into `options`, plus an
-- optional free_text kept for the audit row and shown to the operator.
-- Free text is never interpolated into a plan — otherwise the ask channel
-- becomes an injection funnel aimed at the reviewer's own decision.
CREATE TABLE asks (
    id            BIGSERIAL   PRIMARY KEY,
    task_id       BIGINT      NOT NULL REFERENCES tasks(id),
    kind          TEXT        NOT NULL,
    body          TEXT        NOT NULL,
    options       JSONB       NOT NULL,
    plan_digest   TEXT,
    nonce_sha256  TEXT        NOT NULL,
    state         TEXT        NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','resolved','expired','cancelled')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    deadline_at   TIMESTAMPTZ NOT NULL,
    resolved_at   TIMESTAMPTZ,
    resolved_by   TEXT,
    resolution    JSONB
);

-- Partial index: the expiry sweep only ever scans pending rows. Mirrors
-- `pairing_codes_claimable` from 0018.
CREATE INDEX asks_pending_deadline ON asks (deadline_at) WHERE state = 'pending';
-- Every read from the task side ("does this task have an open ask?").
CREATE INDEX asks_task ON asks (task_id);

-- One pending ask per task, enforced by the database rather than only by
-- `raise`'s application-level guard (`UPDATE tasks … WHERE state =
-- 'running'`, which only stops a SECOND raise against the same task from
-- the same process — it is not a substitute for a constraint). This is
-- exactly the invariant `asks::resolve`'s loud `Err` branch exists to
-- detect a violation of; making it a UNIQUE index turns "detect after the
-- fact" into "cannot happen".
CREATE UNIQUE INDEX asks_one_pending_per_task ON asks (task_id) WHERE state = 'pending';

-- The nonce lookup `asks::resolve_with_nonce` (#564 fix wave) performs
-- would otherwise seq-scan a table that, by design (no DELETE grant),
-- never shrinks. UNIQUE also makes a nonce collision impossible rather
-- than merely improbable — cheap insurance on top of `nonce_sha256` being
-- SHA-256 over 32 bytes of OS CSPRNG output.
CREATE UNIQUE INDEX asks_nonce ON asks (nonce_sha256);

-- (2) The suspended task state.
ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed','refused','awaiting_operator'));

-- (3) Resume wake-up. `tasks_inserted` fires AFTER INSERT only, so an
-- awaiting_operator -> pending UPDATE wakes nobody and the resumed task
-- waits out the lane runner's 30 s HEARTBEAT. A dedicated channel rather
-- than overloading `tasks_inserted`: a channel name that no longer
-- describes what it carries is the trap that broke upgrade_from_git.sh's
-- own post-deploy check in the #516 arc.
CREATE OR REPLACE FUNCTION notify_task_resumed()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state = 'pending' AND OLD.state = 'awaiting_operator' THEN
        PERFORM pg_notify('tasks_resumed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tasks_notify_resumed ON tasks;
CREATE TRIGGER tasks_notify_resumed
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_resumed();

-- (4) Grants. No DELETE: an ask transitions through terminal states and
-- stays, mirroring the append-only-by-GRANT posture `tasks` and
-- `audit_log` already take.
GRANT  SELECT, INSERT, UPDATE ON asks TO kastellan_runtime;
GRANT  USAGE, SELECT ON SEQUENCE asks_id_seq TO kastellan_runtime;
REVOKE DELETE, TRUNCATE ON asks FROM kastellan_runtime;

COMMIT;
