use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::fill;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DeviceScope {
    Read,
    Review,
    ContentWrite,
    SettingsWrite,
    Admin,
}

impl DeviceScope {
    pub(crate) fn parse(value: &str) -> Result<Self, AuthError> {
        match value {
            "read" => Ok(Self::Read),
            "review" => Ok(Self::Review),
            "content:write" => Ok(Self::ContentWrite),
            "settings:write" => Ok(Self::SettingsWrite),
            "admin" => Ok(Self::Admin),
            _ => Err(AuthError::Scope),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Review => "review",
            Self::ContentWrite => "content:write",
            Self::SettingsWrite => "settings:write",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewDevice {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) token: String,
    pub(crate) token_digest: String,
    pub(crate) scopes: Vec<DeviceScope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceRecord {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) scopes: Vec<DeviceScope>,
    pub(crate) created_at_us: u64,
    pub(crate) last_used_at_us: Option<u64>,
    pub(crate) revoked_at_us: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DevicePrincipal {
    pub(crate) device_id: String,
    pub(crate) scopes: Vec<DeviceScope>,
}

impl DevicePrincipal {
    pub(crate) fn permits(&self, required: DeviceScope) -> bool {
        self.scopes.contains(&DeviceScope::Admin) || self.scopes.contains(&required)
    }
}

pub(crate) fn generate_device(name: String, scopes: Vec<String>) -> Result<NewDevice, AuthError> {
    if name.trim().is_empty() || name.len() > 128 {
        return Err(AuthError::Name);
    }
    let scopes = scopes
        .into_iter()
        .map(|value| DeviceScope::parse(&value))
        .collect::<Result<BTreeSet<_>, _>>()?
        .into_iter()
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return Err(AuthError::Scope);
    }
    let mut secret = [0_u8; 32];
    fill(&mut secret).map_err(|_| AuthError::Random)?;
    let token = format!("gnosis_{}", URL_SAFE_NO_PAD.encode(secret));
    Ok(NewDevice {
        device_id: Uuid::now_v7().to_string(),
        name,
        token_digest: token_digest(&token),
        token,
        scopes,
    })
}

pub(crate) fn token_digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub(crate) fn parse_stored_scopes(value: &str) -> Result<Vec<DeviceScope>, AuthError> {
    serde_json::from_str::<Vec<String>>(value)
        .map_err(|_| AuthError::Scope)?
        .into_iter()
        .map(|scope| DeviceScope::parse(&scope))
        .collect()
}

pub(crate) fn stored_scopes(scopes: &[DeviceScope]) -> Result<String, AuthError> {
    serde_json::to_string(
        &scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|_| AuthError::Scope)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum AuthError {
    #[error("device name is invalid")]
    Name,
    #[error("device scope is invalid")]
    Scope,
    #[error("device secret generation failed")]
    Random,
}

#[cfg(test)]
mod tests {
    use super::{DeviceScope, generate_device, token_digest};

    #[test]
    fn device_tokens_are_high_entropy_and_store_only_a_digest() {
        let first = generate_device("phone".into(), vec!["read".into()]).unwrap();
        let second = generate_device("phone".into(), vec!["read".into()]).unwrap();
        assert_ne!(first.token, second.token);
        assert!(first.token.len() >= 50);
        assert_eq!(first.token_digest, token_digest(&first.token));
        assert_eq!(first.scopes, [DeviceScope::Read]);
    }
}
