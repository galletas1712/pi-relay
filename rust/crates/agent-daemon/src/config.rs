use std::{env, path::PathBuf};

use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) database_url: String,
    pub(crate) bind: String,
    pub(crate) workspace: PathBuf,
}

impl Config {
    pub(crate) fn from_env_and_args() -> Result<Self> {
        let mut database_url = env::var("DATABASE_URL").unwrap_or_default();
        let mut bind = env::var("PI_AGENTD_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
        let mut workspace = env::current_dir()?;

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
                "--workspace" => {
                    workspace = PathBuf::from(
                        args.next()
                            .ok_or_else(|| anyhow!("--workspace requires a value"))?,
                    );
                }
                "--help" | "-h" => {
                    println!(
                        "usage: pi-agentd --database-url postgres://... [--bind 127.0.0.1:8787] [--workspace .]"
                    );
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument: {other}")),
            }
        }

        if database_url.trim().is_empty() {
            return Err(anyhow!(
                "DATABASE_URL or --database-url is required for pi-agentd"
            ));
        }

        Ok(Self {
            database_url,
            bind,
            workspace,
        })
    }
}
