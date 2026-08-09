-- The structural fallback is gone: the LLM is a hard dependency, so a window
-- it refuses is now a hole rather than a worse split. Rows written under the
-- old name describe the same situation, and are renamed rather than reset —
-- the window really did spend its attempts. The debris chunks an old fallback
-- produced stay where they are; deleting them would silently shrink sources
-- that have been searchable for months.
UPDATE segment_windows SET state = 'failed' WHERE state = 'fallback';
