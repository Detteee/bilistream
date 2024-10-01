use super::Live;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest_middleware::ClientWithMiddleware;
use serde_json::json;
use std::error::Error;
use std::process::Command;

pub struct Twitch {
    pub channel_name: String,
    pub client: ClientWithMiddleware,
    pub oauth_token: String,
}

#[async_trait]
impl Live for Twitch {
    async fn get_status(
        &self,
    ) -> Result<(bool, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>> {
        let j = json!(
            {
                "operationName":"StreamMetadata",
                "variables":{
                    "channelLogin":&self.channel_name,
                },
                "extensions":{
                    "persistedQuery":{
                        "version":1,
                        "sha256Hash":"1c719a40e481453e5c48d9bb585d971b8b372f8ebb105b17076722264dfa5b3e"
                    }
                }
            }
        );
        let res: serde_json::Value = self
            .client
            .post("https://gql.twitch.tv/gql")
            .header("Client-ID", "kimne78kx3ncx6brgo4mv6wki5h1ko")
            .json(&j)
            .send()
            .await?
            .json()
            .await?;
        if res["data"]["user"]["stream"]["type"] == "live" {
            let m3u8_url = self.get_streamlink_url()?;
            Ok((true, Some(m3u8_url), None))
        } else {
            Ok((false, None, None))
        }
    }

    fn channel_name(&self) -> &str {
        &self.channel_name
    }
}

impl Twitch {
    pub fn new(channel_name: &str, oauth_token: String, client: ClientWithMiddleware) -> impl Live {
        Twitch {
            channel_name: channel_name.to_string(),
            client,
            oauth_token,
        }
    }

    pub fn get_streamlink_url(&self) -> Result<String, Box<dyn Error>> {
        let output = Command::new("streamlink")
            .arg("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-as.cdn-perfprod.com,https://lb-sa.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com")
            .arg("--twitch-disable-ads")
            .arg("--stream-url")
            .arg("--stream-type")
            .arg("hls")
            .arg("--twitch-api-header")
            .arg(format!("Authorization=OAuth {}", self.oauth_token))
            .arg(format!(
                "https://www.twitch.tv/{}",
                self.channel_name.as_str().replace("\"", "")
            ))
            .arg("best")
            .output()?;

        if output.status.success() {
            let url = String::from_utf8(output.stdout)?.trim().to_string();
            Ok(url)
        } else {
            let error = String::from_utf8(output.stderr)?;
            Err(error.into())
        }
    }
}
