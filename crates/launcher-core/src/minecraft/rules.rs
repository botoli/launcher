use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingSystem {
    Windows,
    Linux,
    MacOs,
}

impl OperatingSystem {
    pub const fn metadata_name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::MacOs => "osx",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleContext {
    pub operating_system: OperatingSystem,
    pub features: BTreeMap<String, bool>,
}

impl RuleContext {
    pub fn windows() -> Self {
        Self {
            operating_system: OperatingSystem::Windows,
            features: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Disallow,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct RuleOs {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub action: RuleAction,
    #[serde(default)]
    pub os: Option<RuleOs>,
    #[serde(default)]
    pub features: BTreeMap<String, bool>,
}

pub fn rules_allow(rules: &[Rule], context: &RuleContext) -> bool {
    if rules.is_empty() {
        return true;
    }

    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule, context) {
            allowed = rule.action == RuleAction::Allow;
        }
    }
    allowed
}

fn rule_matches(rule: &Rule, context: &RuleContext) -> bool {
    let os_matches = rule.os.as_ref().is_none_or(|os| {
        os.name
            .as_deref()
            .is_none_or(|name| name == context.operating_system.metadata_name())
            && os.arch.is_none()
            && os.version.is_none()
    });
    let features_match = rule.features.iter().all(|(feature, required)| {
        context.features.get(feature).copied().unwrap_or(false) == *required
    });
    os_matches && features_match
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(action: RuleAction, os_name: Option<&str>) -> Rule {
        Rule {
            action,
            os: os_name.map(|name| RuleOs {
                name: Some(name.to_owned()),
                ..RuleOs::default()
            }),
            features: BTreeMap::new(),
        }
    }

    #[test]
    fn allow_rule_matches_windows() {
        assert!(rules_allow(
            &[rule(RuleAction::Allow, Some("windows"))],
            &RuleContext::windows()
        ));
    }

    #[test]
    fn later_disallow_rule_rejects_windows() {
        assert!(!rules_allow(
            &[
                rule(RuleAction::Allow, None),
                rule(RuleAction::Disallow, Some("windows")),
            ],
            &RuleContext::windows()
        ));
    }

    #[test]
    fn no_rules_is_allowed() {
        assert!(rules_allow(&[], &RuleContext::windows()));
    }

    #[test]
    fn unsupported_true_feature_rule_does_not_match() {
        let mut feature_rule = rule(RuleAction::Allow, Some("windows"));
        feature_rule
            .features
            .insert("has_custom_resolution".to_owned(), true);

        assert!(!rules_allow(&[feature_rule], &RuleContext::windows()));
    }
}
