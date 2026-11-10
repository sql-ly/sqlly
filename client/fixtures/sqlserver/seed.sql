-- SQLly Seed Database
-- Creates test objects for development and integration testing

IF NOT EXISTS (SELECT name FROM sys.databases WHERE name = N'osxql_test')
BEGIN
    CREATE DATABASE [osxql_test];
END
GO

USE [osxql_test];
GO

-- Schemas
IF NOT EXISTS (SELECT name FROM sys.schemas WHERE name = N'sales')
    EXEC('CREATE SCHEMA [sales]');
GO

IF NOT EXISTS (SELECT name FROM sys.schemas WHERE name = N'hr')
    EXEC('CREATE SCHEMA [hr]');
GO

-- Tables
IF NOT EXISTS (SELECT name FROM sys.tables WHERE name = N'Customers' AND SCHEMA_NAME(schema_id) = N'sales')
CREATE TABLE [sales].[Customers] (
    [CustomerID] INT IDENTITY(1,1) NOT NULL,
    [CompanyName] NVARCHAR(100) NOT NULL,
    [ContactName] NVARCHAR(100) NULL,
    [Email] NVARCHAR(255) NULL,
    [Phone] NVARCHAR(50) NULL,
    [CreatedAt] DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME(),
    [IsActive] BIT NOT NULL DEFAULT 1,
    CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED ([CustomerID] ASC)
);
GO

IF NOT EXISTS (SELECT name FROM sys.tables WHERE name = N'Orders' AND SCHEMA_NAME(schema_id) = N'sales')
CREATE TABLE [sales].[Orders] (
    [OrderID] INT IDENTITY(1,1) NOT NULL,
    [CustomerID] INT NOT NULL,
    [OrderDate] DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME(),
    [TotalAmount] DECIMAL(18,2) NOT NULL DEFAULT 0,
    [Status] NVARCHAR(20) NOT NULL DEFAULT N'Pending',
    CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([OrderID] ASC),
    CONSTRAINT [FK_Orders_Customers] FOREIGN KEY ([CustomerID])
        REFERENCES [sales].[Customers] ([CustomerID])
        ON DELETE NO ACTION ON UPDATE NO ACTION
);
GO

IF NOT EXISTS (SELECT name FROM sys.tables WHERE name = N'OrderItems' AND SCHEMA_NAME(schema_id) = N'sales')
CREATE TABLE [sales].[OrderItems] (
    [OrderItemID] INT IDENTITY(1,1) NOT NULL,
    [OrderID] INT NOT NULL,
    [ProductName] NVARCHAR(200) NOT NULL,
    [Quantity] INT NOT NULL DEFAULT 1,
    [UnitPrice] DECIMAL(18,2) NOT NULL,
    CONSTRAINT [PK_OrderItems] PRIMARY KEY CLUSTERED ([OrderItemID] ASC),
    CONSTRAINT [FK_OrderItems_Orders] FOREIGN KEY ([OrderID])
        REFERENCES [sales].[Orders] ([OrderID])
        ON DELETE CASCADE ON UPDATE NO ACTION
);
GO

IF NOT EXISTS (SELECT name FROM sys.tables WHERE name = N'Employees' AND SCHEMA_NAME(schema_id) = N'hr')
CREATE TABLE [hr].[Employees] (
    [EmployeeID] INT IDENTITY(1,1) NOT NULL,
    [FirstName] NVARCHAR(50) NOT NULL,
    [LastName] NVARCHAR(50) NOT NULL,
    [Email] NVARCHAR(255) NULL,
    [HireDate] DATE NOT NULL,
    [Department] NVARCHAR(100) NULL,
    [Salary] DECIMAL(18,2) NULL,
    [ManagerID] INT NULL,
    CONSTRAINT [PK_Employees] PRIMARY KEY CLUSTERED ([EmployeeID] ASC),
    CONSTRAINT [FK_Employees_Manager] FOREIGN KEY ([ManagerID])
        REFERENCES [hr].[Employees] ([EmployeeID])
        ON DELETE NO ACTION ON UPDATE NO ACTION
);
GO

-- Indexes
IF NOT EXISTS (SELECT name FROM sys.indexes WHERE name = N'IX_Orders_CustomerID')
    CREATE NONCLUSTERED INDEX [IX_Orders_CustomerID] ON [sales].[Orders] ([CustomerID]);
GO

IF NOT EXISTS (SELECT name FROM sys.indexes WHERE name = N'IX_Orders_Date')
    CREATE NONCLUSTERED INDEX [IX_Orders_Date] ON [sales].[Orders] ([OrderDate] DESC);
GO

IF NOT EXISTS (SELECT name FROM sys.indexes WHERE name = N'IX_OrderItems_OrderID')
    CREATE NONCLUSTERED INDEX [IX_OrderItems_OrderID] ON [sales].[OrderItems] ([OrderID]);
GO

-- Views
IF EXISTS (SELECT name FROM sys.views WHERE name = N'CustomerOrderSummary' AND SCHEMA_NAME(schema_id) = N'sales')
    DROP VIEW [sales].[CustomerOrderSummary];
