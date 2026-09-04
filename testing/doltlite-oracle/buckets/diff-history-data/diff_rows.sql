.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'a');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
INSERT INTO t VALUES (2, 'b');
UPDATE t SET v = 'aa' WHERE id = 1;
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'second');
.output stdout
SELECT from_id, to_id, diff_type
FROM dolt_diff_t('HEAD~1', 'HEAD')
ORDER BY diff_type, to_id;
SELECT table_name, rows_added, rows_deleted, rows_modified
FROM dolt_diff_stat('HEAD~1', 'HEAD')
ORDER BY table_name;
