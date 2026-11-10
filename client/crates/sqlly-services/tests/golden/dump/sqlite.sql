-- SQLLY database dump
-- Version: <redacted>
-- Engine: Sqlite
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
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
id;

-- Indexes
id
note

id;

-- Indexes
id
name

id;

-- Indexes
id
customer_id

INSERT INTO "dbo"."audit" ("id", "note") VALUES (1, 'seed'), (2, 'audit');

INSERT INTO "dbo"."customers" ("id", "name") VALUES (1, 'Ann'), (2, 'Bob');

INSERT INTO "dbo"."orders" ("id", "customer_id") VALUES (10, 1), (11, 2);

COMMIT;