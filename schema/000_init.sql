-- Ocean-OS — initialization
-- Creates per-source schemas and required extensions.

create extension if not exists pgcrypto;
create extension if not exists vector;

create schema if not exists github;
create schema if not exists slack;
create schema if not exists campaigns;
create schema if not exists content;
create schema if not exists deploys;
create schema if not exists notion;
create schema if not exists telegram;

-- Cross-source knowledge layer
create schema if not exists knowledge;
