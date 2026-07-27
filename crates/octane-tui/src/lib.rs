//! The terminal client.
//!
//! Contains no agent logic. It subscribes to [`octane_protocol::Event`], renders,
//! and sends user input back. That boundary is what makes a second surface — an
//! RPC server, an editor extension — additive rather than a rewrite.
//!
//! # Full screen, not scrollback
//!
//! The most consequential decision here (`RESEARCH.md` §H). There are two ways to
//! build a terminal UI:
//!
//! **Scrollback** — append to the terminal like a normal CLI and move the cursor
//! back up only to redraw the live parts. Claude Code, Codex, Droid, and pi do
//! this, and it keeps the terminal's own scrollback, search, and copy/paste.
//!
//! **Full screen** — own the viewport as a grid of cells and draw everything.
//! opencode and Amp do this. It costs those three things, which must then be
//! reimplemented.
//!
//! This crate takes the second: [`app`] enters the alternate screen and owns the
//! viewport. That is a real cost — `cmd+F` and mouse selection stop meaning what
//! they did — and it is why [`transcript`] implements its own scrolling and keeps
//! a generous line budget.
//!
//! # Panes
//!
//! The screen is a vertical stack of panes, each implementing [`component::Pane`]:
//! it says how much room it wants and how to draw itself. Every pane lives in the
//! module owning its state — the composer's in [`composer`], the status line's in
//! [`status`] — so [`app`] holds no idea of what any of them look like. It
//! collects constraints, splits once, and renders each into what it got.
//!
//! Everything except [`app`] is pure: state in, cells out, no terminal and no
//! async. That is deliberate — rendering logic is where the bugs are, and a pane
//! is testable by drawing it into a [`Buffer`](ratatui::buffer::Buffer) and
//! asserting on the result.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod approval;
pub mod banner;
pub mod completion;
pub mod component;
pub mod composer;
pub mod glyphs;
pub mod highlight;
pub mod keymap;
pub mod markdown;
pub mod picker;
pub mod render;
pub mod status;
pub mod theme;
pub mod themes;
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
