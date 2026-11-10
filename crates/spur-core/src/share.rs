// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Letting other users work inside a running job: who the owner shares it with,
//! and the controller-signed grant a node checks before running a joiner's step.

use serde::{Deserialize, Serialize};

use crate::job::JobId;
use crate::step::StepId;

/// `--share-with` keyword for every member of the job's account. A Unix
/// username cannot contain `@`, so it never shadows a real user.
pub const ACCOUNT_KEYWORD: &str = "@account";

/// Short enough that a leaked grant is useless soon; a reconnecting client
/// asks the controller for the step again rather than reusing an old grant.
pub const JOIN_GRANT_TTL_SECS: u64 = 3600;

/// Who besides the owner may run work inside a job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobShare {
    #[serde(default)]
    pub users: Vec<String>,
    /// Every member of the job's account.
    #[serde(default)]
    pub account: bool,
}

impl JobShare {
    /// Parse `--share-with` / `ShareWith=` values; each may itself be a comma list.
    pub fn parse<S: AsRef<str>>(values: &[S]) -> Result<Self, String> {
        let mut share = Self::default();
        for entry in values
            .iter()
            .flat_map(|v| v.as_ref().split(','))
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            if entry.eq_ignore_ascii_case(ACCOUNT_KEYWORD) {
                share.account = true;
                continue;
            }
            if entry.starts_with('@') {
                return Err(format!(
                    "unknown share target '{entry}'; the only keyword is '{ACCOUNT_KEYWORD}'"
                ));
            }
            if entry
                .chars()
                .any(|c| c.is_whitespace() || c == ':' || c == '/')
            {
                return Err(format!("'{entry}' is not a valid username"));
            }
            if !share.users.iter().any(|u| u == entry) {
                share.users.push(entry.to_string());
            }
        }
        Ok(share)
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && !self.account
    }

    /// The canonical entry list, the inverse of [`parse`](Self::parse).
    pub fn entries(&self) -> Vec<String> {
        let mut entries = self.users.clone();
        if self.account {
            entries.push(ACCOUNT_KEYWORD.to_string());
        }
        entries
    }
}

/// The controller's word that `user` may run step `step_id` of a job they do
/// not own, as `uid`/`gid`. Signed with the cluster's `jwt_key` when native
/// execution credentials are not configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinGrant {
    pub job_id: JobId,
    pub step_id: StepId,
    pub run_attempt: u32,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
}

/// Deliberately has no `sub`: a grant must never decode as a user bearer token.
#[derive(Debug, Serialize, Deserialize)]
struct JoinGrantClaims {
    kind: String,
    job: JobId,
    step: StepId,
    attempt: u32,
    user: String,
    uid: u32,
    gid: u32,
    iat: u64,
    exp: u64,
}

const JOIN_GRANT_KIND: &str = "spur-join";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JoinGrantError {
    #[error("join grant is expired")]
    Expired,
    #[error("join grant is invalid: {0}")]
    Invalid(String),
}

pub fn sign_join_grant(grant: &JoinGrant, key: &[u8], now: u64) -> Result<String, JoinGrantError> {
    use jsonwebtoken::{encode, EncodingKey, Header};
    if key.is_empty() {
        return Err(JoinGrantError::Invalid("no signing key".into()));
    }
    let claims = JoinGrantClaims {
        kind: JOIN_GRANT_KIND.into(),
        job: grant.job_id,
        step: grant.step_id,
        attempt: grant.run_attempt,
        user: grant.user.clone(),
        uid: grant.uid,
        gid: grant.gid,
        iat: now,
        exp: now.saturating_add(JOIN_GRANT_TTL_SECS),
    };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(key))
        .map_err(|e| JoinGrantError::Invalid(e.to_string()))
}