GO
CREATE VIEW [sales].[CustomerOrderSummary]
AS
SELECT
    c.CustomerID,
    c.CompanyName,
    COUNT(o.OrderID) AS OrderCount,
    ISNULL(SUM(o.TotalAmount), 0) AS TotalSpent,
    MAX(o.OrderDate) AS LastOrderDate
FROM sales.Customers c
LEFT JOIN sales.Orders o ON c.CustomerID = o.CustomerID
GROUP BY c.CustomerID, c.CompanyName;
GO

-- Stored Procedures
IF EXISTS (SELECT name FROM sys.procedures WHERE name = N'GetCustomerOrders' AND SCHEMA_NAME(schema_id) = N'sales')
    DROP PROCEDURE [sales].[GetCustomerOrders];
GO
CREATE PROCEDURE [sales].[GetCustomerOrders]
    @CustomerID INT = NULL,
    @Status NVARCHAR(20) = NULL
AS
BEGIN
    SET NOCOUNT ON;
    SELECT o.OrderID, o.OrderDate, o.TotalAmount, o.Status
    FROM sales.Orders o
    WHERE (@CustomerID IS NULL OR o.CustomerID = @CustomerID)
      AND (@Status IS NULL OR o.Status = @Status)
    ORDER BY o.OrderDate DESC;
END;
GO

-- Scalar Function
IF EXISTS (SELECT name FROM sys.objects WHERE name = N'CalculateOrderTotal' AND SCHEMA_NAME(schema_id) = N'sales' AND type = 'FN')
    DROP FUNCTION [sales].[CalculateOrderTotal];
GO
CREATE FUNCTION [sales].[CalculateOrderTotal](@OrderID INT)
RETURNS DECIMAL(18,2)
AS
BEGIN
    DECLARE @Total DECIMAL(18,2);
    SELECT @Total = SUM(Quantity * UnitPrice)
    FROM sales.OrderItems
    WHERE OrderID = @OrderID;
    RETURN ISNULL(@Total, 0);
END;
GO

-- Table-Valued Function
IF EXISTS (SELECT name FROM sys.objects WHERE name = N'GetEmployeesByDepartment' AND SCHEMA_NAME(schema_id) = N'hr' AND type = 'IF')
    DROP FUNCTION [hr].[GetEmployeesByDepartment];
GO
CREATE FUNCTION [hr].[GetEmployeesByDepartment](@Department NVARCHAR(100))
RETURNS TABLE
AS
RETURN
    SELECT EmployeeID, FirstName, LastName, Email, HireDate, Department, Salary
    FROM hr.Employees
    WHERE Department = @Department;
GO

-- Trigger
IF EXISTS (SELECT name FROM sys.triggers WHERE name = N'tr_Orders_StatusUpdate' AND parent_id = OBJECT_ID(N'sales.Orders'))
    DROP TRIGGER [sales].[tr_Orders_StatusUpdate];
GO
CREATE TRIGGER [sales].[tr_Orders_StatusUpdate]
ON [sales].[Orders]
AFTER UPDATE
AS
BEGIN
    SET NOCOUNT ON;
    IF UPDATE(Status)
    BEGIN
        PRINT N'Order status changed';
    END
END;
GO

-- Sample Data
IF NOT EXISTS (SELECT 1 FROM [sales].[Customers])
BEGIN
    INSERT INTO [sales].[Customers] (CompanyName, ContactName, Email, Phone) VALUES
    (N'Acme Corp', N'John Smith', N'john@acme.com', N'555-0101'),
    (N'Globex Inc', N'Jane Doe', N'jane@globex.com', N'555-0102'),
    (N'Initech', N'Peter Gibbons', N'peter@initech.com', N'555-0103'),
    (N'Umbrella Corp', N'Alice Wang', N'alice@umbrella.com', N'555-0104'),
    (N'Wayne Enterprises', N'Bruce Wayne', N'bruce@wayne.com', N'555-0105');
END
GO

IF NOT EXISTS (SELECT 1 FROM [hr].[Employees])
BEGIN
    INSERT INTO [hr].[Employees] (FirstName, LastName, Email, HireDate, Department, Salary) VALUES
    (N'John', N'Smith', N'john.smith@company.com', '2020-01-15', N'Engineering', 95000),
    (N'Jane', N'Doe', N'jane.doe@company.com', '2019-06-01', N'Engineering', 105000),
    (N'Bob', N'Johnson', N'bob.johnson@company.com', '2021-03-10', N'Sales', 78000),
    (N'Alice', N'Wang', N'alice.wang@company.com', '2018-11-20', N'Engineering', 115000),
    (N'Charlie', N'Brown', N'charlie.brown@company.com', '2022-07-01', N'HR', 72000);
END
GO

-- Database Role
IF NOT EXISTS (SELECT name FROM sys.database_principals WHERE name = N'osxql_reader' AND type = 'R')
    CREATE ROLE [osxql_reader];
GO

PRINT N'osxql Management Studio seed database created successfully.';
GO
