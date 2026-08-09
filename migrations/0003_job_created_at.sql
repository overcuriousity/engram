-- `oldest_pending_age` needs to know when a job was enqueued. It previously
-- derived age from `run_after`, which is 0 for any job that was never delayed,
-- so the reported age was seconds-since-epoch rather than a waiting time.
ALTER TABLE jobs ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_jobs_created ON jobs(created_at);