pub fn verify_join_grant(token: &str, key: &[u8]) -> Result<JoinGrant, JoinGrantError> {
    use jsonwebtoken::{decode, errors::ErrorKind, DecodingKey, Validation};
    if key.is_empty() {
        return Err(JoinGrantError::Invalid("no verification key".into()));
    }
    let claims = decode::<JoinGrantClaims>(
        token,
        &DecodingKey::from_secret(key),
        &Validation::default(),
    )
    .map_err(|e| match e.kind() {
        ErrorKind::ExpiredSignature => JoinGrantError::Expired,
        _ => JoinGrantError::Invalid(e.to_string()),
    })?
    .claims;
    if claims.kind != JOIN_GRANT_KIND {
        return Err(JoinGrantError::Invalid("not a join grant".into()));
    }
    Ok(JoinGrant {
        job_id: claims.job,
        step_id: claims.step,
        run_attempt: claims.attempt,
        user: claims.user,
        uid: claims.uid,
        gid: claims.gid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }

    #[test]
    fn parse_splits_dedupes_and_recognizes_the_account_keyword() {
        let share = JobShare::parse(&["alice, bob", "alice,@ACCOUNT", ""]).unwrap();
        assert_eq!(share.users, vec!["alice", "bob"]);
        assert!(share.account);
        assert_eq!(share.entries(), vec!["alice", "bob", "@account"]);
        assert_eq!(JobShare::parse(&share.entries()).unwrap(), share);
    }

    #[test]
    fn parse_of_nothing_is_empty() {
        assert!(JobShare::parse::<&str>(&[]).unwrap().is_empty());
        assert!(JobShare::parse(&[" , "]).unwrap().is_empty());
    }

    #[test]
    fn parse_rejects_unknown_keywords_and_malformed_names() {
        assert!(JobShare::parse(&["@everyone"]).is_err());
        assert!(JobShare::parse(&["bad name"]).is_err());
        assert!(JobShare::parse(&["user:1"]).is_err());
    }

    #[test]
    fn job_share_deserializes_from_an_empty_object() {
        let share: JobShare = serde_json::from_str("{}").unwrap();
        assert!(share.is_empty());
    }

    fn grant() -> JoinGrant {
        JoinGrant {
            job_id: 7,
            step_id: 3,
            run_attempt: 1,
            user: "bob".into(),
            uid: 1001,
            gid: 1001,
        }
    }

    #[test]
    fn join_grant_round_trips() {
        let token = sign_join_grant(&grant(), b"secret", now()).unwrap();
        assert_eq!(verify_join_grant(&token, b"secret").unwrap(), grant());
    }

    #[test]
    fn join_grant_rejects_a_wrong_key_and_an_expired_grant() {
        let token = sign_join_grant(&grant(), b"secret", now()).unwrap();
        assert!(matches!(
            verify_join_grant(&token, b"other"),
            Err(JoinGrantError::Invalid(_))
        ));
        let stale = sign_join_grant(&grant(), b"secret", now() - 2 * JOIN_GRANT_TTL_SECS).unwrap();
        assert_eq!(
            verify_join_grant(&stale, b"secret"),
            Err(JoinGrantError::Expired)
        );
        assert!(sign_join_grant(&grant(), b"", now()).is_err());
        assert!(matches!(
            verify_join_grant(&token, b""),
            Err(JoinGrantError::Invalid(_))
        ));
    }

    #[test]
    fn join_grant_rejects_same_shape_claims_of_another_kind() {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let claims = JoinGrantClaims {
            kind: "spur-other".into(),
            job: 7,
            step: 3,
            attempt: 1,
            user: "bob".into(),
            uid: 1001,
            gid: 1001,
            iat: now(),
            exp: now() + JOIN_GRANT_TTL_SECS,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        assert_eq!(
            verify_join_grant(&token, b"secret"),
            Err(JoinGrantError::Invalid("not a join grant".into()))
        );
    }

    #[test]
    fn a_user_token_is_not_a_join_grant_and_a_grant_is_not_a_user_token() {
        let user_token = crate::auth::generate_token("bob", 1001, false, b"secret", 3600).unwrap();
        assert!(verify_join_grant(&user_token, b"secret").is_err());
        let token = sign_join_grant(&grant(), b"secret", now()).unwrap();
        assert!(crate::auth::verify_token(&token, b"secret").is_err());
    }
}
