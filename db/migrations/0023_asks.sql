-- 0023_asks.sql
--
-- The durable ask record (#564 slice 1a): a correlated, deadline-bounded
-- question the daemon raises for a human, plus the `awaiting_operator`
-- state a task sits in while one is outstanding.
--
-- Design: docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md
--
-- Four parts:
--   1. the `asks` table + its four indexes
--   2. `tasks_state_check` widened with 'awaiting_operator'
--   3. the `tasks_resumed` NOTIFY trigger (awaiting_operator -> pending)
--   4. the `kastellan_runtime` grants
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
--
-- Every one of the four CHECKs below exists because the property it pins
-- was, in review, asserted only in prose. A comment saying "closed set"
-- over a bare JSONB column is a contract on a caller that does not exist
-- yet (slice 1b/2), and the caller that eventually arrives reads the
-- column, not the comment.
CREATE TABLE asks (
    id            BIGSERIAL   PRIMARY KEY,
    task_id       BIGINT      NOT NULL REFERENCES tasks(id),
    -- Closed set, like `state`. A new ask kind is a migration, deliberately:
    -- `kind` selects slice 1b's dispatch, so an unrecognised value there is
    -- a silent no-op on a question a human was asked to answer.
    kind          TEXT        NOT NULL
                  CHECK (kind IN ('plan_approval')),
    body          TEXT        NOT NULL,
    -- A non-empty JSON array. An ask whose `options` is `[]`, `null`, or a
    -- scalar renders as a question with no answers: unanswerable, so the
    -- task waits out the full deadline for a reply that cannot be given.
    options       JSONB       NOT NULL
                  CHECK (jsonb_typeof(options) = 'array'
                         AND jsonb_array_length(options) > 0),
    plan_digest   TEXT,
    nonce_sha256  TEXT        NOT NULL,
    state         TEXT        NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','resolved','expired','cancelled')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Compared against PG's `now()` by `expire_due`, so it is pinned
    -- against PG's own clock here rather than against the caller's:
    -- `created_at` defaults to `transaction_timestamp()`, and a daemon
    -- whose clock trails the database's would otherwise mint an ask that
    -- is already expirable. A past deadline is always a caller bug.
    deadline_at   TIMESTAMPTZ NOT NULL,
    resolved_at   TIMESTAMPTZ,
    resolved_by   TEXT,
    resolution    JSONB,

    CONSTRAINT asks_deadline_after_created CHECK (deadline_at > created_at),

    -- `resolved` is all-or-nothing. Without this a half-resolved row is
    -- representable — and `kastellan_runtime` holds blanket UPDATE, so
    -- "only the Rust path writes all four together" is a property of
    -- today's callers, not of the table.
    CONSTRAINT asks_resolved_is_complete CHECK (
        (state = 'resolved') = (resolved_at IS NOT NULL
                                AND resolved_by IS NOT NULL
                                AND resolution  IS NOT NULL)
    )
);

-- Partial index: the expiry sweep only ever scans pending rows. Mirrors
-- `pairing_codes_claimable` from 0018.
CREATE INDEX asks_pending_deadline ON asks (deadline_at) WHERE state = 'pending';
-- A task's ask HISTORY, which accumulates: a task may escalate more than
-- once over its life (approved on plan 2, escalating again on plan 4), and
-- every ask it ever raised stays on the table because there is no DELETE
-- grant. Serves the operator-facing "show me this task's questions" read
-- and `cancel_for_task`'s task-scoped sweep.
CREATE INDEX asks_task ON asks (task_id);

-- One **pending** ask per task — partial, and the partiality is the point:
-- a task that resolved one ask must be able to raise the next. A plain
-- UNIQUE (task_id) would let the first escalation succeed and every
-- subsequent one fail forever, which is a mainline slice-1b flow.
--
-- The database rather than only `raise`'s application-level guard
-- (`UPDATE tasks … WHERE state = 'running'`). That guard is a row-
-- conditional UPDATE and so IS cross-process — two concurrent `raise(T)`
-- calls serialize on the tasks row lock and the loser re-evaluates against
-- the committed `awaiting_operator` and matches nothing. What it does not
-- cover is a writer that reaches `asks` without going through `raise` at
-- all: direct SQL, a future call site, or a task returned to `running`
-- with a stale pending ask still attached. This is exactly the invariant
-- `asks::resolve`'s loud `Err` branch exists to detect a violation of;
-- making it a UNIQUE index turns "detect after the fact" into "cannot
-- happen".
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
-- than overloading `tasks_inserted`, whose name would then no longer
-- describe what it carries — the same class of trap as the renamed LOG
-- LINE in the #516 arc, which broke `upgrade_from_git.sh`'s post-deploy
-- grep. (That was a log line, not a NOTIFY channel; the lesson transfers,
-- the mechanism does not.)
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
