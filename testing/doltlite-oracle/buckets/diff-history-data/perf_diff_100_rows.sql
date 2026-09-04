.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
WITH RECURSIVE nums(id) AS (
    SELECT 1
    UNION ALL
    SELECT id + 1 FROM nums WHERE id < 100
)
INSERT INTO t SELECT id, 'value-' || id FROM nums;
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
UPDATE t SET v = 'changed';
.output stdout
SELECT count(*) FROM t;
