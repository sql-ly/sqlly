SELECT c.country,
       date_trunc('month', o.placed_at) AS month,
       count(DISTINCT o.id) AS orders,
       sum(oi.quantity * oi.unit_price) AS revenue
FROM orders o
JOIN customers c ON c.id = o.customer_id
JOIN order_items oi ON oi.order_id = o.id
JOIN products p ON p.id = oi.product_id
WHERE o.status <> 'new'
  AND p.category = 'games'
  AND lower(c.email) LIKE '%@example.com'
GROUP BY c.country, date_trunc('month', o.placed_at)
ORDER BY revenue DESC
LIMIT 20
