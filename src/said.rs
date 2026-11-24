//! What the agent said ([`ideae_core::said`]), kept with a task for the
//! sidebar.

pub use ideae_core::said::*;

/// Kept with a task: whole for the tooltip, its start for the row.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub whole: gpui_kit::SharedString,
    pub lead: gpui_kit::SharedString,
}

/// The row shows three lines at most, of a narrow sidebar.
const LEAD_CHARS: usize = 180;

impl Said {
    pub fn new(text: &str) -> Option<Self> {
        let lead = lead(text, LEAD_CHARS);
        (!lead.is_empty()).then(|| Self { whole: cap(text, KEPT_CHARS).into(), lead: lead.into() })
    }
}
