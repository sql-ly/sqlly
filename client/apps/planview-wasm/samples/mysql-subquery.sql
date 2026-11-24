SELECT name,
       (SELECT MAX(total) FROM orders o WHERE o.customer_id = c.id) AS biggest
FROM customers c
WHERE c.id IN (SELECT customer_id FROM orders WHERE total > 900)
ORDER BY name
LIMIT 20
