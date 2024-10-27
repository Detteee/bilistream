use super::Live;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest_middleware::ClientWithMiddleware;
use serde_json::json;
use std::error::Error;
use std::process::Command;

pub struct Twitch {
    pub channel_id: String,
    pub client: ClientWithMiddleware,
    pub oauth_token: String,
    pub proxy_region: String,
}

#[async_trait]
impl Live for Twitch {
    async fn get_title(&self) -> Result<String, Box<dyn Error>> {
        let j = json!(
            {
                "operationName":"StreamMetadata",
                "variables":{
                    "channelLogin":&self.channel_id,
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
        // println!("{:?}", res["data"]["user"]["lastBroadcast"]["title"]);
        Ok(res["data"]["user"]["lastBroadcast"]["title"]
            .as_str()
            .unwrap()
            .to_string())
    }
    async fn get_status(
        &self,
    ) -> Result<(bool, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>> {
        let j = json!(
            {
                "operationName":"StreamMetadata",
                "variables":{
                    "channelLogin":&self.channel_id,
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
        // println!("{:?}", res["data"]["user"]["lastBroadcast"]["title"]);
        if res["data"]["user"]["stream"]["type"] == "live" {
            let m3u8_url = self.get_streamlink_url()?;
            Ok((true, Some(m3u8_url), None))
        } else {
            Ok((false, None, None))
        }
    }

    // fn channel_name(&self) -> &str {
    //     &self.channel_id
    // }
}

impl Twitch {
    pub fn new(
        channel_id: &str,
        oauth_token: String,
        client: ClientWithMiddleware,
        proxy_region: String,
    ) -> impl Live {
        Twitch {
            channel_id: channel_id.to_string(),
            client,
            oauth_token,
            proxy_region,
        }
    }
    pub fn get_proxy_url(&self) -> Result<String, &'static str> {
        match self.proxy_region.as_str() {
            "na" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com".to_string()),
            "eu" => Ok("--twitch-proxy-playlist=https://lb-eu.cdn-perfprod.com".to_string()),
            "eu2" => Ok("--twitch-proxy-playlist=https://lb-eu2.cdn-perfprod.com".to_string()),
            "eu3" => Ok("--twitch-proxy-playlist=https://lb-eu3.cdn-perfprod.com".to_string()),
            "eu4" => Ok("--twitch-proxy-playlist=https://lb-eu4.cdn-perfprod.com".to_string()),
            "eu5" => Ok("--twitch-proxy-playlist=https://lb-eu5.cdn-perfprod.com".to_string()),
            "as" => Ok("--twitch-proxy-playlist=https://lb-as.cdn-perfprod.com".to_string()),
            "sa" => Ok("--twitch-proxy-playlist=https://lb-sa.cdn-perfprod.com".to_string()),
            // "eul" => Ok("--twitch-proxy-playlist=https://eu.luminous.dev".to_string()),
            "eu2l" => Ok("--twitch-proxy-playlist=https://eu2.luminous.dev".to_string()),
            "asl" => Ok("--twitch-proxy-playlist=https://as.luminous.dev".to_string()),
            "all" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com,https://eu.luminous.dev,https://eu2.luminous.dev,https://as.luminous.dev".to_string()),
            "perf" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com".to_string()),
            "lu" => Ok("--twitch-proxy-playlist=https://eu.luminous.dev,https://eu2.luminous.dev,https://as.luminous.dev".to_string()),
            _ => Err("Invalid proxy region specified"),
        }
    }
    pub fn get_streamlink_url(&self) -> Result<String, Box<dyn Error>> {
        let proxy_url = self.get_proxy_url()?;
        let output = Command::new("streamlink")
            // .arg("--twitch-proxy-playlist=https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com")
            // .arg("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com")
            .arg(proxy_url)
            .arg("--stream-url")
            .arg("--stream-type")
            .arg("hls")
            .arg("--twitch-api-header")
            .arg(format!("Authorization=OAuth {}", self.oauth_token))
            .arg(format!(
                "https://www.twitch.tv/{}",
                self.channel_id.as_str().replace("\"", "")
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
