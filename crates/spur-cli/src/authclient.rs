// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller channel that attaches the caller's credential.
//!
//! Every CLI subcommand connects through [`connect`], so a token is sent on all RPCs without each
//! command knowing about authentication. Having no token is not an error: the control plane decides
//! whether that is acceptable via `[auth] mode`, and the CLI stays usable against a cluster that has
//! not adopted authentication yet.

use std::path::PathBuf;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Request, Status};

/// Environment variable holding a credential, checked before the on-disk token.
const TOKEN_ENV: &str = "SPUR_AUTH_TOKEN";

/// Credential file, relative to the user's home directory.
const TOKEN_FILE: &str = ".spur/token";

/// A channel that attaches the caller's credential to every request.
pub type AuthChannel = InterceptedService<Channel, AuthInterceptor>;

#[derive(Clone, Default)]
pub struct AuthInterceptor {
    /// Pre-formatted `Bearer <token>`; `None` when the caller has no credential.
    header: Option<MetadataValue<tonic::metadata::Ascii>>,
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(value) = &self.header {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        Ok(request)
    }
}

/// Read the caller's credential: `$SPUR_AUTH_TOKEN`, else `~/.spur/token`.
///
/// A token file with group/other permissions is ignored with a warning rather than used — a bearer
/// credential readable by other users on a shared login node is not a credential.
pub fn load_token() -> Option<String> {
    if let Ok(t) = std::env::var(TOKEN_ENV) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path: PathBuf = dirs_home()?.join(TOKEN_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o077;
        if mode != 0 {
            eprintln!(
                "warning: ignoring {} because it is readable by other users (chmod 600 it)",
                path.display()
            );
            return None;
        }
    }
    let t = std::fs::read_to_string(&path).ok()?.trim().to_string();
    (!t.is_empty()).then_some(t)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn interceptor() -> AuthInterceptor {
    let header = load_token().and_then(|t| MetadataValue::try_from(format!("Bearer {t}")).ok());
    AuthInterceptor { header }
}

/// Wrap an already-established channel with the caller's credential.
///
/// Used when the channel is built by the caller (e.g. agent connections, test channels) rather
/// than going through [`connect`].
pub fn wrap(channel: Channel) -> AuthChannel {
    InterceptedService::new(channel, interceptor())
}

/// Connect to the controller, attaching the caller's credential if one is available.
pub async fn connect(endpoints: &str) -> Result<AuthChannel, tonic::transport::Error> {
    // NOTE: the raw transport connect — deliberately the only `spur_client::connect_channel` call
    // left in the CLI, so every subcommand goes through the credential-attaching wrapper.
    let channel = spur_client::connect_channel(endpoints).await?;
    Ok(InterceptedService::new(channel, interceptor()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both env cases live in ONE test on purpose: the variable is process-global, so two tests
    /// mutating it run concurrently under the default test harness and race each other.
    #[test]
    fn env_token_is_trimmed_and_blank_is_not_a_credential() {
        // SAFETY: this is the only test that touches TOKEN_ENV, so no other thread reads it here.
        unsafe { std::env::set_var(TOKEN_ENV, "  abc123\n") };
        assert_eq!(
            load_token().as_deref(),
            Some("abc123"),
            "the env credential wins and is trimmed"
        );

        unsafe { std::env::set_var(TOKEN_ENV, "   ") };
        // Blank falls through to the file rather than sending a literal "Bearer " with no token.
        let blank = load_token();
        unsafe { std::env::remove_var(TOKEN_ENV) };
        assert!(
            blank.as_deref() != Some(""),
            "a blank env value must not become an empty credential"
        );
    }

    #[test]
    fn no_credential_yields_an_interceptor_that_adds_no_header() {
        let i = AuthInterceptor::default();
        assert!(i.header.is_none());
    }
}
