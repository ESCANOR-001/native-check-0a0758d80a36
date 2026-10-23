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
pub mod claude_history;
pub mod claude_host;
pub mod claude_link;
pub mod claude_observer;
pub mod claude_outbox;
mod claude_reconnect;
pub mod claude_remote;
pub mod claude_turns;
pub mod dashboard;
pub mod server;
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

pub mod invitation_file;
pub mod profile_lock;
mod profile_snapshot;
mod repository_clone;
mod runtime_metrics;
pub mod storage;
mod storage_disk;
mod storage_preview;
mod storage_report;
mod storage_trash;

mod subprocess;

mod codex_assets;
mod shared_history;
