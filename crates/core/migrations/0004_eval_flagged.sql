-- 0004: Eval-flagged event ranges (§Phase 5 observability).
--
-- Operators flag a range of events on a conversation as a regression
-- target. The CLI (`execlaw eval flag <conv> --range a..b --label X`)
-- writes a row here; the LLM-judge harness reads matching rows to
-- pick which traces to replay against rubrics.
--
-- Range is inclusive on both ends (from_seq..=to_seq) — matching how
-- operators think about "events 12 through 48 are the bad turn."
CREATE TABLE IF NOT EXISTS eval_flagged (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT    NOT NULL,
    from_seq        INTEGER NOT NULL,
    to_seq          INTEGER NOT NULL,
    label           TEXT    NOT NULL,
    tags_json       BLOB,                -- JSON array of free-form tags
    flagged_by      TEXT    NOT NULL,    -- principal_id of the operator
    flagged_at      INTEGER NOT NULL,
    notes           TEXT
);

CREATE INDEX IF NOT EXISTS idx_eval_flagged_label
    ON eval_flagged(label, flagged_at);

CREATE INDEX IF NOT EXISTS idx_eval_flagged_conversation
    ON eval_flagged(conversation_id, from_seq);
