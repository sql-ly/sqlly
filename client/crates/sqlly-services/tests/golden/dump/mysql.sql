-- SQLLY database dump
-- Version: <redacted>
-- Engine: MySql
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
SET FOREIGN_KEY_CHECKS=0;
SET NAMES utf8mb4;
id;

id;

id;

INSERT INTO `dbo`.`audit` (`id`, `note`) VALUES (1, 'seed'), (2, 'audit');

INSERT INTO `dbo`.`customers` (`id`, `name`) VALUES (1, 'seed'), (2, 'audit');

INSERT INTO `dbo`.`orders` (`id`, `customer_id`) VALUES (1, 'seed'), (2, 'audit');

SET FOREIGN_KEY_CHECKS=1;