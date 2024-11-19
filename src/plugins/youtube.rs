use async_trait::async_trait;
// use reqwest_middleware::ClientWithMiddleware;
use super::{get_youtube_live_status, Live};
use chrono::{DateTime, Utc};
use std::error::Error; // Ensure this is included
use std::process::Command;
pub struct Youtube {
    pub channel_name: String,
    pub channel_id: String,
    pub proxy: Option<String>,
    // pub access_token: String,
    // pub client: ClientWithMiddleware,
}
#[async_trait]
impl Live for Youtube {
    async fn get_title(&self) -> Result<String, Box<dyn Error>> {
        let mut command = Command::new("yt-dlp");
        if let Some(proxy) = &self.proxy {
            command.arg("--proxy").arg(proxy);
        }
        command.arg("-e");
        command.arg(format!(
            "https://www.youtube.com/channel/{}/live",
            self.channel_id
        ));
        let output = command.output()?;
        let live_title = String::from_utf8_lossy(&output.stdout);
        // println!("live_title: {}", live_title);
        Ok(live_title.to_string())
    }
    // fn channel_name(&self) -> &str {
    //     &self.channel_name // Return channel_id instead of room
    // }
    async fn get_status(
        &self,
    ) -> Result<(bool, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>> {
        let status = get_youtube_live_status(&self.channel_id, self.proxy.clone()).await?;

        // Check for scheduled live event
        if !status.0 {
            // If not live
            if let Some(start_time) = status.2 {
                // Check if there's a scheduled start time
                return Ok((false, None, Some(start_time))); // Return scheduled start time
            }
        }

        Ok((status.0, status.1, None)) // Return live status and no scheduled time
    }
}

impl Youtube {
    pub fn new(channel_name: &str, channel_id: &str, proxy: Option<String>) -> impl Live {
        Youtube {
            channel_name: channel_name.to_string(),
            channel_id: channel_id.to_string(),
            proxy,
        }
    }
}
