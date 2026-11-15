//! Named provider presets: the wire, the endpoint and the key variable.

pub mod anthropic;
pub mod chatgpt;
pub mod commandcode;
pub mod deepseek;
pub mod freeinference;
pub mod mistral;
pub mod mistral_vibe;
pub mod openai;
pub mod opencode_go;
pub mod openrouter;
pub mod radius;
pub mod synthetic;

use serde::{Deserialize, Serialize};

/// The wire protocol a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API.
    Anthropic,
    /// OpenAI-compatible Chat Completions.
    OpenAiChat,
    /// The OpenAI Responses API.
    OpenAiResponses,
    /// The pi-messages API, which carries one JSON context per request.
    PiMessages,
}

impl Wire {
    /// The canonical name, as written in a config file.
    pub const fn name(self) -> &'static str {
        match self {
            Wire::Anthropic => "anthropic",
            Wire::OpenAiChat => "openai-chat",
            Wire::OpenAiResponses => "openai-responses",
            Wire::PiMessages => "pi-messages",
        }
    }

    /// The wire with canonical name `name`.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Wire::Anthropic,
            Wire::OpenAiChat,
            Wire::OpenAiResponses,
            Wire::PiMessages,
        ]
        .into_iter()
        .find(|wire| wire.name() == name)
    }

    /// The environment variable holding the key when none is named.
    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Wire::Anthropic => "ANTHROPIC_API_KEY",
            Wire::OpenAiChat | Wire::OpenAiResponses => "OPENAI_API_KEY",
            Wire::PiMessages => "RADIUS_API_KEY",
        }
    }
}

/// The request field that caps reply tokens on a chat endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`.
    MaxTokens,
    /// `max_completion_tokens`.
    MaxCompletionTokens,
    /// No cap field at all, for an endpoint that refuses one.
    Omit,
}

/// The request field that carries the reasoning level on a chat endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningField {
    /// `"reasoning_effort":"<text>"`.
    #[serde(rename = "reasoning_effort")]
    Effort,
    /// `"reasoning":{"effort":"<text>"}` or `{"max_tokens":N}`.
    #[serde(rename = "reasoning")]
    EffortObject,
}

/// How an endpoint differs from the plain wire.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compat {
    /// The field that caps reply tokens.
    pub max_tokens_field: MaxTokensField,
    /// The field that carries the reasoning level.
    pub reasoning_field: ReasoningField,
    /// The word the endpoint takes for reasoning off, or `None` to leave the field out.
    pub reasoning_off: Option<&'static str>,
    /// The header that carries one id per conversation, for an endpoint that
    /// rejects a request without it.
    pub session_header: Option<&'static str>,
    /// Whether the request asks for a usage chunk with `stream_options`, which
    /// an endpoint that does not take the field must leave out.
    pub stream_usage: bool,
    /// Whether an assistant message sends its thinking back as
    /// `reasoning_content`, which an endpoint that refuses unknown fields
    /// must not get.
    pub replay_reasoning: bool,
}

impl Compat {
    /// The plain wire.
    pub const DEFAULT: Compat = Compat {
        max_tokens_field: MaxTokensField::MaxTokens,
        reasoning_field: ReasoningField::Effort,
        reasoning_off: None,
        session_header: None,
        stream_usage: true,
        replay_reasoning: true,
    };
}

impl Default for Compat {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Compat fields a config file sets. A field left out keeps the base value.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatOverride {
    /// The field that caps reply tokens.
    pub max_tokens_field: Option<MaxTokensField>,
    /// The field that carries the reasoning level.
    pub reasoning_field: Option<ReasoningField>,
    /// The word the endpoint takes for reasoning off; an empty word leaves the
    /// field out.
    pub reasoning_off: Option<String>,
}

impl CompatOverride {
    /// `base` with each given field replaced.
    pub fn apply(self, base: Compat) -> Compat {
        Compat {
            max_tokens_field: self.max_tokens_field.unwrap_or(base.max_tokens_field),
            reasoning_field: self.reasoning_field.unwrap_or(base.reasoning_field),
            reasoning_off: match self.reasoning_off {
                // Leaked so the word outlives the run and `Compat` stays `Copy`;
                // a config is read once per run.
                Some(word) if !word.is_empty() => Some(String::leak(word)),
                Some(_) => None,
                None => base.reasoning_off,
            },
            session_header: base.session_header,
            stream_usage: base.stream_usage,
            replay_reasoning: base.replay_reasoning,
        }
    }
}

/// How the models.dev catalogue fills a provider's model list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelsDev {
    /// The catalogue provider id, when it is not the preset name.
    pub id: Option<&'static str>,
    /// Whether a missing per-token price is filled, or only the limits. A plan
    /// with no per-token price fills the limits only.
    pub prices: bool,
}

