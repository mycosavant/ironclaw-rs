-- Add hash-chain audit trail columns to job_actions.
-- content_hash: blake3 hash of canonical action fields (tamper-evident).
-- prev_hash: content_hash of the previous action in the chain (linkage).
ALTER TABLE job_actions ADD COLUMN content_hash TEXT;
ALTER TABLE job_actions ADD COLUMN prev_hash TEXT;
