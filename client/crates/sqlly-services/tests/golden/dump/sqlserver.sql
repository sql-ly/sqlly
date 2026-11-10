-- SQLLY database dump
-- Version: <redacted>
-- Engine: SqlServer
-- Database: appdb
-- Generated: <redacted>
-- Content: structure and data
-- Layout: single file
-- Drop if exists: false
-- Include views: true
-- Include routines: true
-- Include triggers: true
-- Include sequences: true
-- Batch size: 500
SET NOCOUNT ON;
CREATE TABLE [dbo].[audit] (
    [id] int IDENTITY(1,1) NULL,
    [note] int NULL
);

-- Indexes
id
note

-- Foreign Keys
id
note

-- Triggers
GO
int
GO
int
GO
CREATE TABLE [dbo].[customers] (
    [id] int IDENTITY(1,1) NULL,
    [name] int NULL
);

-- Indexes
id
name

-- Foreign Keys
id
name

-- Triggers
GO
int
GO
int
GO
CREATE TABLE [dbo].[orders] (
    [id] int IDENTITY(1,1) NULL,
    [customer_id] int NULL
);

-- Indexes
id
customer_id

-- Foreign Keys
id
customer_id

-- Triggers
GO
int
GO
int
GO
SET IDENTITY_INSERT [dbo].[audit] ON;
INSERT INTO [dbo].[audit] ([id], [note]) VALUES (1, N'seed'), (2, N'audit');
SET IDENTITY_INSERT [dbo].[audit] OFF;
GO
SET IDENTITY_INSERT [dbo].[customers] ON;
INSERT INTO [dbo].[customers] ([id], [name]) VALUES (1, N'Ann'), (2, N'Bob');
SET IDENTITY_INSERT [dbo].[customers] OFF;
GO
SET IDENTITY_INSERT [dbo].[orders] ON;
INSERT INTO [dbo].[orders] ([id], [customer_id]) VALUES (10, 1), (11, 2);
SET IDENTITY_INSERT [dbo].[orders] OFF;
GO