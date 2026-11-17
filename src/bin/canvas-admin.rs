//! Closed provisioning CLI. Never run in CI: enrollment output is a privileged capability.

use anyhow::{Context, Result};
use endless_drawing_server::{admin::provision, config::Config, db};
use std::io::{self, IsTerminal, Write};
use zeroize::Zeroizing;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2 && matches!(args[0].as_str(), "create" | "recover" | "disable"),
        "usage: canvas-admin create|recover|disable LOGIN (interactive trusted terminal only)"
    );
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "requires an interactive trusted terminal; never run in Actions or log/redirection pipelines"
    );
    if args[0] != "create" {
        eprintln!(
            "{} revokes existing access. Verify the owner's identity before recovery.",
            args[0]
        );
        eprint!("Type '{} {}' to confirm: ", args[0], args[1]);
        io::stderr().flush()?;
        let mut confirmation = String::new();
        io::stdin().read_line(&mut confirmation)?;
        anyhow::ensure!(
            confirmation.trim() == format!("{} {}", args[0], args[1]),
            "confirmation did not match"
        );
    }
    let password = if args[0] != "disable" {
        let first = Zeroizing::new(rpassword::prompt_password(
            "New password (manager-generated, minimum 20 characters): ",
        )?);
        let second = Zeroizing::new(rpassword::prompt_password("Repeat password: ")?);
        anyhow::ensure!(*first == *second, "passwords did not match");
        Some(first)
    } else {
        None
    };
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(dotenvy::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("could not load .env"),
    }
    let config = Config::from_env()?;
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let pool = db::connect(&config).await.context("could not connect to PostgreSQL")?;
        // The running server must already have applied this image's migrations.
        let result = match args[0].as_str() {
            "create" => Some(provision::create(&pool, &args[1], password.ok_or_else(|| anyhow::anyhow!("missing password"))?).await?),
            "recover" => Some(provision::recover(&pool, &args[1], password.ok_or_else(|| anyhow::anyhow!("missing password"))?).await?),
            _ => { provision::disable(&pool, &args[1]).await?; None },
        };
        pool.close().await;
        if let Some(grant) = result {
            eprintln!("Account: {}. Enrollment expires: {}. Register ONE UV passkey, then sign in to optionally connect Google Authenticator backup. This is NOT an admin session.", grant.account_id(), grant.expires_at());
            eprintln!("Keep the following enrollment capability private; enter only in the HTTPS admin enrollment form:");
            println!("{}", grant.secret());
        } else { eprintln!("Administrator disabled; existing access revoked."); }
        Ok::<(), anyhow::Error>(())
    })
}
