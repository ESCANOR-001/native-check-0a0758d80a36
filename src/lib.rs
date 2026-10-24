//! Native Connect migration, opt-in and isolated from the production runtime.
//! Only advertise a capability when its implementation and parity checks exist.
mod activity;
pub mod agent;
mod agent_setup;
pub mod auth;
pub mod auth_service;
pub mod chat;
mod chat_projection;
mod chat_store;
mod claude_background;
pub mod claude_cursor;
mod claude_fork_resume;
pub mod claude_history;
pub mod claude_host;
pub mod claude_link;
pub mod claude_observer;
pub mod claude_outbox;
mod claude_permissions;
mod claude_rebind;
mod claude_reconnect;
pub mod claude_remote;
pub mod claude_turns;
pub mod dashboard;
pub mod diagnostics;
mod handoff_codex;
pub mod launch_config;
pub mod legacy_activation;
mod legacy_claude;
mod legacy_claude_assets;
mod legacy_claude_bindings;
mod legacy_claude_native;
mod legacy_claude_reconcile;
mod legacy_claude_resume;
mod legacy_codex;
mod legacy_codex_headers;
mod legacy_codex_history;
mod legacy_codex_native;
pub mod legacy_connection;
pub mod legacy_export;
mod legacy_export_assets;
mod legacy_preferences;
pub mod legacy_rollback;
pub mod legacy_stage;
pub mod legacy_upgrade;
mod legacy_upload_display;
mod legacy_upload_native;
mod legacy_uploads;
mod migration_assets;
pub mod migration_snapshot;
pub mod profile_handoff;
pub mod restart_drain;
pub mod server;
pub mod service_cutover;
pub mod service_registration;
pub mod store;

pub mod attachments;
mod tool_media;
mod turn_timing;

mod models;
mod projects;
mod workspace;

mod branches;
mod capabilities;
mod checkpoints;
mod controls;
mod discovery;
mod editing;
mod elicitation;
mod native_control;
mod private_directory;
mod process_tree;
mod questions;
mod retention;
mod usage;
mod vcs;
mod worktrees;

mod cloudflared;
pub mod invitation_file;
pub mod mobile_connection;
pub mod network_access;
pub mod profile_lock;
mod profile_snapshot;
mod repository_clone;
mod runtime_metrics;
pub mod storage;
mod storage_disk;
mod storage_legacy;
mod storage_policy;
mod storage_policy_job;
mod storage_preview;
mod storage_report;
mod storage_trash;
pub mod tunnel;

mod subprocess;

mod codex_assets;
mod shared_history;

pub mod supervisor;

pub mod storage_stage;

pub mod storage_rows;

pub mod storage_cleanup;

#[cfg(test)]
mod storage_native_schema_tests;

mod source_hold;
