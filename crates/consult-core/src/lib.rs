//! Shared core of claude-consult: the reviewer panel and its sandbox, the OpenRouter
//! client, the catalog and live listing, the registry, the status records, and the
//! install file plan with its settings.json merge.
//!
//! Nothing here reads a key from, or writes to, a real location on its own: every path
//! and every URL is a parameter, so the tests run against temp dirs and a mock server.

pub mod catalog;
pub mod display;
pub mod error;
pub mod generate;
pub mod key;
pub mod listing;
pub mod models;
pub mod openrouter;
pub mod panel;
pub mod paths;
pub mod records;
pub mod sandbox;
pub mod settings;
pub mod util;

pub use error::{ConsultError, GenerateError};
