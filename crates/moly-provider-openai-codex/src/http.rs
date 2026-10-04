//! Redirect-free, proxy-free, retry-free bounded networking.

use std::time::Duration;

use reqwest::{Client, Response};
use serde::de::DeserializeOwned;

use crate::error::Error;

pub(crate) fn client() -> Result<Client, Error> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!(
            "moly-provider-openai-codex/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|_| Error::Internal)
}

pub(crate) fn network(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Network
    }
}

pub(crate) async fn body(mut response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if chunk.len() > limit - bytes.len() {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) async fn json<T: DeserializeOwned>(
    response: Response,
    limit: usize,
) -> Result<T, Error> {
    serde_json::from_slice(&body(response, limit).await?).map_err(|_| Error::Identity)
}
