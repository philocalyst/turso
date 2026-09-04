.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'a');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
INSERT INTO t VALUES (2, 'b');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'second');
.output stdout
SELECT id, v
FROM dolt_history_t('HEAD')
ORDER BY id;
SELECT id, v
FROM dolt_at_t('HEAD')
ORDER BY id;
