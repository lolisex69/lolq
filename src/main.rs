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
        println!("Attempting to find League Client process...");
        
        // Find League process and extract port/password
        let output = match Command::new("powershell")
            .args(&[
                "Get-WmiObject",
                "Win32_Process",
                "-Filter",
                "name='LeagueClientUx.exe'",
                "|",
                "Select-Object",
                "CommandLine",
            ])
            .output() {
                Ok(output) => output,
                Err(e) => {
                    println!("Failed to execute PowerShell command: {}", e);
                    return None;
                }
            };

        let output_str = String::from_utf8_lossy(&output.stdout);
        println!("PowerShell output: {}", output_str);
        
        if output_str.trim().is_empty() {
            println!("No League Client process found. Is the client running?");
            return None;
        }
        
        let port_re = Regex::new(r"--app-port=(\d+)").unwrap();
        let auth_re = Regex::new(r"--remoting-auth-token=([a-zA-Z0-9_-]+)").unwrap();

        let port = match port_re.captures(&output_str) {
            Some(cap) => match cap.get(1) {
                Some(p) => p.as_str(),
                None => {
                    println!("Failed to extract port from client arguments");
                    return None;
                }
            },
            None => {
                println!("Could not find app-port in client arguments");
                return None;
            }
        };

        let auth = match auth_re.captures(&output_str) {
            Some(cap) => match cap.get(1) {
                Some(a) => a.as_str(),
                None => {
                    println!("Failed to extract auth token from client arguments");
                    return None;
                }
            },
            None => {
                println!("Could not find auth token in client arguments");
                return None;
            }
        };

        println!("Found League Client - Port: {}, Auth token: {}", port, auth);

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
        println!("Making GET request to {}{}", self.base_url, path);
        let response = self.client
            .get(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth))
            .send()
            .await?;
            
        println!("Response status: {}", response.status());
        response.json().await
    }

    async fn post(&self, path: &str, json: Option<serde_json::Value>) -> Result<(), reqwest::Error> {
        println!("Making POST request to {}{}", self.base_url, path);
        if let Some(ref j) = json {
            println!("Request body: {}", j);
        }
        
        let mut req = self.client
            .post(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth));

        if let Some(json) = json {
            req = req.json(&json);
        }

        let response = req.send().await?;
        println!("Response status: {}", response.status());
        Ok(())
    }

    async fn patch(&self, path: &str, json: serde_json::Value) -> Result<(), reqwest::Error> {
        println!("Making PATCH request to {}{}", self.base_url, path);
        println!("Request body: {}", json);
        
        let response = self.client
            .patch(format!("{}{}", self.base_url, path))
            .basic_auth("riot", Some(&self.auth))
            .json(&json)
            .send()
            .await?;
            
        println!("Response status: {}", response.status());
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    println!("Starting League Queue Assistant...");
    
    // Load config
    println!("Loading config from config.toml...");
    let config: Config = match fs::read_to_string("config.toml") {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(config) => {
                println!("Loaded config successfully:");
                println!("Picks: {:?}", config.picks);
                println!("Bans: {:?}", config.bans);
                config
            },
            Err(e) => {
                println!("Failed to parse config.toml: {}", e);
                return;
            }
        },
        Err(e) => {
            println!("Failed to read config.toml: {}", e);
            return;
        }
    };

    println!("Connecting to League Client...");
    let lcu = match LcuClient::new().await {
        Some(client) => client,
        None => {
            println!("Failed to connect to League Client. Make sure the client is running and try again.");
            return;
        }
    };

    // Get summoner data
    println!("Fetching summoner data...");
    let summoner: SummonerData = match lcu.get("/lol-summoner/v1/current-summoner").await {
        Ok(data) => {
            println!("Got summoner data - ID: {}", data.summoner_id);
            data
        },
        Err(e) => {
            println!("Failed to get summoner data: {}", e);
            return;
        }
    };

    // Get champion data
    println!("Fetching champion data...");
    let champions: Vec<ChampionData> = match lcu.get(&format!(
        "/lol-champions/v1/inventories/{}/champions-minimal",
        summoner.summoner_id
    )).await {
        Ok(data) => {
            println!("Successfully retrieved data for {} champions", data.len());
            data
        },
        Err(e) => {
            println!("Failed to get champion data: {}", e);
            return;
        }
    };

    let champions_map: HashMap<String, i32> = champions
        .into_iter()
        .map(|c| (c.name, c.id))
        .collect();

    println!("Loaded {} champions into lookup map", champions_map.len());

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    let ws_url = format!("wss://127.0.0.1:{}", lcu.base_url.split(":").nth(2).unwrap());
    println!("Connecting to websocket at {}", ws_url);
    
    let (mut ws_stream, _) = match connect_async(ws_url).await {
        Ok(conn) => {
            println!("Successfully connected to LCU websocket");
            conn
        },
        Err(e) => {
            println!("Failed to connect to LCU websocket: {}", e);
            return;
        }
    };

    let mut pick_number = 0;
    let mut ban_number = 0;
    let mut am_i_assigned = false;
    let mut am_i_picking = false;
    let mut am_i_banning = false;
    let mut have_i_prepicked = false;
    let mut in_game = false;
    let mut action_id = 0;

    println!("Entering main event loop...");
    
    while running.load(Ordering::SeqCst) {
        if let Some(msg) = ws_stream.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    println!("Error receiving websocket message: {}", e);
                    continue;
                }
            };
            
            let data: serde_json::Value = match serde_json::from_str(&msg.to_string()) {
                Ok(d) => d,
                Err(e) => {
                    println!("Failed to parse websocket message: {}", e);
                    continue;
                }
            };

            match data["uri"].as_str() {
                Some("/lol-matchmaking/v1/ready-check") => {
                    println!("Received ready check event");
                    if data["data"]["state"] == "InProgress" && data["data"]["playerResponse"] == "None" {
                        println!("Accepting ready check...");
                        if let Err(e) = lcu.post("/lol-matchmaking/v1/ready-check/accept", None).await {
                            println!("Failed to accept ready check: {}", e);
                        }
                    }
                }

                Some("/lol-champ-select/v1/session") => {
                    let session = &data["data"];
                    let local_player_cell_id = session["localPlayerCellId"].as_i64().unwrap();
                    let phase = session["timer"]["phase"].as_str().unwrap();
                    println!("Champion select phase: {}", phase);
                    have_i_prepicked = false;

                    // Check if player is assigned a position
                    for teammate in session["myTeam"].as_array().unwrap() {
                        if teammate["cellId"] == local_player_cell_id {
                            let assigned_position = teammate["assignedPosition"].as_str().unwrap();
                            println!("Assigned position: {}", assigned_position);
                            am_i_assigned = true;
                        }
                    }

                    // Check current action
                    for action_list in session["actions"].as_array().unwrap() {
                        for action in action_list.as_array().unwrap() {
                            if action["actorCellId"] == local_player_cell_id && action["isInProgress"].as_bool().unwrap() {
                                let action_type = action["type"].as_str().unwrap();
                                action_id = action["id"].as_i64().unwrap();
                                println!("Current action: {} (ID: {})", action_type, action_id);

                                if action_type == "ban" {
                                    am_i_banning = action["isInProgress"].as_bool().unwrap();
                                }
                                if action_type == "pick" {
                                    am_i_picking = action["isInProgress"].as_bool().unwrap();
                                }

                                // Handle banning phase
                                if action_type == "ban" && phase == "BAN_PICK" && am_i_banning {
                                    while am_i_banning {
                                        println!("Attempting to ban {} (attempt {})", config.bans[ban_number], ban_number + 1);
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
                                            println!("Champion {} not available, trying next ban", config.bans[ban_number]);
                                            ban_number = (ban_number + 1) % config.bans.len();
                                        }
                                    }
                                }

                                // Handle picking phase
                                if action_type == "pick" && phase == "BAN_PICK" && am_i_picking {
                                    while am_i_picking {
                                        println!("Attempting to pick {} (attempt {})", config.picks[pick_number], pick_number + 1);
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
                                            println!("Champion {} not available, trying next pick", config.picks[pick_number]);
                                            pick_number = (pick_number + 1) % config.picks.len();
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Handle planning phase
                    if phase == "PLANNING" && !have_i_prepicked {
                        println!("Planning phase - attempting to pre-pick {}", config.picks[0]);
                        if let Some(champion_id) = champions_map.get(&config.picks[0]) {
                            if let Ok(_) = lcu.patch(
                                &format!("/lol-champ-select/v1/session/actions/{}", action_id),
                                serde_json::json!({
                                    "championId": champion_id,
                                    "completed": false
                                })
                            ).await {
                                println!("Successfully pre-picked {}", config.picks[0]);
                                have_i_prepicked = true;
                            }
                        }
                    }

                    // Handle game start
                    if phase == "FINALIZATION" {
                        println!("Finalization phase - checking if game has started...");
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
                                    println!("Game has started! Shutting down queue assistant.");
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
