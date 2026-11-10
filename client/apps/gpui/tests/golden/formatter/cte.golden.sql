WITH
    recent AS (SELECT TOP 10 id FROM dbo.Orders ORDER BY created_at DESC)
SELECT
    *
FROM
    recent