//! Explicit SSH execution binding. Remote paths are validated lexically on this PC.
use bridge_domain::{DomainError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecution {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub executable: String,
    pub config: String,
    pub state_root: String,
    pub project: String,
    pub repository: String,
}
impl RemoteExecution {
    pub fn validate(&self) -> Result<()> {
        let error = || {
            DomainError::invalid_input(
                "invalid remote execution settings: check SSH host/user/port, absolute paths, project and Git repository",
            )
        };
        if self.host.is_empty()
            || self.host.len() > 253
            || !self
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
            || !self.host.as_bytes()[0].is_ascii_alphanumeric()
            || self.user.is_empty()
            || self.user.len() > 64
            || !self
                .user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || self.user.starts_with('-')
            || self.port == 0
            || self.project.parse::<bridge_domain::ProjectId>().is_err()
        {
            return Err(error());
        }
        for path in [&self.executable, &self.config, &self.state_root] {
            if !Path::new(path).is_absolute()
                || path.len() > 4096
                || path.chars().any(char::is_control)
                || Path::new(path)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
                || path == "/"
            {
                return Err(error());
            }
        }
        // Credentials belong in SSH/git credential helpers, never in project configuration.
        if self.repository.len() > 2000
            || self
                .repository
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || self.repository.starts_with('-')
        {
            return Err(error());
        }
        let valid = if let Ok(url) = url::Url::parse(&self.repository) {
            matches!(url.scheme(), "https" | "ssh")
                && url.host_str().is_some()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && (url.scheme() != "https" || url.username().is_empty())
        } else {
            self.repository.split_once(':').is_some_and(|(host, path)| {
                host.contains('@')
                    && !path.is_empty()
                    && !host.contains('/')
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"@._-".contains(&b))
            })
        };
        if !valid {
            return Err(error());
        }
        Ok(())
    }
}
pub(crate) fn parse(values: &toml::Table) -> Result<Option<RemoteExecution>> {
    values
        .get("remote_execution")
        .map(|v| {
            let settings: RemoteExecution = v
                .clone()
                .try_into()
                .map_err(|_| DomainError::invalid_input("invalid remote execution settings"))?;
            settings.validate()?;
            Ok(settings)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> RemoteExecution {
        RemoteExecution {
            host: "192.168.1.22".into(),
            user: "executor".into(),
            port: 22,
            executable: "/opt/bridge/agent-bridge".into(),
            config: "/home/executor/projects.toml".into(),
            state_root: "/home/executor/state".into(),
            project: "proj".into(),
            repository: "git@github.com:owner/repo.git".into(),
        }
    }
    #[test]
    fn remote_is_opt_in_and_rejects_options_secrets_and_unsafe_paths() {
        assert!(parse(&toml::Table::new()).unwrap().is_none());
        let valid = binding();
        assert!(valid.validate().is_ok());
        for repository in [
            "https://github.com/owner/repo.git",
            "ssh://git@gitlab.example:2222/owner/repo.git",
        ] {
            let mut b = valid.clone();
            b.repository = repository.into();
            assert!(b.validate().is_ok());
        }
        for repository in [
            "-x",
            "/tmp/repo.git",
            "https://token@github.com/repo",
            "https://git:secret@host/repo",
            "https://host/repo?token=secret",
            "ssh://host/repo\ncommand",
        ] {
            let mut b = valid.clone();
            b.repository = repository.into();
            assert!(b.validate().is_err(), "{repository}");
        }
        let mut b = valid.clone();
        b.host = "-oProxyCommand=bad".into();
        assert!(b.validate().is_err());
        let mut b = valid.clone();
        b.user = "user@other".into();
        assert!(b.validate().is_err());
        let mut b = valid.clone();
        b.config = "/tmp/../etc/config".into();
        assert!(b.validate().is_err());
        let mut b = valid;
        b.state_root = "/".into();
        assert!(b.validate().is_err());
    }
}
