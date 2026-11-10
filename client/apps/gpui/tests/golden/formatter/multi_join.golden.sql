SELECT
    o.id,
    c.name
FROM
    dbo.Orders o
    INNER JOIN dbo.Customers c ON c.id = o.customer_id
    LEFT JOIN dbo.Products p ON p.id = o.product_id
WHERE
    o.status = 'open'