impl ModelsDev {
    /// Prices come from the catalogue entry named as the provider.
    pub const DEFAULT: ModelsDev = ModelsDev {
        id: None,
        prices: true,
    };
}

/// The hours that multiply a price from the models.dev catalogue.
///
/// The catalogue's discount is a time of day, so a price it gave doubles inside
/// the peak windows. Chinese public holidays are off-peak and not modeled, so a
/// run on a holiday shows the peak price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Peak {
    /// The UTC hours `[start, end)` of the peak.
    pub windows: &'static [(u8, u8)],
    /// Whether the windows cover Monday to Friday only.
    pub weekdays_only: bool,
    /// The multiplier inside a window.
    pub factor: f64,
}

impl Peak {
    /// The weekday (0 is Sunday) and the hour of the UTC time `secs`. Day 0 of
    /// the Unix clock, 1970-01-01, was a Thursday.
    fn weekday_hour(secs: u64) -> (u64, u64) {
        let days = secs / 86_400;
        ((days + 4) % 7, (secs % 86_400) / 3_600)
    }

    /// The multiplier at the UTC time `secs`: `factor` inside a window, else
    /// 1.0.
    pub fn factor_at(self, secs: u64) -> f64 {
        let (weekday, hour) = Self::weekday_hour(secs);
        if self.weekdays_only && matches!(weekday, 0 | 6) {
            return 1.0;
        }
        let hour = hour as u8;
        if self
            .windows
            .iter()
            .any(|(start, end)| (*start..*end).contains(&hour))
        {
            self.factor
        } else {
            1.0
        }
    }
}

/// The model ids a route takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Models {
    /// Every model.
    Any,
    /// The ids that start with this text.
    Prefix(&'static str),
    /// These ids only.
    Exact(&'static [&'static str]),
}

impl Models {
    /// Whether the route takes the model `id`.
    pub fn matches(self, id: &str) -> bool {
        match self {
            Models::Any => true,
            Models::Prefix(prefix) => id.starts_with(prefix),
            Models::Exact(ids) => ids.contains(&id),
        }
    }
}

/// The header a route sends the key in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// The wire's own header: `x-api-key` on the Anthropic wire, else
    /// `authorization: Bearer`.
    Wire,
    /// `authorization: Bearer`, on any wire.
    Bearer,
}

/// One endpoint of a provider: the models it takes, its wire and its quirks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Route {
    /// The route name, as an error names it.
    pub name: &'static str,
    /// The models it takes.
    pub models: Models,
    /// The wire protocol.
    pub wire: Wire,
    /// The endpoint, with no trailing slash.
    pub base_url: &'static str,
    /// The environment variable that replaces `base_url` when set.
    pub base_url_env: Option<&'static str>,
    /// The header the key goes in.
    pub auth: Auth,
    /// How the endpoint differs from the plain wire.
    pub compat: Compat,
}

impl Route {
    /// A chat route for every model. Routes override what differs.
    pub const DEFAULT: Route = Route {
        name: "chat",
        models: Models::Any,
        wire: Wire::OpenAiChat,
        base_url: "",
        base_url_env: None,
        auth: Auth::Wire,
        compat: Compat::DEFAULT,
    };

    /// The endpoint: the `base_url_env` variable when set and not empty, else
    /// `base_url`.
    pub fn endpoint(&self) -> String {
        self.base_url_env
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.base_url.to_string())
    }
}

