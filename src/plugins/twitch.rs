use super::Live;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use reqwest::Client;
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
    async fn get_status(
        &self,
    ) -> Result<
        (
            bool,                    // is_live
            Option<String>,          // topic
            Option<String>,          // title
            Option<String>,          // m3u8_url
            Option<DateTime<Local>>, // start_time
        ),
        Box<dyn Error>,
    > {
        /*         let j = json!(
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
            .await? */
        // println!("{:?}", res);
        let (is_live, game_name, title) = get_twitch_status(&self.channel_id).await?;
        if is_live {
            let m3u8_url = self.get_streamlink_url()?;
            return Ok((
                is_live,
                Some(game_name.unwrap_or_default()),
                Some(title.unwrap_or_default()),
                Some(m3u8_url),
                None,
            ));
        } else {
            return Ok((is_live, None, None, None, None));
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
    fn get_streamlink_url(&self) -> Result<String, Box<dyn Error>> {
        // First try with configured proxy region
        match self.try_with_proxy(&self.proxy_region) {
            Ok(url) => return Ok(url),
            Err(e) => tracing::warn!("Failed with configured proxy {}: {}", self.proxy_region, e),
        }

        // Try backup proxy regions in order
        let backup_regions = ["asl", "as", "na", "sa", "eu", "eu3"];
        for region in backup_regions {
            if region == self.proxy_region {
                continue; // Skip if it's the same as the already tried region
            }
            match self.try_with_proxy(region) {
                Ok(url) => {
                    tracing::info!(
                        "Successfully got stream URL with backup proxy region: {}",
                        region
                    );
                    return Ok(url);
                }
                Err(e) => tracing::debug!("Failed with backup proxy region {}: {}", region, e),
            }
        }

        // If all proxies fail, return the last error
        tracing::error!("Failed to get stream URL with all proxy regions");
        Err("Failed to get stream URL with all proxy regions".into())
    }

    fn try_with_proxy(&self, proxy_region: &str) -> Result<String, Box<dyn Error>> {
        let proxy_url = self.get_proxy_url_for_region(proxy_region)?;
        let output = Command::new("streamlink")
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

    fn get_proxy_url_for_region(&self, region: &str) -> Result<String, &'static str> {
        match region {
            "na" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com".to_string()),
            "eu" => Ok("--twitch-proxy-playlist=https://lb-eu.cdn-perfprod.com".to_string()),
            "eu2" => Ok("--twitch-proxy-playlist=https://lb-eu2.cdn-perfprod.com".to_string()),
            "eu3" => Ok("--twitch-proxy-playlist=https://lb-eu3.cdn-perfprod.com".to_string()),
            "eu4" => Ok("--twitch-proxy-playlist=https://lb-eu4.cdn-perfprod.com".to_string()),
            "eu5" => Ok("--twitch-proxy-playlist=https://lb-eu5.cdn-perfprod.com".to_string()),
            "as" => Ok("--twitch-proxy-playlist=https://lb-as.cdn-perfprod.com".to_string()),
            "sa" => Ok("--twitch-proxy-playlist=https://lb-sa.cdn-perfprod.com".to_string()),
            "eul" => Ok("--twitch-proxy-playlist=https://eu.luminous.dev".to_string()),
            "eu2l" => Ok("--twitch-proxy-playlist=https://eu2.luminous.dev".to_string()),
            "asl" => Ok("--twitch-proxy-playlist=https://as.luminous.dev".to_string()),
            "all" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com,https://eu.luminous.dev,https://eu2.luminous.dev,https://as.luminous.dev".to_string()),
            "perf" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com".to_string()),
            "" => Ok(String::new()),
            _ => Err("Invalid proxy region specified"),
        }
    }
}

pub async fn get_twitch_status(
    channel_id: &str,
) -> Result<
    (
        bool,           // is_live
        Option<String>, // topic
        Option<String>, // title
    ),
    Box<dyn std::error::Error>,
> {
    let client = Client::new();

    let query = r#"
    query GetStreamInfo($login: String!) {
        user(login: $login) {
            stream {
                id
                game {
                    id
                    name
                    displayName
                }
                title
                type
                viewersCount
                language
                tags {
                    id
                    localizedName
                }
            }
        }
    }"#;

    let variables = json!({
        "login": channel_id
    });

    let response = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .json(&json!({
            "query": query,
            "variables": variables
        }))
        .send()
        .await?;

    let json_response = response.json::<serde_json::Value>().await?;
    // status = {is_live, game_name, title}
    let is_live = json_response["data"]["user"]["stream"]["type"] == "live";
    let game_name = json_response["data"]["user"]["stream"]["game"]["name"]
        .as_str()
        .unwrap_or("");
    let title = json_response["data"]["user"]["stream"]["title"]
        .as_str()
        .unwrap_or("");
    // let start_time = json_response["data"]["user"]["stream"]["start_time"].as_str().unwrap_or("");
    // Parse the response to get game name
    // println!("{:?}", json_response);

    Ok((
        is_live,
        Some(game_name.to_string()),
        Some(title.to_string()),
    ))
}
