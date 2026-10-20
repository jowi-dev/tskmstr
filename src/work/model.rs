//! Which model a launch passes to its agent (GitHub issue #143).
//!
//! Every launch on runner R resolves its model through [`resolve_model`],
//! which walks one chain of candidates, most specific first, and keeps the
//! first one R's [`AgentRunner::validate_lane_model`] accepts:
//!
//! 1. the launch's own candidates, in the order the caller gives them: the
//!    `tm work run --model` flag (lane runs only), then the surface's own
//!    key (lane `model`, `[work.merge] model`, `[work.audit] model`, ...);
//! 2. `[work] default_model`;
//! 3. `[agent.<R>] default_model`;
//! 4. R's built-in default ([`AgentKind::builtin_models`]).
//!
//! A candidate R rejects (a claude bare name on opencode, or the reverse)
//! is skipped, not passed, and is reported in
//! [`ResolvedModel::rejected`] so `tm check` can flag it. The result is
//! always a model, so no launch falls through to the agent CLI's own
//! default.

use crate::agent::AgentRunner;
use crate::config::{AgentKind, Config, ModelsConfig};

/// The config-wide end of the chain: `[work] default_model` and the
/// `[agent.<runner>]` models. Every launch surface carries one of these and
/// adds its own candidates in front.
#[derive(Debug, Clone, Copy)]
pub struct ModelDefaults<'a> {
    /// `[work] default_model`.
    pub work_default: Option<&'a str>,
    /// The `[agent.<runner>]` models.
    pub models: &'a ModelsConfig,
}

/// No configured models, for a launch with no config loaded: the chain
/// ends straight in the built-in default.
static NO_MODELS: ModelsConfig = ModelsConfig::UNSET;

impl<'a> ModelDefaults<'a> {
    /// The defaults `config` sets.
    pub fn from_config(config: &'a Config) -> Self {
        ModelDefaults {
            work_default: config.work.default_model.as_deref(),
            models: &config.models,
        }
    }

    /// No configured defaults: the chain ends straight in the built-in
    /// default.
    pub fn none() -> ModelDefaults<'static> {
        ModelDefaults {
            work_default: None,
            models: &NO_MODELS,
        }
    }

    /// [`ModelDefaults::from_config`], or [`ModelDefaults::none`] with no
    /// config loaded.
    pub fn from_optional(config: Option<&'a Config>) -> Self {
        match config {
            Some(config) => ModelDefaults::from_config(config),
            None => ModelDefaults::none(),
        }
    }

    /// [`resolve_model`] with these defaults.
    pub fn resolve(
        &self,
        runner: &dyn AgentRunner,
        specific: &[ModelCandidate<'_>],
    ) -> ResolvedModel {
        resolve_model(runner, *self, specific)
    }
}

/// One configured model a launch may use, and where it came from.
#[derive(Debug, Clone, Copy)]
pub struct ModelCandidate<'a> {
    /// The config key or flag the value came from, e.g. `"lane model"`.
    pub source: &'a str,
    /// The configured value, or `None` when the key is unset.
    pub model: Option<&'a str>,
}

/// A configured model the runner rejected, so the chain skipped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedModel {
    /// See [`ModelCandidate::source`].
    pub source: String,
    /// The rejected value.
    pub model: String,
    /// The runner's reason, from [`AgentRunner::validate_lane_model`].
    pub reason: String,
}

/// The model a launch passes, and how the chain got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The model to pass as `--model`.
    pub model: String,
    /// Where [`ResolvedModel::model`] came from: a
    /// [`ModelCandidate::source`], or `"built-in default"`.
    pub source: String,
    /// Configured candidates the runner rejected, most specific first.
    pub rejected: Vec<RejectedModel>,
}

/// [`ResolvedModel::source`] when no configured candidate was usable.
pub const BUILTIN_SOURCE: &str = "built-in default";

/// The `[agent.<runner>] default_model` source label for `kind`.
pub fn runner_default_source(kind: AgentKind) -> String {
    format!("[agent.{}] default_model", kind.as_str())
}

