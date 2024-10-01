use async_trait::async_trait;
use regex::Regex;
// use reqwest_middleware::ClientWithMiddleware;
use chrono::{DateTime, Utc};
use std::error::Error; // Ensure this is included
use std::process::Command; // Ensure this is included

use super::{get_youtube_live_status, Live};
pub struct Youtube {
    pub channel_name: String, // Changed from room to channel_id
    pub channel_id: String,
    // pub access_token: String,
    // pub client: ClientWithMiddleware,
}
#[async_trait]
impl Live for Youtube {
    fn channel_name(&self) -> &str {
        &self.channel_name // Return channel_id instead of room
    }
    async fn get_status(&self) -> Result<(bool, Option<DateTime<Utc>>), Box<dyn Error>> {
        let status = get_youtube_live_status(&self.channel_id).await?;

        // Check for scheduled live event
        if !status.0 {
            // If not live
            if let Some(start_time) = status.1 {
                // Check if there's a scheduled start time
                return Ok((false, Some(start_time))); // Return scheduled start time
            }
        }

        Ok((status.0, None)) // Return live status and no scheduled time
    }
    async fn get_real_m3u8_url(&self) -> Result<String, Box<dyn Error>> {
        self.ytdlp()
    }
}

impl Youtube {
    pub fn new(channel_name: &str, channel_id: &str) -> impl Live {
        Youtube {
            channel_name: channel_name.to_string(),
            channel_id: channel_id.to_string(),
        }
    }

    pub fn ytdlp(&self) -> Result<String, Box<dyn Error>> {
        let mut command = Command::new("yt-dlp");
        command.arg("-g");
        command.arg(format!(
            "https://www.youtube.com/channel/{}/live",
            self.channel_id.as_str().replace("\"", "")
        ));
        match command.status().unwrap().code() {
            Some(code) => {
                if code == 0 {
                    let res = command.output().unwrap();
                    let res = String::from_utf8(res.stdout).unwrap();
                    Ok(self.replace_url(res.as_str()))
                } else {
                    Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "yt-dlp error",
                    )))
                }
            }
            None => Err("yt-dlp error".into()),
        }
    }

    fn replace_url(&self, content: &str) -> String {
        let re = Regex::new(r"^WARNING.*").unwrap();
        let res = re.replace_all(content, "");
        return res.to_string();
    }
}
