use std::collections::VecDeque;

use serde_json::json;

use crate::domain::EffectExecutor;
use crate::{EffectIntent, EffectIntentKind, EffectOutcome};

#[derive(Debug, Default)]
pub(crate) struct RecordingHerdrProcessAdapter {
    recorded: Vec<EffectIntent>,
    outcomes: VecDeque<EffectOutcome>,
}

impl RecordingHerdrProcessAdapter {
    pub(crate) fn with_outcomes(outcomes: impl IntoIterator<Item = EffectOutcome>) -> Self {
        Self {
            recorded: Vec::new(),
            outcomes: outcomes.into_iter().collect(),
        }
    }

    pub(crate) fn recorded(&self) -> &[EffectIntent] {
        &self.recorded
    }
}

impl EffectExecutor for RecordingHerdrProcessAdapter {
    fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome {
        self.recorded.push(intent.clone());
        self.outcomes.pop_front().unwrap_or_else(recorded_success)
    }
}

#[derive(Debug, Default)]
pub(crate) struct RecordingAgentProviderAdapter {
    recorded: Vec<EffectIntent>,
    outcomes: VecDeque<EffectOutcome>,
}

impl RecordingAgentProviderAdapter {
    pub(crate) fn with_outcomes(outcomes: impl IntoIterator<Item = EffectOutcome>) -> Self {
        Self {
            recorded: Vec::new(),
            outcomes: outcomes.into_iter().collect(),
        }
    }

    pub(crate) fn recorded(&self) -> &[EffectIntent] {
        &self.recorded
    }
}

impl EffectExecutor for RecordingAgentProviderAdapter {
    fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome {
        self.recorded.push(intent.clone());
        self.outcomes.pop_front().unwrap_or_else(recorded_success)
    }
}

pub(crate) fn is_herdr_process_intent(intent: &EffectIntentKind) -> bool {
    matches!(
        intent,
        EffectIntentKind::EnsureRoleReady { .. }
            | EffectIntentKind::ObserveRole { .. }
            | EffectIntentKind::RefreshMissionMirror
    )
}

pub(crate) fn is_agent_provider_intent(intent: &EffectIntentKind) -> bool {
    matches!(
        intent,
        EffectIntentKind::DeliverPrompt { .. } | EffectIntentKind::RecordEvidence { .. }
    )
}

fn recorded_success() -> EffectOutcome {
    EffectOutcome::Succeeded {
        observation: json!({"adapter": "recording", "external_invocation": false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Generation, RoleAttachMode, RoleKind, RoleRef};

    fn intent(kind: EffectIntentKind) -> EffectIntent {
        EffectIntent {
            effect_id: "effect-recording".into(),
            generation: Generation::new("generation-recording").unwrap(),
            intent: kind,
        }
    }

    #[test]
    fn recording_herdr_adapter_captures_process_intents_without_invocation() {
        let mut adapter = RecordingHerdrProcessAdapter::with_outcomes([]);
        let effect = intent(EffectIntentKind::EnsureRoleReady {
            role: RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            attach_mode: RoleAttachMode::Managed,
        });
        let outcome = adapter.execute(&effect);
        assert_eq!(adapter.recorded(), &[effect]);
        assert!(matches!(outcome, EffectOutcome::Succeeded { .. }));
        assert!(is_herdr_process_intent(&adapter.recorded()[0].intent));
    }

    #[test]
    fn recording_agent_adapter_captures_provider_intents_without_invocation() {
        let mut adapter = RecordingAgentProviderAdapter::default();
        let effect = intent(EffectIntentKind::DeliverPrompt {
            role: RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            assignment_id: Some("asg-recording".into()),
            prompt: "record only".into(),
        });
        let outcome = adapter.execute(&effect);
        assert_eq!(adapter.recorded(), &[effect]);
        assert!(matches!(outcome, EffectOutcome::Succeeded { .. }));
        assert!(is_agent_provider_intent(&adapter.recorded()[0].intent));
    }

    #[test]
    fn recording_adapters_return_preloaded_typed_outcomes_in_order() {
        let pending = EffectOutcome::Pending {
            reason: "provider is starting".into(),
        };
        let mut adapter = RecordingAgentProviderAdapter::with_outcomes([pending.clone()]);
        let effect = intent(EffectIntentKind::RecordEvidence {
            kind: "fixture".into(),
            payload: json!({"value": 1}),
        });
        assert_eq!(adapter.execute(&effect), pending);
        assert_eq!(adapter.recorded(), &[effect]);
    }
}
