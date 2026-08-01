use serde::Deserialize;
use std::error::Error;
use std::sync::OnceLock;

/// Spectator-V5 is only ever queried on the JP1 platform.
const SPECTATOR_V5_ACTIVE_GAME_URL: &str =
    "https://jp1.api.riotgames.com/lol/spectator/v5/active-games/by-summoner";

/// The Spectator-V5 payload, narrowed to the fields the LOL monitor reads.
#[derive(Deserialize)]
struct CurrentGameInfo {
    #[serde(default)]
    participants: Vec<CurrentGameParticipant>,
}

#[derive(Deserialize)]
struct CurrentGameParticipant {
    #[serde(default, rename = "riotId")]
    riot_id: Option<String>,
}

/// Shared across the monitor's polling loop, which runs once a second by default.
fn spectator_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Riot IDs of every participant in `puuid`'s current game, or `None` when that
/// player is not in one.
pub async fn current_game_riot_ids(
    api_key: &str,
    puuid: &str,
) -> Result<Option<Vec<String>>, Box<dyn Error>> {
    let response = spectator_client()
        .get(format!("{}/{}", SPECTATOR_V5_ACTIVE_GAME_URL, puuid))
        .header("X-Riot-Token", api_key)
        .send()
        .await?;

    // Riot answers 404 for a player who is not currently in a game.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let game: CurrentGameInfo = response.error_for_status()?.json().await?;
    Ok(Some(
        game.participants
            .into_iter()
            .filter_map(|participant| participant.riot_id)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::CurrentGameInfo;

    #[test]
    fn parses_riot_ids_and_tolerates_missing_ones() {
        let payload = r#"{
            "gameId": 1,
            "participants": [
                { "puuid": "a", "riotId": "Player#JP1" },
                { "puuid": "b" }
            ]
        }"#;

        let game: CurrentGameInfo = serde_json::from_str(payload).unwrap();
        let ids: Vec<String> = game
            .participants
            .into_iter()
            .filter_map(|participant| participant.riot_id)
            .collect();
        assert_eq!(ids, vec!["Player#JP1".to_string()]);
    }

    #[test]
    fn treats_an_absent_participant_list_as_empty() {
        let game: CurrentGameInfo = serde_json::from_str(r#"{"gameId": 1}"#).unwrap();
        assert!(game.participants.is_empty());
    }
}
