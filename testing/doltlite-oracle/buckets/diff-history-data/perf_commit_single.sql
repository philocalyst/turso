.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'base');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
INSERT INTO t VALUES (2, 'one');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'single');
.output stdout
SELECT message FROM dolt_log WHERE message = 'single';
