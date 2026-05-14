-- Add an operator-facing cap on how many tokens of conversation history
-- the prompt builder includes per turn.
--
-- Pre-2026-05-14 the prompt builder loaded EVERY event from `seq = 0`
-- forward on every turn — no truncation, no token accounting. With a
-- long-lived Signal thread we observed a 24× latency jump (657 ms web
-- UI vs ~24.5 s Signal) driven almost entirely by re-prefilling a
-- 25× larger conversation history. The same explosion will bite any
-- web chat that accumulates enough turns; it just bites Signal first
-- because the controller's main Signal thread grows fastest.
--
-- Char-to-token heuristic: ~4 chars per token for English. The default
-- 8000 tokens corresponds to ≈32 KB of message text. On the 27B Qwen
-- model (32K context default; 262K on 3.5) this leaves plenty of room
-- for system prompt + routing prose + turn context + the current user
-- message + reply generation. Operators can bump higher if they have
-- a big-context model.
--
-- Floor of 1000 is enforced at read time (see crates/core/src/history_budget.rs)
-- so a fat-fingered "100" doesn't drop the entire conversation; the
-- recent-pair guarantee in the truncation policy means even a
-- ridiculously small cap still surfaces the last exchange.

ALTER TABLE config_general
    ADD COLUMN max_history_tokens INTEGER NOT NULL DEFAULT 8000;
