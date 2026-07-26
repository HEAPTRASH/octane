//! The terminal client.
//!
//! Contains no agent logic. It subscribes to [`octane_protocol::Event`], renders,
//! and sends user input back. That boundary is what makes a second surface — an
//! RPC server, an editor extension — additive rather than a rewrite.
//!
//! # Scrollback, not full screen
//!
//! The most consequential decision here (`RESEARCH.md` §H). There are two ways to
//! build a terminal UI:
//!
//! **Full screen** — own the viewport as a grid of cells and draw everything.
//! opencode and Amp do this. It costs the scrollback buffer, terminal search, and
//! sane copy/paste, all of which must then be reimplemented, and mouse scrolling
//! never feels quite right afterwards.
//!
//! **Scrollback** — append to the terminal like a normal CLI, and move the cursor
//! back up only to redraw the live parts: the composer and the spinner. Claude
//! Code, Codex, Droid, and pi do this.
//!
//! A coding agent is a linear chat — prompt, reply, tool call, result — which maps
//! onto the terminal's native model exactly. It gains nothing from owning the
//! viewport and loses the three things users rely on most. So: finished content is
//! pushed into real scrollback with `Terminal::insert_before`, and only a small
//! [`Viewport::Inline`](ratatui::Viewport::Inline) region is redrawn.
//!
//! # Module split
//!
//! Everything except [`app`] is pure: it turns state into lines and has no
//! terminal, no I/O, and no async. That is deliberate — rendering logic is where
//! the bugs are, and it should be testable by calling a function.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod approval;
pub mod banner;
pub mod completion;
pub mod composer;
pub mod glyphs;
pub mod highlight;
pub mod keymap;
pub mod markdown;
pub mod picker;
pub mod render;
pub mod status;
pub mod theme;
pub mod transcript;
pub mod wrap;

pub use app::{App, AppEvent};
pub use approval::{ApprovalPrompt, ApprovalReply, TuiApprover};
pub use completion::{Candidate, Completion};
pub use composer::{Composer, Submission};
pub use keymap::{KeyAction, KeyContext};
pub use picker::{Picker, PickerItem, PickerKind};
pub use glyphs::Glyphs;
pub use status::{Activity, StatusLine};
pub use theme::{ColorDepth, Theme};
pub use transcript::Transcript;
