use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use pi_web::{normalize_host_authority, GitExecutables};

pub(crate) struct Config {
    pub(crate) database_url: String,
    pub(crate) bind: SocketAddr,
    pub(crate) web_root: PathBuf,
    pub(crate) allowed_hosts: Vec<String>,
    pub(crate) git_executables: GitExecutables,
}

fn normalize_hosts(hosts: Vec<String>) -> Result<Vec<String>> {
    let mut hosts = hosts
        .into_iter()
        .map(|host| {
            normalize_host_authority(&host)
                .ok_or_else(|| anyhow!("invalid PI_WEB_ALLOWED_HOSTS/--allowed-host: {host}"))
        })
        .collect::<Result<Vec<_>>>()?;
    hosts.sort();
    hosts.dedup();
    Ok(hosts)
}

fn validated_bind(value: &str, allow_non_loopback: bool) -> Result<SocketAddr> {
    let bind: SocketAddr = value
        .parse()
        .map_err(|_| anyhow!("pi-web bind must be an IP socket address"))?;
    if !bind.ip().is_loopback() && !allow_non_loopback {
        return Err(anyhow!(
            "refusing non-loopback bind {bind}; set PI_WEB_ALLOW_NON_LOOPBACK=1 \
             or pass --allow-non-loopback only behind a trusted access layer"
        ));
    }
    Ok(bind)
}

impl Config {
    pub(crate) fn from_env_and_args() -> Result<Self> {
        let mut database_url = env::var("DATABASE_URL").unwrap_or_default();
        let mut bind = env::var("PI_WEB_BIND").unwrap_or_else(|_| "127.0.0.1:8788".to_string());
        let mut web_root = env::var_os("PI_WEB_DIST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("packages/web/dist"));
        let mut allowed_hosts = split_hosts(&env::var("PI_WEB_ALLOWED_HOSTS").unwrap_or_default());
        let mut allow_non_loopback = env_flag("PI_WEB_ALLOW_NON_LOOPBACK");
        let mut git_bin = env::var_os("PI_WEB_GIT_BIN").map(PathBuf::from);
        let mut gh_bin = env::var_os("PI_WEB_GH_BIN").map(PathBuf::from);

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--database-url" => {
                    database_url = args
                        .next()
                        .ok_or_else(|| anyhow!("--database-url requires a value"))?;
                }
                "--bind" => {
                    bind = args
                        .next()
                        .ok_or_else(|| anyhow!("--bind requires a value"))?;
                }
                "--web-root" => {
                    web_root = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow!("--web-root requires a value"))?;
                }
                "--allowed-host" => {
                    allowed_hosts.push(
                        args.next()
                            .ok_or_else(|| anyhow!("--allowed-host requires a value"))?,
                    );
                }
                "--git-bin" => {
                    git_bin = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or_else(|| anyhow!("--git-bin requires a value"))?,
                    );
                }
                "--gh-bin" => {
                    gh_bin = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or_else(|| anyhow!("--gh-bin requires a value"))?,
                    );
                }
                "--allow-non-loopback" => allow_non_loopback = true,
                "--help" | "-h" => {
                    println!(
                        "usage: pi-web --database-url postgres://... \
                         [--bind 127.0.0.1:8788] [--web-root packages/web/dist] \
                         [--allowed-host HOST] [--git-bin /absolute/git] \
                         [--gh-bin /absolute/gh] [--allow-non-loopback]"
                    );
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument: {other}")),
            }
        }

        if database_url.trim().is_empty() {
            return Err(anyhow!(
                "DATABASE_URL or --database-url is required for pi-web"
            ));
        }
        let bind = validated_bind(&bind, allow_non_loopback)?;
        if web_root.as_os_str().is_empty() {
            return Err(anyhow!("pi-web web root must not be empty"));
        }

        allowed_hosts.extend(["localhost".to_string(), "127.0.0.1".to_string()]);
        allowed_hosts.push("[::1]".to_string());
        allowed_hosts.push(match bind.ip() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        });
        allowed_hosts = normalize_hosts(allowed_hosts)?;
        let git_executables = GitExecutables::resolve(git_bin.as_deref(), gh_bin.as_deref())?;

        Ok(Self {
            database_url,
            bind,
            web_root,
            allowed_hosts,
            git_executables,
        })
    }
}

fn split_hosts(value: &str) -> Vec<String> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect()
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_is_loopback_by_default_and_non_loopback_requires_acknowledgement() {
        assert_eq!(
            validated_bind("127.0.0.1:8788", false).expect("loopback"),
            "127.0.0.1:8788".parse().unwrap()
        );
        assert!(validated_bind("0.0.0.0:8788", false)
            .expect_err("non-loopback must fail")
            .to_string()
            .contains("refusing non-loopback bind"));
        assert!(validated_bind("0.0.0.0:8788", true).is_ok());
        assert!(validated_bind("localhost:8788", true).is_err());
    }

    #[test]
    fn allowed_hosts_use_the_strict_request_authority_parser() {
        assert_eq!(
            normalize_hosts(vec![
                "Example.TEST.:8788".to_string(),
                "[::1]".to_string(),
                "[::1]:8788".to_string(),
            ])
            .expect("valid hosts"),
            vec!["::1".to_string(), "example.test".to_string()]
        );
        for invalid in [
            "user@example.test",
            "example.test:bad",
            "[::1]suffix",
            "::1",
            "",
        ] {
            assert!(
                normalize_hosts(vec![invalid.to_string()]).is_err(),
                "{invalid}"
            );
        }
        assert!(normalize_hosts(split_hosts("one.example,,two.example")).is_err());
        assert!(split_hosts("  ").is_empty());
    }
}
