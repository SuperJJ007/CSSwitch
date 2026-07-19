pub mod anthropic_compat;
pub mod auth;
pub mod codex_auth;
pub(crate) mod codex_models;
pub(crate) mod codex_network;
pub mod codex_protocol;
pub(crate) mod codex_transport;
pub mod config;
pub mod connect;
pub mod dsml_shim;
pub mod messages;
pub mod models;
pub mod openai_chat;
pub mod openai_responses;
pub mod policy;
pub(crate) mod provider_contracts;
pub mod science_control;
pub mod server;
pub mod skill_install;
pub mod static_profile;

#[cfg(test)]
pub(crate) fn test_temp_root() -> std::path::PathBuf {
    let private_tmp = std::path::PathBuf::from("/private/tmp");
    if private_tmp.is_dir() {
        private_tmp
    } else {
        std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
    }
}
