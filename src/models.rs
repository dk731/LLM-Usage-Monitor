use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Replaces the reset countdown in the widget text. Copilot uses it to show
    /// spend in dollars, where a countdown carries no useful information.
    pub countdown_override: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
}

/// A usage source the widget can display. Ordering here is the order the
/// columns appear in the widget and the tray, so keep new entries at the end
/// unless the layout is meant to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    ClaudeCode,
    Codex,
    Antigravity,
    Kimi,
    Copilot,
}

pub const PROVIDER_COUNT: usize = 5;

impl Provider {
    pub const ALL: [Provider; PROVIDER_COUNT] = [
        Provider::ClaudeCode,
        Provider::Codex,
        Provider::Antigravity,
        Provider::Kimi,
        Provider::Copilot,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::ClaudeCode => 0,
            Self::Codex => 1,
            Self::Antigravity => 2,
            Self::Kimi => 3,
            Self::Copilot => 4,
        }
    }

    /// Stable identifier used in diagnostic logs.
    pub fn log_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Kimi => "Kimi",
            Self::Copilot => "Copilot",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    providers: [Option<UsageData>; PROVIDER_COUNT],
}

impl AppUsageData {
    pub fn get(&self, provider: Provider) -> Option<&UsageData> {
        self.providers[provider.index()].as_ref()
    }

    pub fn set(&mut self, provider: Provider, data: UsageData) {
        self.providers[provider.index()] = Some(data);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.iter().all(Option::is_none)
    }
}
