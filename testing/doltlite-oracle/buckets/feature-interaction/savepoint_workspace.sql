.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'base');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
SAVEPOINT application_work;
INSERT INTO t VALUES (2, 'rolled back');
ROLLBACK TO application_work;
RELEASE application_work;
.output stdout
SELECT id, v
FROM t
ORDER BY id;
SELECT count(*) FROM dolt_diff;
