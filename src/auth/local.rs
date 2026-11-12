use super::Identity;
use super::tokens::verify_secret;
use crate::config::LocalConfig;
use crate::error::{Error, Result};

pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::rand_core::OsRng;
    use argon2::password_hash::{PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Store(format!("hashing failed: {e}")))
}

pub fn check_credentials(cfg: &LocalConfig, username: &str, password: &str) -> Option<Identity> {
    if username != cfg.username {
        // Hash anyway so wrong username and wrong password take the same time.
        let _ = verify_secret(password, &cfg.password_hash);
        return None;
    }
    if verify_secret(password, &cfg.password_hash) {
        Some(Identity {
            subject: cfg.username.clone(),
            email: None,
            session: None,
        })
    } else {
        None
    }
}

pub fn assert_bind_is_safe(bind: &str) -> Result<()> {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let host = host.trim_start_matches('[').trim_end_matches(']');

    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);

    if loopback {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "auth.mode = \"local\" cannot bind to {bind}; bind to loopback behind a proxy or use oidc."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LocalConfig;

    fn cfg() -> LocalConfig {
        LocalConfig {
            username: "dev".into(),
            password_hash: hash_password("correct horse").unwrap(),
        }
    }

    #[test]
    fn correct_credentials_produce_an_identity() {
        let id = check_credentials(&cfg(), "dev", "correct horse").unwrap();
        assert_eq!(id.subject, "dev");
    }

    #[test]
    fn wrong_password_or_username_is_rejected() {
        assert!(check_credentials(&cfg(), "dev", "wrong").is_none());
        assert!(check_credentials(&cfg(), "someone", "correct horse").is_none());
        assert!(check_credentials(&cfg(), "", "").is_none());
    }

    #[test]
    fn local_mode_refuses_a_non_loopback_bind() {
        // A network-reachable dev shortcut is production auth by accident.
        assert!(assert_bind_is_safe("0.0.0.0:8080").is_err());
        assert!(assert_bind_is_safe("192.168.1.10:8080").is_err());
        assert!(assert_bind_is_safe("[::]:8080").is_err());
    }

    #[test]
    fn local_mode_allows_loopback() {
        assert!(assert_bind_is_safe("127.0.0.1:8080").is_ok());
        assert!(assert_bind_is_safe("localhost:8080").is_ok());
        assert!(assert_bind_is_safe("[::1]:8080").is_ok());
    }

    #[test]
    fn an_unparsable_bind_is_treated_as_unsafe() {
        assert!(assert_bind_is_safe("not a bind address").is_err());
    }

    #[test]
    fn a_hostname_that_merely_starts_with_localhost_is_not_loopback() {
        // "localhost.evil.com" resolves wherever its owner wants.
        assert!(assert_bind_is_safe("localhost.evil.com:8080").is_err());
    }
}
