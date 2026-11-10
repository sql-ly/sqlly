-- golden rollback;
-- MANUAL REVIEW: recreate dbo.Legacy from a backup; definition not captured;
CREATE OR ALTER VIEW dbo.vOrders AS SELECT o.Id, o.CustomerId FROM dbo.Orders o;
EXEC sp_rename N'[dbo].[Products]', N'[dbo].[Product]';