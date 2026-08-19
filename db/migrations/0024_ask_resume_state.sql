-- 0024_ask_resume_state.sql
--
-- The suspended run's step history, carried on the ask (#564 slice 1b, D11).
--
-- Design: docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md
--         (Addendum 2026-08-19 — D11)
--
-- WHY THIS COLUMN EXISTS
--
-- `Verdict::Escalate` suspends a task on a question for a human. Before
-- this column existed the resumed task rebuilt its loop context with an
-- EMPTY plan history, so it re-formulated every iteration it had already
-- run — and re-executed their steps. Plan 1 sends an email, plan 2
-- escalates, the operator approves, and on resume plan 1 is formulated
-- again and the email is sent a second time. The feature whose whole
-- purpose is human oversight caused duplicate side effects *because* a
-- human approved something.
--
-- So the suspension carries the run's history and the resume restores it.
--
-- WHY THE ASK ROW IS ITS HOME
--
-- The state belongs to *this* suspension, and there is exactly one ask per
-- suspension (`asks_one_pending_per_task` in 0023 makes that a database
-- fact, not a convention), so the ask row is where it naturally lives.
--
-- Not `tasks.payload`: that column is the PRODUCER's declared intent — the
-- instruction, the classification floor, the plan budget — and every one of
-- its readers treats it as such. Making it double as scheduler scratch
-- would mean the scheduler writing over a record of what the submitter
-- asked for, and would leave the scratch behind after the task ends, where
-- `tasks.payload`'s readers would keep seeing it.
--
-- WHAT IS IN IT
--
-- `{"plans": [{"plan": <Plan>, "outcomes": [<StepOutcome>]}],
--   "advisories": [<String>], "blocks": [<String>]}` — the INPUTS to
-- `PlanRecord::new`, not its renders. The screened, planner-bound render is
-- a pure function of `(plan, outcomes)`, so storing the inputs and calling
-- the same constructor on restore keeps the screened-once invariant a
-- property of the code rather than of whatever is in this column. See
-- `core::scheduler::asks::resume_state_from` / `restore_resume_state`.
--
-- Nullable, and a NULL restores as an empty history: an ask raised before
-- this column existed, or one raised by a future kind that binds to no run,
-- must still be answerable. A lost history costs a replay; a failed restore
-- would cost the operator's decision entirely.
--
-- No new GRANT. 0023's grants are TABLE-level
-- (`GRANT SELECT, INSERT, UPDATE ON asks TO kastellan_runtime`), and a
-- table-level grant covers columns added later — column-level grants would
-- not, which is why this comment states which kind 0023 used.

BEGIN;

ALTER TABLE asks ADD COLUMN resume_state JSONB;

COMMENT ON COLUMN asks.resume_state IS
    'The suspended run''s plan history + reviewer feedback, as the inputs to '
    'PlanRecord::new. Restored by the scheduler when the task resumes so it '
    'does not re-formulate (and re-execute) iterations it already ran. NULL '
    'restores as an empty history.';

COMMIT;
