//! Shared terminal UI pieces of claude-consult, used by the installer and the
//! management TUI.
//!
//! Everything that decides something is pure state with a `step`/`handle` method and a
//! `render` into a [`ratatui::buffer::Buffer`], so it is tested without a terminal:
//!
//! - [`picker`]: the panel picker ([`Picker`], [`PickerRow`], [`rows_from`]), a port of
//!   the PowerShell installer's `New-PickerRows` / `Step-Picker` / `Format-Picker`.
//! - [`keys`]: the [`Key`] every widget takes, and [`map_key`] / [`read_key`] from
//!   crossterm events (presses only; Ctrl without Alt is a shortcut, AltGr is typing).
//! - [`widgets`]: [`TextInput`] (with a masked mode for the API key), [`Confirm`],
//!   [`StepLog`] and the [`Wizard`] frame.
//! - [`theme`]: [`Theme`], the colours.
//! - [`terminal`]: [`Terminal`], a guard around `ratatui::try_init` / `restore`.
//! - [`plain`]: the non-TTY path: line prompts, hidden input, `==>`/`[ok]`/`[!!]`/`[xx]`
//!   output.
//!
//! crossterm is reached through `ratatui::crossterm`, never as a dependency of its own,
//! so there is exactly one event type in the build.

pub mod keys;
pub mod picker;
pub mod plain;
pub mod terminal;
pub mod theme;
pub mod widgets;

pub use keys::{Key, map_key, read_key};
pub use picker::{Picker, PickerRow, rows_from};
pub use terminal::Terminal;
pub use theme::Theme;
pub use widgets::{Confirm, ConfirmOutcome, InputOutcome, StepKind, StepLog, TextInput, Wizard};
