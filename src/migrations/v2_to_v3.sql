-- v2 -> v3 migration. Additive only — three new views for the
-- B3 auto-populated status doc surface:
--
--   * status_summary_per_extension — total function counts by
--     (extension, status) with the kind axis folded away.
--   * leaf_coverage — per-leaf functions-with-cases and
--     verified-functions counters, plus a coverage percent.
--   * verification_freshness — one row per verified scalar.
--
-- Runs under a single transaction. Idempotent thanks to
-- `CREATE VIEW IF NOT EXISTS`; the migration function is gated
-- on `user_version = 2` so double-application won't happen in
-- practice either.

BEGIN;

CREATE VIEW IF NOT EXISTS status_summary_per_extension AS
    SELECT extension, status, COUNT(*) AS n
    FROM (
        SELECT extension, status FROM scalars
        UNION ALL SELECT extension, status FROM aggregates
        UNION ALL SELECT extension, status FROM table_functions
        UNION ALL SELECT extension, status FROM window_functions
    )
    GROUP BY extension, status;

CREATE VIEW IF NOT EXISTS leaf_coverage AS
    SELECT
        tc.extension,
        COALESCE(
            (SELECT j.value FROM json_each(tc.tags_json) AS j
                WHERE j.value LIKE 'leaf:%' LIMIT 1),
            json_extract(tc.tags_json, '$[0]')
        ) AS leaf,
        COUNT(DISTINCT tc.function_name) AS functions_with_cases,
        SUM(CASE WHEN s.status = 'implemented_verified' THEN 1 ELSE 0 END)
            AS verified_functions,
        ROUND(
            100.0 *
            SUM(CASE WHEN s.status = 'implemented_verified' THEN 1 ELSE 0 END)
            / COUNT(DISTINCT tc.function_name),
            1
        ) AS coverage_pct
    FROM test_cases tc
    LEFT JOIN scalars s
      ON s.extension = tc.extension AND s.name = tc.function_name
    GROUP BY tc.extension, leaf;

CREATE VIEW IF NOT EXISTS verification_freshness AS
    SELECT extension, name, status, last_verified_at,
           last_verified_upstream_version
    FROM scalars
    WHERE status = 'implemented_verified';

PRAGMA user_version = 3;

COMMIT;
