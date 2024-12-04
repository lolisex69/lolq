use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio;
use serde::{Deserialize, Serialize};
use reqwest;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{SinkExt, StreamExt};
use toml;
use std::process::Command;
use std::collections::HashMap;
use regex::Regex;

#[derive(Debug, Deserialize)]
struct Config {
    picks: Vec<String>,
    bans: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ChampionData {
    name: String,
    id: i32,
}

#[derive(Debug, Deserialize)]
struct SummonerData {
    #[serde(rename = "summonerId")]
    summoner_id: i64,
}

struct LcuClient {
    client: reqwest::Client,
    base_url: String,
    auth: String,
}

impl LcuClient {
    async fn new() -> Option<Self> {
        // Find League process and extract port/password
        let output = Command::new("powershell")
            .args(&[
                "Get-WmiObject",
                "Win32_Process",
                "-Filter",
                "name='LeagueClientUx.exe'",
                "|",
                "Select-Object",
                "CommandLine",
            ])
            .output()
            .ok()?;

        let output_str = String::from_utf8_lossy(&output.stdout);
        
        let port_re = Regex::new(r"--app-port=(\d+)").unwrap();
        let auth_re = Regex::new(r"--remoting-auth-token=([a-zA-Z0-9_-]+)").unwrap();

        let port = port_re.captures(&output_str)?.get(1)?.as_str();
        let auth = auth_re.captures(&output_str)?.get(1)?.as_str();

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        
        let base_url = format!("https://127.0.0.1:{}", port);

        Some(Self {
            client,
            base_url,
            auth: auth.to_string(),
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, reqwest::Error> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth))
            .send()
            .await?
            .json()
            .await
    }

    async fn post(&self, path: &str, json: Option<serde_json::Value>) -> Result<(), reqwest::Error> {
        let mut req = self.client
            .post(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth));

        if let Some(json) = json {
            req = req.json(&json);
        }

        req.send().await?;
        Ok(())
    }

    async fn patch(&self, path: &str, json: serde_json::Value) -> Result<(), reqwest::Error> {
        self.client
            .patch(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth))
            .json(&json)
            .send()
            .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // Load config
    let config: Config = toml::from_str(
        &fs::read_to_string("config.toml").expect("Failed to read config file")
    ).expect("Failed to parse config");

    let lcu = LcuClient::new().await.expect("Failed to connect to League Client");

    // Get summoner data
    let summoner: SummonerData = lcu.get("/lol-summoner/v1/current-summoner")
        .await
        .expect("Failed to get summoner data");

    // Get champion data
    let champions: Vec<ChampionData> = lcu.get(&format!(
        "/lol-champions/v1/inventories/{}/champions-minimal",
        summoner.summoner_id
    ))
    .await
    .expect("Failed to get champion data");

    let champions_map: HashMap<String, i32> = champions
        .into_iter()
        .map(|c| (c.name, c.id))
        .collect();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    let ws_url = format!("wss://127.0.0.1:{}", lcu.base_url.split(":").nth(2).unwrap());
    let (mut ws_stream, _) = connect_async(ws_url)
        .await
        .expect("Failed to connect to LCU websocket");

    let mut pick_number = 0;
    let mut ban_number = 0;
    let mut am_i_assigned = false;
    let mut am_i_picking = false;
    let mut am_i_banning = false;
    let mut have_i_prepicked = false;
    let mut in_game = false;
    let mut action_id = 0;

