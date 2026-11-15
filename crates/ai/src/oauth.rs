//! The OAuth logins of the providers: the ChatGPT, OpenRouter and Mistral Vibe
//! flows and the saved session, over the shared pieces of `reagent-oauth`.

pub mod chatgpt;
pub mod mistral_vibe;
pub mod openrouter;
pub mod session;

pub use reagent_oauth::device;
pub use reagent_oauth::jwt;
pub use reagent_oauth::percent_encode;
pub use reagent_oauth::{
    Pkce, callback_code, callback_query, challenge, host_id, pkce, random_token,
};

#[cfg(any(test, feature = "testing"))]
pub use reagent_oauth::test_server;
