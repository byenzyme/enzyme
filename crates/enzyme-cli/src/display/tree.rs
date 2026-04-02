use std::io::Write;

use crossterm::style::{SetForegroundColor, ResetColor, Color};

use enzyme_core::models::EntityType;

use super::colors::{entity_symbol, heatmap_color};

// ---------------------------------------------------------------------------
// Tree connector constants
// ---------------------------------------------------------------------------

pub const BRANCH: &str = "├─";
pub const LAST_BRANCH: &str = "└─";
pub const PIPE: &str = "│ ";
pub const ARROW: &str = "→";
pub const DOT: &str = "·";

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Render a single entity node in the discovery tree.
///
/// Format: `  ├─ # entity_name → catalyst insight text`
/// When `failed = true`: entity name is red, error shown in dim red.
pub fn render_entity_node(
    writer: &mut impl Write,
    entity_name: &str,
    entity_type: &EntityType,
    catalyst_text: Option<&str>,
    position: usize,
    total: usize,
    is_last: bool,
    failed: bool,
) -> std::io::Result<()> {
    let connector = if is_last { LAST_BRANCH } else { BRANCH };
    let symbol = entity_symbol(entity_type);
    let color = if failed {
        Color::Red
    } else {
        heatmap_color(position, total)
    };

    write!(
        writer,
        "  {connector} {set_color}{symbol} {name}{reset}",
        set_color = SetColorDisplay(color),
        name = entity_name,
        reset = ResetColorDisplay,
    )?;

    if let Some(text) = catalyst_text {
        let truncated = if text.chars().count() > 60 {
            let end = text.char_indices().nth(57).map_or(text.len(), |(i, _)| i);
            format!("{}...", &text[..end])
        } else {
            text.to_string()
        };
        let text_color = if failed { Color::DarkRed } else { Color::DarkGrey };
        write!(
            writer,
            " {dim}{arrow} {text}{reset}",
            dim = SetColorDisplay(text_color),
            arrow = ARROW,
            text = truncated,
            reset = ResetColorDisplay,
        )?;
    }

    writeln!(writer)
}

/// Render the discovery section header.
pub fn render_discovery_header(
    writer: &mut impl Write,
    completed: usize,
    total: usize,
    spinner_frame: &str,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "  {set_color}{spinner}{reset} Discovering entities [{completed}/{total}]",
        set_color = SetColorDisplay(super::colors::ENZYME_PURPLE),
        spinner = spinner_frame,
        reset = ResetColorDisplay,
    )
}

// ---------------------------------------------------------------------------
// Display wrappers for crossterm types (avoid format! incompatibility)
// ---------------------------------------------------------------------------

struct SetColorDisplay(Color);

impl std::fmt::Display for SetColorDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", SetForegroundColor(self.0))
    }
}

struct ResetColorDisplay;

impl std::fmt::Display for ResetColorDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", ResetColor)
    }
}
