SELECT c.email, o.id, o.placed_at, o.status, o.total
FROM customers c
JOIN orders o ON o.customer_id = c.id
WHERE lower(c.email) = 'user4242@example.com'
ORDER BY o.placed_at DESC
