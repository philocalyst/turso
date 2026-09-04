.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'base');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
SELECT dolt_branch('feature');
SELECT dolt_checkout('feature');
UPDATE t SET v = 'feature' WHERE id = 1;
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'feature');
SELECT dolt_checkout('main');
SELECT dolt_merge('feature');
.output stdout
SELECT v FROM t ORDER BY id;