/// A known provider.
#[derive(Clone, Copy, Debug)]
pub struct Preset {
    /// The provider name.
    pub name: &'static str,
    /// The endpoints, in order: a run takes the first that takes its model and
    /// the wire the user named, if any. The first route for every model is the
    /// default, and the model list is read from it.
    pub routes: &'static [Route],
    /// The environment variable holding the key.
    pub api_key_env: &'static str,
    /// The model list path under the default route's endpoint.
    pub models_path: &'static str,
    /// How the models.dev catalogue fills its model list.
    pub models_dev: ModelsDev,
    /// The hours that multiply a models.dev price, if any.
    pub peak: Option<Peak>,
    /// Reads the provider's own model list in its own shape.
    pub read_list: crate::catalog::ReadList,
    /// Whether the endpoint lists its models without a key.
    pub public_list: bool,
}

impl Preset {
    /// A preset with no routes yet. Presets set the routes and override what
    /// differs.
    pub const DEFAULT: Preset = Preset {
        name: "",
        routes: &[],
        api_key_env: "",
        models_path: "/models",
        models_dev: ModelsDev::DEFAULT,
        peak: None,
        read_list: crate::catalog::list::read,
        public_list: false,
    };

    /// The route a run without a named wire takes for a model no other route
    /// claims: the first route for every model.
    pub fn default_route(&self) -> &'static Route {
        self.routes
            .iter()
            .find(|route| route.models == Models::Any)
            .expect("every preset has a route for every model")
    }

    /// The first route that takes `model` and, when one is named, `wire`.
    pub fn route(&self, model: &str, wire: Option<Wire>) -> Option<&'static Route> {
        self.routes
            .iter()
            .find(|route| route.models.matches(model) && wire.is_none_or(|wire| route.wire == wire))
    }

    /// The routes that take `model`, as `name (wire)`, for an error that
    /// lists the wires a user may name.
    pub fn route_names(&self, model: &str) -> String {
        let names: Vec<String> = self
            .routes
            .iter()
            .filter(|route| route.models.matches(model))
            .map(|route| format!("{} ({})", route.name, route.wire.name()))
            .collect();
        names.join(", ")
    }
}

/// Every preset.
pub const ALL: &[&Preset] = &[
    &anthropic::PRESET,
    &openai::PRESET,
    &chatgpt::PRESET,
    &openrouter::PRESET,
    &opencode_go::PRESET,
    &mistral::PRESET,
    &mistral_vibe::PRESET,
    &commandcode::PRESET,
    &deepseek::PRESET,
    &freeinference::PRESET,
    &radius::PRESET,
    &synthetic::PRESET,
];

