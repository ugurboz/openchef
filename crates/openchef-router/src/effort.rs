//! Effort resolution (§7.4).

use openchef_config::{EffortProfile, PlanTier};
use openchef_core::{Effort as Level, Importance};
use openchef_quota::Health;

/// The effort levels, least first.
const LEVELS: [Level; 4] = [Level::Low, Level::Medium, Level::High, Level::Max];

/// §7.4's effort table: a fixed profile's effort for each importance.
const fn table(profile: Fixed, importance: Importance) -> Level {
    use Importance::{Critical, High, Low, Normal};
    match (profile, importance) {
        (Fixed::Minimal, Low | Normal | High | Critical)
        | (Fixed::Economy, Low | Normal)
        | (Fixed::Balanced, Low) => Level::Low,
        (Fixed::Economy, High | Critical) | (Fixed::Balanced, Normal) | (Fixed::Quality, Low) => {
            Level::Medium
        }
        (Fixed::Balanced, High | Critical) | (Fixed::Quality, Normal) => Level::High,
        (Fixed::Quality, High | Critical) => Level::Max,
    }
}

/// The profiles with a fixed table row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixed {
    Minimal,
    Economy,
    Balanced,
    Quality,
}

impl Fixed {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Economy => "economy",
            Self::Balanced => "balanced",
            Self::Quality => "quality",
        }
    }
}

/// What effort resolution looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffortInput {
    /// The step's importance.
    pub importance: Importance,
    /// The pool's profile.
    pub profile: EffortProfile,
    /// The pool's plan as recorded by `init`.
    pub plan: Option<PlanTier>,
    /// The pool's health, for auto's pressure rule.
    pub health: Health,
    /// The quota model projects that at least 40 % of the current window goes unused at
    /// its reset (auto's surplus rule).
    pub surplus: bool,
    /// A pinned effort (agent, task or `--effort`) bypasses profile and adaptation.
    pub pin: Option<Level>,
}

/// A resolved effort with the reason shown in plan approval and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effort {
    /// The level.
    pub level: Level,
    /// Why.
    pub reason: String,
}

/// Entry plans start auto at economy; larger plans at balanced (§7.4). Users on entry
/// plans are most users, so automatic effort is frugal there and never exceeds high.
fn entry_plan(plan: Option<PlanTier>) -> bool {
    match plan {
        Some(PlanTier::Free | PlanTier::Pro | PlanTier::Plus) => true,
        Some(
            PlanTier::Max | PlanTier::Ultra | PlanTier::Team | PlanTier::Enterprise | PlanTier::Api,
        )
        | None => false,
    }
}

fn index(level: Level) -> usize {
    LEVELS.iter().position(|l| *l == level).unwrap_or(0)
}

/// Applies §7.4's three steps: base from importance, profile, and (auto only) quota
/// adaptation, clamped to low…max; on entry plans automatic effort never exceeds high.
#[must_use]
pub fn resolve_effort(input: EffortInput) -> Effort {
    if let Some(level) = input.pin {
        return Effort {
            level,
            reason: "pinned".to_owned(),
        };
    }
    let imp = input.importance;
    let fixed = match input.profile {
        EffortProfile::Auto => None,
        EffortProfile::Minimal => Some(Fixed::Minimal),
        EffortProfile::Economy => Some(Fixed::Economy),
        EffortProfile::Balanced => Some(Fixed::Balanced),
        EffortProfile::Quality => Some(Fixed::Quality),
    };
    if let Some(profile) = fixed {
        return Effort {
            level: table(profile, imp),
            reason: format!("{imp} importance, {} profile", profile.as_str()),
        };
    }
    let entry = entry_plan(input.plan);
    let start = if entry {
        Fixed::Economy
    } else {
        Fixed::Balanced
    };
    let base = index(table(start, imp));
    let mut reason = format!("{imp} importance, auto ({} start", start.as_str());
    let idx = match input.health {
        Health::Yellow | Health::Red => {
            reason = format!("{reason}, -1 for {} quota", input.health.as_str());
            base.saturating_sub(1)
        }
        Health::Green if input.surplus => {
            reason.push_str(", +1 for unused quota");
            base + 1
        }
        Health::Green | Health::Exhausted | Health::Unknown => base,
    };
    reason.push(')');
    let mut idx = idx.min(LEVELS.len() - 1);
    if entry {
        idx = idx.min(index(Level::High));
    }
    Effort {
        level: LEVELS[idx],
        reason,
    }
}

