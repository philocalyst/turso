.output /dev/null
SELECT dolt_config('user.name', 'Ada');
SELECT dolt_config('user.email', 'ada@example.com');
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'base');
SELECT dolt_add('-A');
SELECT dolt_commit('-m', 'seed');
SELECT dolt_branch('feature');
SELECT dolt_tag('v1');
.output stdout
SELECT name, latest_commit_message, dirty
FROM dolt_branches
ORDER BY name;
SELECT tag_name, message
FROM dolt_tags
ORDER BY tag_name;
