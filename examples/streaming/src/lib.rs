//! Setup shared by the streaming examples.

use schwab_rs::{SchwabClient, SchwabConfig, StreamClient, config::StreamConfig};

/// Build a client from the `SCHWAB_*` environment (`.env` is loaded if
/// present), make sure it holds tokens, and return a stream client whose
/// LOGIN identity (customer ID, channel, function ID) comes from the user
/// preferences, as Schwab requires. Not yet connected.
pub async fn stream_client(
    config: StreamConfig,
) -> Result<(SchwabClient, StreamClient), Box<dyn std::error::Error>> {
    if dotenvy::from_filename("../../.env").is_err() {
        let _ = dotenvy::dotenv();
    }
    let mut schwab = SchwabConfig::from_env()?;
    schwab.oauth.capture_callback |= cfg!(feature = "callback-server");
    let client = SchwabClient::new(schwab)?;

    let auth = client.auth_manager();
    if !auth.has_valid_tokens() {
        let (url, code) = auth.authorize().await?;
        if code.is_empty() {
            return Err(format!(
                "no valid tokens: authorize at {url} and exchange the code \
                 (cargo run -p oauth-flow), then run this example again"
            )
            .into());
        }
    }
    client.init().await?;

    let prefs = client.get_user_preferences().await?;
    let info = prefs
        .streamer_info
        .as_deref()
        .and_then(<[_]>::first)
        .ok_or("user preferences carry no streamer info")?;
    let stream = StreamClient::builder()
        .config(config)
        .auth_manager(auth)
        .streamer_info(info)
        .build()?;
    Ok((client, stream))
}
