WITH step_1 AS (
  SELECT * FROM "orders" AS l INNER JOIN "customers" AS r USING ("customer_id")
),
step_2 AS (
  SELECT "country", "year", "amount" FROM step_1
),
step_3 AS (
  PIVOT step_2 ON "year" USING SUM("amount") GROUP BY "country"
),
step_4 AS (
  UNPIVOT step_3 ON "2025", "2026" INTO NAME "year" VALUE "total"
)
SELECT * FROM step_4