/// The preset called `name`.
pub fn get(name: &str) -> Option<&'static Preset> {
    ALL.iter().copied().find(|preset| preset.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_rule_peak_covers_its_weekday_hours_only() {
        let peak = get("deepseek").unwrap().peak.expect("deepseek peaks");
        assert_eq!(peak.factor, 2.0);
        assert_eq!(peak.factor_at(439_200), 2.0, "02:00 UTC Tuesday");
        // 05:00 UTC is outside both windows; 02:00 on a Saturday is a weekend.
        assert_eq!(peak.factor_at(439_200 + 3 * 3_600), 1.0, "05:00 Tuesday");
        assert_eq!(peak.factor_at(439_200 + 4 * 86_400), 1.0, "02:00 Saturday");
        assert_eq!(peak.factor_at(86_400), 1.0, "Friday 00:00");
        let all_day = Peak {
            windows: &[(0, 24)],
            weekdays_only: false,
            factor: 1.5,
        };
        assert_eq!(all_day.factor_at(439_200 + 4 * 86_400), 1.5);
    }

    #[test]
    fn preset_names_are_unique_and_get_finds_each() {
        for (index, preset) in ALL.iter().enumerate() {
            assert!(!preset.name.is_empty());
            assert!(ALL[..index].iter().all(|seen| seen.name != preset.name));
            assert_eq!(get(preset.name).map(|found| found.name), Some(preset.name));
        }
        assert!(get("nope").is_none());
    }

    #[test]
    fn preset_read_list_reads_the_provider_shape() {
        use crate::catalog::list;
        let expected: [(&str, crate::catalog::ReadList); 10] = [
            ("anthropic", anthropic::read_list),
            ("openai", list::read),
            ("chatgpt", chatgpt::read_list),
            ("openrouter", list::read),
            ("opencode-go", list::read),
            ("commandcode", list::read),
            ("deepseek", deepseek::read_list),
            ("freeinference", freeinference::read_list),
            ("radius", radius::read_list),
            ("synthetic", synthetic::read_list),
        ];
        for (name, read) in expected {
            let preset = get(name).expect("a preset");
            assert!(
                std::ptr::fn_addr_eq(preset.read_list, read),
                "{name} reads another shape"
            );
        }
    }

    #[test]
    fn preset_public_list_is_only_radius() {
        for preset in ALL {
            assert_eq!(
                preset.public_list,
                preset.name == "radius",
                "{}",
                preset.name
            );
        }
    }

    #[test]
    fn each_preset_base_url_has_no_trailing_slash() {
        for preset in ALL {
            for route in preset.routes {
                assert!(route.base_url.starts_with("https://"), "{}", preset.name);
                assert!(!route.base_url.ends_with('/'), "{}", preset.name);
            }
            assert!(preset.models_path.starts_with('/'), "{}", preset.name);
        }
    }

    #[test]
    fn max_tokens_field_omit_is_named_omit() {
        assert_eq!(
            serde_json::from_str::<MaxTokensField>(r#""omit""#).unwrap(),
            MaxTokensField::Omit
        );
        assert_eq!(
            serde_json::to_string(&MaxTokensField::Omit).unwrap(),
            r#""omit""#
        );
    }

    #[test]
    fn preset_wires_and_keys_match_the_design() {
        let anthropic = get("anthropic").unwrap();
        assert_eq!(anthropic.default_route().wire, Wire::Anthropic);
        assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
        let openai = get("openai").unwrap();
        assert_eq!(openai.default_route().wire, Wire::OpenAiResponses);
        assert_eq!(
            openai.default_route().compat.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        );
        assert_eq!(
            get("openrouter").unwrap().default_route().compat,
            Compat {
                reasoning_field: ReasoningField::EffortObject,
                reasoning_off: Some("none"),
                ..Compat::DEFAULT
            }
        );
    }

    #[test]
    fn deepseek_preset_sends_none_for_off_on_the_chat_wire() {
        let deepseek = get("deepseek").expect("a deepseek preset");
        assert_eq!(deepseek.default_route().wire, Wire::OpenAiChat);
        assert_eq!(
            deepseek.default_route().base_url,
            "https://api.deepseek.com"
        );
        assert_eq!(deepseek.api_key_env, "DEEPSEEK_API_KEY");
        let head = |level| {
            let builder = crate::openai::chat::RequestBuilder::with_compat(
                "deepseek-flash",
                1024,
                None,
                Some(level),
                &[],
                deepseek.default_route().compat,
            );
            std::str::from_utf8(builder.head()).unwrap().to_string()
        };
        assert!(head("off").contains(r#""reasoning_effort":"none""#));
        assert!(head("max").contains(r#""reasoning_effort":"max""#));
        assert!(head("max").contains(r#""max_tokens":1024"#));
    }

    #[test]
    fn synthetic_preset_sends_none_for_off_and_has_a_messages_route() {
        let synthetic = get("synthetic").expect("a synthetic preset");
        assert_eq!(synthetic.api_key_env, "SYNTHETIC_API_KEY");
        let chat = synthetic.route("hf:zai-org/GLM-5.3", None).unwrap();
        assert_eq!(chat.wire, Wire::OpenAiChat);
        assert_eq!(chat.base_url, "https://api.synthetic.new/openai/v1");
        let builder = crate::openai::chat::RequestBuilder::with_compat(
            "hf:zai-org/GLM-5.3",
            1024,
            None,
            Some("off"),
            &[],
            chat.compat,
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""reasoning_effort":"none""#), "{head}");
        let messages = synthetic
            .route("hf:zai-org/GLM-5.3", Some(Wire::Anthropic))
            .unwrap();
        assert_eq!(messages.name, "messages");
        assert_eq!(messages.base_url, "https://api.synthetic.new/anthropic");
    }

    #[test]
    fn mistral_presets_use_the_chat_wire_and_send_no_stream_options() {
        for name in ["mistral", "mistral-vibe"] {
            let preset = get(name).expect("a mistral preset");
            let route = preset.default_route();
            assert_eq!(route.wire, Wire::OpenAiChat, "{name}");
            assert_eq!(route.base_url, "https://api.mistral.ai/v1", "{name}");
            assert_eq!(
                route.compat.reasoning_field,
                ReasoningField::Effort,
                "{name}"
            );
            assert_eq!(route.compat.reasoning_off, Some("none"), "{name}");
            assert!(!route.compat.stream_usage, "{name}");
            assert!(!route.compat.replay_reasoning, "{name}");
        }
        assert_eq!(
            get("mistral").expect("the preset").api_key_env,
            "MISTRAL_API_KEY"
        );
        // The Vibe key is the one `reagent login mistral-vibe` saves.
        assert_eq!(get("mistral-vibe").expect("the preset").api_key_env, "");
        for preset in ALL
            .iter()
            .filter(|preset| !preset.name.starts_with("mistral"))
        {
            assert!(
                preset.default_route().compat.stream_usage,
                "{} lost its usage chunk",
                preset.name
            );
            assert!(
                preset.default_route().compat.replay_reasoning,
                "{} lost its replayed thinking",
                preset.name
            );
        }
    }

    #[test]
    fn reasoning_text_field_names_round_trip() {
        assert_eq!(
            serde_json::from_str::<ReasoningField>(r#""reasoning""#).unwrap(),
            ReasoningField::EffortObject
        );
        assert_eq!(
            serde_json::from_str::<ReasoningField>(r#""reasoning_effort""#).unwrap(),
            ReasoningField::Effort
        );
        assert!(serde_json::from_str::<ReasoningField>(r#""nope""#).is_err());
        assert_eq!(Compat::DEFAULT.reasoning_field, ReasoningField::Effort);
    }

    #[test]
    fn preset_wire_names_round_trip() {
        for wire in [Wire::Anthropic, Wire::OpenAiChat, Wire::OpenAiResponses] {
            assert_eq!(Wire::parse(wire.name()), Some(wire));
        }
        assert_eq!(Wire::parse("fake"), None);
        assert_eq!(Wire::OpenAiChat.default_api_key_env(), "OPENAI_API_KEY");
    }

    #[test]
    fn compat_override_sets_only_given_fields() {
        let base = Compat {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            session_header: Some("x-session"),
            ..Compat::DEFAULT
        };
        let none: CompatOverride = serde_json::from_str("{}").unwrap();
        assert_eq!(none.apply(base), base);
        let set: CompatOverride =
            serde_json::from_str(r#"{"max_tokens_field":"max_tokens"}"#).unwrap();
        assert_eq!(
            set.apply(base),
            Compat {
                max_tokens_field: MaxTokensField::MaxTokens,
                ..base
            }
        );
    }

    #[test]
    fn a_compat_override_sets_the_reasoning_text_field() {
        let set: CompatOverride =
            serde_json::from_str(r#"{"reasoning_field":"reasoning"}"#).unwrap();
        assert_eq!(
            set.apply(Compat::DEFAULT).reasoning_field,
            ReasoningField::EffortObject
        );
    }

    #[test]
    fn gateway_opencode_go_preset_sets_the_session_header() {
        let opencode = get("opencode-go").unwrap();
        assert_eq!(
            opencode.default_route().compat.session_header,
            Some("x-opencode-session")
        );
        assert_eq!(opencode.default_route().wire, Wire::OpenAiChat);
        for preset in ALL.iter().filter(|preset| preset.name != "opencode-go") {
            assert_eq!(
                preset.default_route().compat.session_header,
                None,
                "{}",
                preset.name
            );
        }
    }

    #[test]
    fn reasoning_off_presets_name_their_own_off_word() {
        assert_eq!(
            get("commandcode")
                .unwrap()
                .default_route()
                .compat
                .reasoning_off,
            Some("off")
        );
        assert_eq!(
            get("openrouter")
                .unwrap()
                .default_route()
                .compat
                .reasoning_off,
            Some("none")
        );
        assert_eq!(
            get("openai").unwrap().default_route().compat.reasoning_off,
            Some("none")
        );
        assert_eq!(Compat::DEFAULT.reasoning_off, None);
    }

    #[test]
    fn reasoning_off_a_config_word_replaces_the_base_word() {
        let base = Compat {
            reasoning_off: Some("none"),
            ..Compat::DEFAULT
        };
        let set = CompatOverride {
            reasoning_off: Some("disabled".into()),
            ..CompatOverride::default()
        };
        assert_eq!(set.apply(Compat::DEFAULT).reasoning_off, Some("disabled"));
        let empty = CompatOverride {
            reasoning_off: Some(String::new()),
            ..CompatOverride::default()
        };
        assert_eq!(empty.apply(base).reasoning_off, None);
    }

    #[test]
    fn compat_override_refuses_an_unknown_flag() {
        assert!(serde_json::from_str::<CompatOverride>(r#"{"nope":true}"#).is_err());
        assert!(serde_json::from_str::<CompatOverride>(r#"{"max_tokens_field":"nope"}"#).is_err());
    }

    #[test]
    fn route_models_take_any_a_prefix_or_exact_ids() {
        assert!(Models::Any.matches(""));
        assert!(Models::Prefix("claude-").matches("claude-haiku-5-5"));
        assert!(!Models::Prefix("claude-").matches("deepseek/claude-x"));
        assert!(Models::Exact(&["a", "b"]).matches("b"));
        assert!(!Models::Exact(&["a", "b"]).matches("ab"));
    }

    #[test]
    fn route_every_preset_has_a_default_and_unique_route_names() {
        for preset in ALL {
            assert_eq!(
                preset.default_route().models,
                Models::Any,
                "{}",
                preset.name
            );
            for (index, route) in preset.routes.iter().enumerate() {
                assert!(
                    preset.routes[..index]
                        .iter()
                        .all(|seen| seen.name != route.name),
                    "{}: route {} twice",
                    preset.name,
                    route.name
                );
            }
        }
    }

    #[test]
    fn route_commandcode_sends_claude_to_messages_and_the_rest_to_chat() {
        let commandcode = get("commandcode").unwrap();
        let claude = commandcode.route("claude-haiku-5-5", None).unwrap();
        assert_eq!(claude.wire, Wire::Anthropic);
        assert_eq!(claude.base_url, "https://api.commandcode.ai/provider");
        let other = commandcode
            .route("deepseek/deepseek-v4.1-flash", None)
            .unwrap();
        assert_eq!(other.wire, Wire::OpenAiChat);
        assert_eq!(other.base_url, "https://api.commandcode.ai/provider/v1");
        assert_eq!(other.compat.reasoning_off, Some("off"));
        // A named chat wire takes the chat route even for a claude model.
        let named = commandcode
            .route("claude-haiku-5-5", Some(Wire::OpenAiChat))
            .unwrap();
        assert_eq!(named.base_url, "https://api.commandcode.ai/provider/v1");
    }

    #[test]
    fn route_a_named_wire_picks_the_messages_route_of_deepseek_and_openrouter() {
        let deepseek = get("deepseek").unwrap();
        assert_eq!(
            deepseek.route("deepseek-flash", None).unwrap().wire,
            Wire::OpenAiChat
        );
        let messages = deepseek
            .route("deepseek-flash", Some(Wire::Anthropic))
            .unwrap();
        assert_eq!(messages.base_url, "https://api.deepseek.com/anthropic");
        assert_eq!(messages.auth, Auth::Wire);

        let openrouter = get("openrouter").unwrap();
        assert_eq!(
            openrouter
                .route("anthropic/claude-sonnet-5", None)
                .unwrap()
                .wire,
            Wire::OpenAiChat
        );
        let messages = openrouter
            .route("anthropic/claude-sonnet-5", Some(Wire::Anthropic))
            .unwrap();
        assert_eq!(messages.base_url, "https://openrouter.ai/api");
        assert_eq!(messages.auth, Auth::Bearer);
        assert!(
            openrouter
                .route("openai/gpt-5", Some(Wire::Anthropic))
                .is_none()
        );
    }

    #[test]
    fn route_a_wire_no_route_takes_is_none() {
        let deepseek = get("deepseek").unwrap();
        assert!(
            deepseek
                .route("deepseek-flash", Some(Wire::OpenAiResponses))
                .is_none()
        );
        assert_eq!(
            deepseek.route_names("deepseek-flash"),
            "chat (openai-chat), messages (anthropic)"
        );
        // A route that does not take the model is left out.
        assert_eq!(
            get("openrouter").unwrap().route_names("openai/gpt-5"),
            "chat (openai-chat), responses (openai-responses)"
        );
    }
}
