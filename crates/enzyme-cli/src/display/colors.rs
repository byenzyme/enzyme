use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crossterm::style::Color;

use enzyme_core::models::EntityType;

// ---------------------------------------------------------------------------
// Brand colors
// ---------------------------------------------------------------------------

pub const ENZYME_PURPLE: Color = Color::Rgb {
    r: 147,
    g: 112,
    b: 219,
};
pub const FOREST_GREEN: Color = Color::Rgb {
    r: 34,
    g: 139,
    b: 34,
};
pub const CATALYZING_ORANGE: Color = Color::Rgb {
    r: 255,
    g: 165,
    b: 0,
};

// ---------------------------------------------------------------------------
// Pulse cycle colors (pink -> purple -> teal -> back)
// ---------------------------------------------------------------------------

pub const PULSE_CYCLE_COLORS: &[Color] = &[
    Color::Rgb {
        r: 255,
        g: 105,
        b: 180,
    }, // hot pink
    Color::Rgb {
        r: 255,
        g: 85,
        b: 170,
    },
    Color::Rgb {
        r: 238,
        g: 75,
        b: 180,
    },
    Color::Rgb {
        r: 220,
        g: 65,
        b: 190,
    },
    Color::Rgb {
        r: 200,
        g: 60,
        b: 200,
    },
    Color::Rgb {
        r: 180,
        g: 60,
        b: 210,
    },
    Color::Rgb {
        r: 160,
        g: 65,
        b: 220,
    },
    Color::Rgb {
        r: 140,
        g: 75,
        b: 225,
    },
    Color::Rgb {
        r: 120,
        g: 90,
        b: 220,
    },
    Color::Rgb {
        r: 100,
        g: 110,
        b: 210,
    },
    Color::Rgb {
        r: 80,
        g: 135,
        b: 200,
    }, // transition to teal
    Color::Rgb {
        r: 60,
        g: 160,
        b: 190,
    },
    Color::Rgb {
        r: 50,
        g: 180,
        b: 180,
    },
    Color::Rgb {
        r: 55,
        g: 190,
        b: 170,
    },
    Color::Rgb {
        r: 60,
        g: 195,
        b: 160,
    }, // teal peak
    Color::Rgb {
        r: 80,
        g: 185,
        b: 165,
    },
    Color::Rgb {
        r: 110,
        g: 170,
        b: 175,
    },
    Color::Rgb {
        r: 150,
        g: 150,
        b: 185,
    },
    Color::Rgb {
        r: 200,
        g: 125,
        b: 185,
    },
    Color::Rgb {
        r: 240,
        g: 110,
        b: 180,
    }, // back to pink
];

// ---------------------------------------------------------------------------
// Stable entity colors (hash-based)
// ---------------------------------------------------------------------------

pub const STABLE_COLORS: &[Color] = &[
    Color::Rgb {
        r: 147,
        g: 112,
        b: 219,
    }, // medium purple
    Color::Rgb {
        r: 100,
        g: 149,
        b: 237,
    }, // cornflower blue
    Color::Rgb {
        r: 72,
        g: 209,
        b: 204,
    }, // medium turquoise
    Color::Rgb {
        r: 144,
        g: 238,
        b: 144,
    }, // light green
    Color::Rgb {
        r: 255,
        g: 182,
        b: 193,
    }, // light pink
    Color::Rgb {
        r: 255,
        g: 218,
        b: 185,
    }, // peach puff
];

// ---------------------------------------------------------------------------
// Heatmap colors (indigo -> yellow, cool=old hot=new)
// ---------------------------------------------------------------------------

pub const HEATMAP_COLORS: &[Color] = &[
    Color::Rgb {
        r: 63,
        g: 0,
        b: 125,
    }, // deep indigo
    Color::Rgb {
        r: 75,
        g: 0,
        b: 145,
    },
    Color::Rgb {
        r: 107,
        g: 0,
        b: 168,
    },
    Color::Rgb {
        r: 148,
        g: 30,
        b: 165,
    },
    Color::Rgb {
        r: 185,
        g: 60,
        b: 140,
    },
    Color::Rgb {
        r: 215,
        g: 90,
        b: 110,
    },
    Color::Rgb {
        r: 240,
        g: 120,
        b: 75,
    },
    Color::Rgb {
        r: 252,
        g: 160,
        b: 40,
    },
    Color::Rgb {
        r: 253,
        g: 200,
        b: 20,
    },
    Color::Rgb {
        r: 252,
        g: 240,
        b: 10,
    }, // hot yellow
];

// ---------------------------------------------------------------------------
// Entity symbol
// ---------------------------------------------------------------------------

pub fn entity_symbol(entity_type: &EntityType) -> &'static str {
    match entity_type {
        EntityType::Tag => "#",
        EntityType::Link => "~",
        EntityType::Folder => "/",
        EntityType::AppendLog => "%",
    }
}

// ---------------------------------------------------------------------------
// Deterministic entity color
// ---------------------------------------------------------------------------

pub fn entity_color(name: &str) -> Color {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let idx = (hasher.finish() as usize) % STABLE_COLORS.len();
    STABLE_COLORS[idx]
}

// ---------------------------------------------------------------------------
// Pulse color (for actively-processing entities)
// ---------------------------------------------------------------------------

/// Get a pulsating color based on elapsed time.
///
/// For the first 5 seconds, cycles through PULSE_CYCLE_COLORS.
/// After that, anneals toward the entity's stable color.
pub fn pulse_color(elapsed_secs: f64, entity_name: &str) -> Color {
    if elapsed_secs > 5.0 {
        return entity_color(entity_name);
    }

    // Cycle through pulse colors (~4 colors per second)
    let cycle_pos = (elapsed_secs * 4.0) as usize;
    let idx = cycle_pos % PULSE_CYCLE_COLORS.len();
    PULSE_CYCLE_COLORS[idx]
}

/// Map a position in a rolling window [0, size) to a heatmap color.
/// Position 0 = oldest (cool), position size-1 = newest (hot).
pub fn heatmap_color(position: usize, total: usize) -> Color {
    if total <= 1 {
        return HEATMAP_COLORS[HEATMAP_COLORS.len() - 1];
    }
    let idx = (position * (HEATMAP_COLORS.len() - 1)) / (total - 1);
    HEATMAP_COLORS[idx.min(HEATMAP_COLORS.len() - 1)]
}