    while running.load(Ordering::SeqCst) {
        if let Some(msg) = ws_stream.next().await {
            let msg = msg.expect("Failed to get websocket message");
            let data: serde_json::Value = serde_json::from_str(&msg.to_string())
                .expect("Failed to parse websocket message");

            match data["uri"].as_str() {
                Some("/lol-matchmaking/v1/ready-check") => {
                    if data["data"]["state"] == "InProgress" && data["data"]["playerResponse"] == "None" {
                        lcu.post("/lol-matchmaking/v1/ready-check/accept", None)
                            .await
                            .expect("Failed to accept ready check");
                    }
                }

                Some("/lol-champ-select/v1/session") => {
                    let session = &data["data"];
                    let local_player_cell_id = session["localPlayerCellId"].as_i64().unwrap();
                    let phase = session["timer"]["phase"].as_str().unwrap();
                    have_i_prepicked = false;

                    // Check if player is assigned a position
                    for teammate in session["myTeam"].as_array().unwrap() {
                        if teammate["cellId"] == local_player_cell_id {
                            let _assigned_position = teammate["assignedPosition"].as_str().unwrap();
                            am_i_assigned = true;
                        }
                    }

                    // Check current action
                    for action_list in session["actions"].as_array().unwrap() {
                        for action in action_list.as_array().unwrap() {
                            if action["actorCellId"] == local_player_cell_id && action["isInProgress"].as_bool().unwrap() {
                                let action_type = action["type"].as_str().unwrap();
                                action_id = action["id"].as_i64().unwrap();

                                if action_type == "ban" {
                                    am_i_banning = action["isInProgress"].as_bool().unwrap();
                                }
                                if action_type == "pick" {
                                    am_i_picking = action["isInProgress"].as_bool().unwrap();
                                }

                                // Handle banning phase
                                if action_type == "ban" && phase == "BAN_PICK" && am_i_banning {
                                    while am_i_banning {
                                        if let Some(champion_id) = champions_map.get(&config.bans[ban_number]) {
                                            if let Ok(_) = lcu.patch(
                                                &format!("/lol-champ-select/v1/session/actions/{}", action_id),
                                                serde_json::json!({
                                                    "championId": champion_id,
                                                    "completed": true
                                                })
                                            ).await {
                                                println!("Successfully banned {}", config.bans[ban_number]);
                                                ban_number += 1;
                                                am_i_banning = false;
                                            }
                                        } else {
                                            ban_number = (ban_number + 1) % config.bans.len();
                                        }
                                    }
                                }

                                // Handle picking phase
                                if action_type == "pick" && phase == "BAN_PICK" && am_i_picking {
                                    while am_i_picking {
                                        if let Some(champion_id) = champions_map.get(&config.picks[pick_number]) {
                                            if let Ok(_) = lcu.patch(
                                                &format!("/lol-champ-select/v1/session/actions/{}", action_id),
                                                serde_json::json!({
                                                    "championId": champion_id,
                                                    "completed": true
                                                })
                                            ).await {
                                                println!("Successfully picked {}", config.picks[pick_number]);
                                                am_i_picking = false;
                                            }
                                        } else {
                                            pick_number = (pick_number + 1) % config.picks.len();
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Handle planning phase
                    if phase == "PLANNING" && !have_i_prepicked {
                        if let Some(champion_id) = champions_map.get(&config.picks[0]) {
                            if let Ok(_) = lcu.patch(
                                &format!("/lol-champ-select/v1/session/actions/{}", action_id),
                                serde_json::json!({
                                    "championId": champion_id,
                                    "completed": false
                                })
                            ).await {
                                println!("Pre-picked {}", config.picks[0]);
                                have_i_prepicked = true;
                            }
                        }
                    }

                    // Handle game start
                    if phase == "FINALIZATION" {
                        if let Ok(game_data) = reqwest::Client::builder()
                            .danger_accept_invalid_certs(true)
                            .build()
                            .unwrap()
                            .get("https://127.0.0.1:2999/liveclientdata/allgamedata")
                            .send()
                            .await {
                            if game_data.status().is_success() {
                                let game_json: serde_json::Value = game_data.json().await.unwrap();
                                if game_json["gameData"]["gameTime"].as_f64().unwrap() > 0.0 && !in_game {
                                    println!("Game found!");
                                    in_game = true;
                                    running.store(false, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                }

                _ => {}
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