/// The [`AgentKind`] whose adapter `runner` is.
///
/// # Panics
///
/// When `runner`'s name is not an [`AgentKind`] name. Every runner comes
/// from `crate::agent::routing::runner_for`, so this never happens.
fn kind_of(runner: &dyn AgentRunner) -> AgentKind {
    AgentKind::parse(runner.name()).expect("every agent runner's name is an AgentKind name")
}

/// `runner`'s built-in default model ([`AgentKind::builtin_models`]).
pub fn builtin_default_model(runner: &dyn AgentRunner) -> &'static str {
    kind_of(runner).builtin_models().default_model
}

/// Resolves the model a launch on `runner` passes. See the module docs for
/// the order. `specific` holds the launch's own candidates (the `--model`
/// flag, then the surface's key), most specific first.
pub fn resolve_model(
    runner: &dyn AgentRunner,
    defaults: ModelDefaults<'_>,
    specific: &[ModelCandidate<'_>],
) -> ResolvedModel {
    let ModelDefaults {
        work_default,
        models,
    } = defaults;
    let kind = kind_of(runner);
    let runner_source = runner_default_source(kind);
    let configured = specific.iter().copied().chain([
        ModelCandidate {
            source: "[work] default_model",
            model: work_default,
        },
        ModelCandidate {
            source: &runner_source,
            model: models.for_kind(kind).default_model.as_deref(),
        },
    ]);
    let mut rejected = Vec::new();
    for candidate in configured {
        let Some(model) = candidate.model.map(str::trim).filter(|m| !m.is_empty()) else {
            continue;
        };
        match runner.validate_lane_model(model) {
            Ok(()) => {
                return ResolvedModel {
                    model: model.to_string(),
                    source: candidate.source.to_string(),
                    rejected,
                };
            }
            Err(reason) => rejected.push(RejectedModel {
                source: candidate.source.to_string(),
                model: model.to_string(),
                reason,
            }),
        }
    }
    ResolvedModel {
        model: kind.builtin_models().default_model.to_string(),
        source: BUILTIN_SOURCE.to_string(),
        rejected,
    }
}

/// The model `runner`'s usage probe runs: `[agent.<runner>]
/// usage_probe_model` when the runner accepts it, else the built-in probe
/// model. `None` for a runner with no probe.
pub fn resolve_usage_probe_model(
    runner: &dyn AgentRunner,
    models: &ModelsConfig,
) -> Option<String> {
    let kind = kind_of(runner);
    let builtin = kind.builtin_models().usage_probe_model?;
    let configured = models
        .for_kind(kind)
        .usage_probe_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty() && runner.validate_lane_model(m).is_ok());
    Some(configured.unwrap_or(builtin).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::agent::opencode::OpencodeRunner;
    use crate::config::RunnerModels;

    fn md<'a>(models: &'a ModelsConfig, work_default: Option<&'static str>) -> ModelDefaults<'a> {
        ModelDefaults {
            work_default,
            models,
        }
    }

    fn lane(model: Option<&str>) -> Vec<ModelCandidate<'_>> {
        vec![ModelCandidate {
            source: "lane model",
            model,
        }]
    }

    fn opencode_default(model: &str) -> ModelsConfig {
        ModelsConfig {
            opencode: RunnerModels {
                default_model: Some(model.to_string()),
                usage_probe_model: None,
            },
            ..ModelsConfig::default()
        }
    }

    #[test]
    fn nothing_configured_resolves_to_each_runners_builtin_default() {
        let models = ModelsConfig::default();

        let claude = resolve_model(&ClaudeRunner, md(&models, None), &[]);
        assert_eq!(claude.model, "opus");
        assert_eq!(claude.source, BUILTIN_SOURCE);

        let opencode = resolve_model(&OpencodeRunner, md(&models, None), &[]);
        assert_eq!(opencode.model, "venice/z-ai-glm-5-3-flash");
    }

    #[test]
    fn the_most_specific_candidate_wins() {
        let models = ModelsConfig {
            claude: RunnerModels {
                default_model: Some("haiku".to_string()),
                usage_probe_model: None,
            },
            ..ModelsConfig::default()
        };
        let specific = [
            ModelCandidate {
                source: "--model",
                model: Some("fable"),
            },
            ModelCandidate {
                source: "lane model",
                model: Some("sonnet"),
            },
        ];

        let resolved = resolve_model(&ClaudeRunner, md(&models, Some("opus")), &specific);
        assert_eq!(resolved.model, "fable");
        assert_eq!(resolved.source, "--model");

        let resolved = resolve_model(&ClaudeRunner, md(&models, Some("opus")), &specific[1..]);
        assert_eq!(resolved.model, "sonnet");

        let resolved = resolve_model(&ClaudeRunner, md(&models, Some("opus")), &[]);
        assert_eq!(
            (resolved.model.as_str(), resolved.source.as_str()),
            ("opus", "[work] default_model")
        );

        let resolved = resolve_model(&ClaudeRunner, md(&models, None), &[]);
        assert_eq!(
            (resolved.model.as_str(), resolved.source.as_str()),
            ("haiku", "[agent.claude] default_model")
        );
    }

    #[test]
    fn a_candidate_the_runner_rejects_is_skipped_and_reported() {
        let models = opencode_default("venice/glm");

        let resolved = resolve_model(
            &OpencodeRunner,
            md(&models, Some("opus")),
            &lane(Some("sonnet")),
        );

        assert_eq!(resolved.model, "venice/glm");
        assert_eq!(resolved.source, "[agent.opencode] default_model");
        let skipped: Vec<_> = resolved
            .rejected
            .iter()
            .map(|r| (r.source.as_str(), r.model.as_str()))
            .collect();
        assert_eq!(
            skipped,
            [("lane model", "sonnet"), ("[work] default_model", "opus")]
        );
    }

    #[test]
    fn a_model_valid_on_the_runner_is_kept() {
        let resolved = resolve_model(
            &OpencodeRunner,
            md(&opencode_default("venice/glm"), None),
            &lane(Some("anthropic/claude-opus")),
        );
        assert_eq!(resolved.model, "anthropic/claude-opus");
        assert!(resolved.rejected.is_empty());
    }

    #[test]
    fn a_blank_candidate_counts_as_unset() {
        let resolved = resolve_model(
            &ClaudeRunner,
            md(&ModelsConfig::default(), Some("  ")),
            &lane(Some("")),
        );
        assert_eq!(resolved.model, "opus");
        assert!(resolved.rejected.is_empty());
    }

    #[test]
    fn the_usage_probe_uses_the_configured_model_or_the_builtin_one() {
        assert_eq!(
            resolve_usage_probe_model(&ClaudeRunner, &ModelsConfig::default()).as_deref(),
            Some("haiku")
        );

        let models = ModelsConfig {
            claude: RunnerModels {
                default_model: None,
                usage_probe_model: Some("sonnet".to_string()),
            },
            ..ModelsConfig::default()
        };
        assert_eq!(
            resolve_usage_probe_model(&ClaudeRunner, &models).as_deref(),
            Some("sonnet")
        );

        assert_eq!(
            resolve_usage_probe_model(&OpencodeRunner, &ModelsConfig::default()),
            None
        );
    }

    #[test]
    fn every_builtin_default_is_valid_for_its_own_runner() {
        for name in AgentKind::names() {
            let kind = AgentKind::parse(name).unwrap();
            let runner = crate::agent::routing::runner_for(kind);
            let builtin = kind.builtin_models();
            assert!(
                runner.validate_lane_model(builtin.default_model).is_ok(),
                "{name}"
            );
            if let Some(probe) = builtin.usage_probe_model {
                assert!(runner.validate_lane_model(probe).is_ok(), "{name} probe");
            }
        }
    }
}
