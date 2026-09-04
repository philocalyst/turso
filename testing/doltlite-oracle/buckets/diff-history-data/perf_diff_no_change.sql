.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'base');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
.output stdout
SELECT count(*) FROM dolt_diff;
