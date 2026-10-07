use std::collections::HashMap;
use std::ffi::OsString;

use warp_core::channel::ChannelState;

pub(crate) const FOCUS_URL_ENV: &str = "WARP_FOCUS_URL";
pub(crate) const TERMINAL_SESSION_UUID_ENV: &str = "WARP_TERMINAL_SESSION_UUID";

pub(crate) fn session_focus_url(session_uuid_hex: &str) -> String {
    format!(
        "{}://session/{session_uuid_hex}",
        ChannelState::url_scheme()
    )
}

pub fn add_session_focus_env_vars(env_vars: &mut HashMap<OsString, OsString>, session_uuid: &[u8]) {
    let session_uuid_hex = hex::encode(session_uuid);
    env_vars.insert(
        OsString::from(TERMINAL_SESSION_UUID_ENV),
        OsString::from(session_uuid_hex.clone()),
    );
    env_vars.insert(
        OsString::from(FOCUS_URL_ENV),
        OsString::from(session_focus_url(&session_uuid_hex)),
    );
}

/// Returns the hex-encoded session UUID set by [`add_session_focus_env_vars`], if any.
pub fn session_uuid_from_env_vars(env_vars: &HashMap<OsString, OsString>) -> Option<&str> {
    env_vars
        .get(&OsString::from(TERMINAL_SESSION_UUID_ENV))
        .and_then(|uuid| uuid.to_str())
}

#[cfg(test)]
#[path = "focus_env_tests.rs"]
mod tests;
