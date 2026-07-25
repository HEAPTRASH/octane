//! Colors.
//!
//! Deliberately small and drawn from the terminal's own 16-color palette rather
//! than fixed RGB. A hardcoded palette looks wrong on half of users' themes, and
//! a coding agent is not the place to fight someone's carefully chosen colors.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub user: Color,
    pub assistant: Color,
    pub reasoning: Color,
    pub tool: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    /// Secondary text: hints, counts, timings.
    pub dim: Color,
    pub added: Color,
    pub removed: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user: Color::Cyan,
            assistant: Color::Reset,
            reasoning: Color::DarkGray,
            tool: Color::Blue,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            dim: Color::DarkGray,
            added: Color::Green,
            removed: Color::Red,
        }
    }
}

impl Theme {
    pub fn dim(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn label(&self, color: Color) -> Style {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }

    /// Reasoning is shown italic *and* dim so it is unmistakably the model
    /// thinking rather than something it is telling the user.
    pub fn reasoning(&self) -> Style {
        Style::default().fg(self.reasoning).add_modifier(Modifier::ITALIC)
    }
}