#[cfg(test)]
mod tests {
    use openchef_config::{EffortProfile, PlanTier};
    use openchef_core::{Effort as Level, Importance};
    use openchef_quota::Health;

    use super::{EffortInput, resolve_effort};

    const IMPS: [Importance; 4] = [
        Importance::Low,
        Importance::Normal,
        Importance::High,
        Importance::Critical,
    ];

    fn input(importance: Importance, plan: PlanTier, health: Health) -> EffortInput {
        EffortInput {
            importance,
            profile: EffortProfile::Auto,
            plan: Some(plan),
            health,
            surplus: false,
            pin: None,
        }
    }

    #[test]
    fn effort_tables() {
        use Level::{High, Low, Max, Medium};
        for (profile, row) in [
            (EffortProfile::Minimal, [Low, Low, Low, Low]),
            (EffortProfile::Economy, [Low, Low, Medium, Medium]),
            (EffortProfile::Balanced, [Low, Medium, High, High]),
            (EffortProfile::Quality, [Medium, High, Max, Max]),
        ] {
            for (imp, want) in IMPS.into_iter().zip(row) {
                // A fixed profile ignores quota entirely.
                let got = resolve_effort(EffortInput {
                    importance: imp,
                    profile,
                    plan: None,
                    health: Health::Red,
                    surplus: true,
                    pin: None,
                });
                assert_eq!(got.level, want, "{profile}/{imp}");
            }
        }
    }

    #[test]
    fn auto_effort() {
        use Importance::{Critical, High, Low, Normal};
        let surplus = |mut i: EffortInput| {
            i.surplus = true;
            i
        };
        let cases = [
            (
                "pro starts economy",
                input(Normal, PlanTier::Pro, Health::Green),
                Level::Low,
            ),
            (
                "max starts balanced",
                input(Normal, PlanTier::Max, Health::Green),
                Level::Medium,
            ),
            (
                "pressure lowers",
                input(High, PlanTier::Max, Health::Yellow),
                Level::Medium,
            ),
            (
                "red lowers",
                input(High, PlanTier::Max, Health::Red),
                Level::Medium,
            ),
            (
                "surplus raises",
                surplus(input(High, PlanTier::Max, Health::Green)),
                Level::Max,
            ),
            (
                "surplus needs green",
                surplus(input(High, PlanTier::Max, Health::Yellow)),
                Level::Medium,
            ),
            (
                "clamped at low",
                input(Low, PlanTier::Pro, Health::Red),
                Level::Low,
            ),
            // Entry plans never exceed high automatically: critical on Plus with surplus.
            (
                "entry cap",
                surplus(input(Critical, PlanTier::Plus, Health::Green)),
                Level::High,
            ),
            (
                "entry critical",
                input(Critical, PlanTier::Pro, Health::Green),
                Level::Medium,
            ),
            (
                "pin wins",
                EffortInput {
                    pin: Some(Level::Max),
                    ..input(Low, PlanTier::Pro, Health::Green)
                },
                Level::Max,
            ),
            (
                "unknown health",
                input(Normal, PlanTier::Max, Health::Unknown),
                Level::Medium,
            ),
        ];
        for (name, i, want) in cases {
            let got = resolve_effort(i);
            assert_eq!(got.level, want, "{name}: {}", got.reason);
            assert!(!got.reason.is_empty(), "{name}");
        }
    }
}
