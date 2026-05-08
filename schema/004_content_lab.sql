-- Content Lab ingestion worker — idempotency columns.
--
-- !! RUN THIS MANUALLY ON THE DATABASE BEFORE DEPLOYING THE WORKER !!
--
-- Adds lab_job_id to content.jobs and lab_distribution_id to content.distributions
-- so the ingestion worker can upsert without duplicating rows.
-- Safe to run multiple times (IF NOT EXISTS / DO NOTHING guards).

ALTER TABLE content.jobs
  ADD COLUMN IF NOT EXISTS lab_job_id text;

ALTER TABLE content.distributions
  ADD COLUMN IF NOT EXISTS lab_distribution_id text;

-- Unique constraints — worker uses ON CONFLICT on these.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'content_jobs_lab_job_id_key'
  ) THEN
    ALTER TABLE content.jobs ADD CONSTRAINT content_jobs_lab_job_id_key UNIQUE (lab_job_id);
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'content_distributions_lab_distribution_id_key'
  ) THEN
    ALTER TABLE content.distributions ADD CONSTRAINT content_distributions_lab_distribution_id_key UNIQUE (lab_distribution_id);
  END IF;
END $$;
