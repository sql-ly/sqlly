-- SQLLY database dump
-- Version: <redacted>
-- Engine: Postgres
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
SET client_encoding = 'UTF8';
SET session_replication_role = replica;
id;

id;

id;

INSERT INTO "dbo"."audit" ("id", "note") VALUES (1, 'seed'), (2, 'audit');
SELECT setval(pg_get_serial_sequence('"dbo"."audit"', 'id'), COALESCE(MAX("id"), 1)) FROM "dbo"."audit";

INSERT INTO "dbo"."customers" ("id", "name") VALUES (1, 'Ann'), (2, 'Bob');
SELECT setval(pg_get_serial_sequence('"dbo"."customers"', 'id'), COALESCE(MAX("id"), 1)) FROM "dbo"."customers";

INSERT INTO "dbo"."orders" ("id", "customer_id") VALUES (10, 1), (11, 2);
SELECT setval(pg_get_serial_sequence('"dbo"."orders"', 'id'), COALESCE(MAX("id"), 1)) FROM "dbo"."orders";

SET session_replication_role = DEFAULT;