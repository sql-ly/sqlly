-- golden deploy;
EXEC sp_rename N'[dbo].[Product]', N'[dbo].[Products]';;
CREATE OR ALTER VIEW dbo.vOrders AS SELECT o.Id FROM dbo.Orders o;
DROP TABLE [dbo].[Legacy];