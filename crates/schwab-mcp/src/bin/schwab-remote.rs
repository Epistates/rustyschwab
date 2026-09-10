//! Submit a typed JSON authority request using a protected short-lived token file.
use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    request: PathBuf,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    schwab_rs::security::verify_file_permissions(&args.token_file)?;
    let token =
        secrecy::SecretString::from(std::fs::read_to_string(args.token_file)?.trim().to_owned());
    let request: serde_json::Value = serde_json::from_slice(&std::fs::read(args.request)?)?;
    let remote = schwab_rs::remote::RemoteBackend::new(&args.endpoint, token)?;
    let reply: serde_json::Value = remote.post(&request).await?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    if matches!(
        reply.get("status").and_then(|v| v.as_str()),
        Some("unknown" | "rejected")
    ) {
        anyhow::bail!("Gateway did not confirm acceptance; reconcile using the attempt ID");
    }
    Ok(())
}
