WITH step_1 AS (
  SELECT * FROM "orders" WHERE "amount" > 100
),
step_2 AS (
  SELECT * FROM step_1 WHERE "status" IN ('paid', 'shipped')
),
step_3 AS (
  SELECT * FROM step_2 WHERE strpos(lower(CAST("note" AS VARCHAR)), lower('it''s')) > 0
),
step_4 AS (
  SELECT *, (amount * (1 - discount)) AS "net" FROM step_3
),
step_5 AS (
  SELECT * FROM step_4 AS l LEFT JOIN "customers" AS r ON l."customer_id" = r."id"
),
step_6 AS (
  SELECT "country", SUM("net") AS "sum_net", COUNT(*) AS "row_count", COUNT(DISTINCT "customer_id") AS "customers" FROM step_5 GROUP BY "country"
),
step_7 AS (
  SELECT * RENAME ("sum_net" AS "revenue") FROM step_6
),
step_8 AS (
  SELECT * FROM step_7 ORDER BY "revenue" DESC, "country"
),
step_9 AS (
  SELECT * FROM step_8 ORDER BY "revenue" DESC, "country" LIMIT 10
)
SELECT * FROM step_9 ORDER BY "revenue" DESC, "country"