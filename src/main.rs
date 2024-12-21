use std::collections::HashMap;
use std::process::exit;
use std::time::Duration;
use tokio::time::sleep;
use serde_json::{Value, json};
use reqwest::{Client, ClientBuilder};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{StreamExt, SinkExt};
use sysinfo::{System, SystemExt, Process, ProcessExt};
use toml;
use anyhow::{Result, anyhow};
use url::Url;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct GameState {
    am_i_assigned: bool,
    am_i_picking: bool,
    am_i_banning: bool,
    pick_number: usize,
    ban_number: usize,
    phase: String,
    in_game: bool,
    have_i_prepicked: bool,
    action_id: Option<i64>,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            am_i_assigned: false,
            am_i_picking: false,
            am_i_banning: false,
            pick_number: 0,
            ban_number: 0,
            phase: String::new(),
            in_game: false,
            have_i_prepicked: false,
            action_id: None,
        }
    }
}

#[derive(Debug)]
struct LcuConnection {
    port: u16,
    auth_token: String,
    pid: u32,
}

impl LcuConnection {
    fn new(process: &Process) -> Option<Self> {
        let cmdline = process.cmd();
        if cmdline.is_empty() {
            return None;
        }
        
        let mut port = None;
        let mut auth_token = None;
        let mut pid = None;

        for arg in cmdline {
            if arg.starts_with("--app-port=") {
                port = arg.strip_prefix("--app-port=").and_then(|p| p.parse().ok());
            } else if arg.starts_with("--remoting-auth-token=") {
                auth_token = arg.strip_prefix("--remoting-auth-token=").map(String::from);
            } else if arg.starts_with("--app-pid=") {
                pid = arg.strip_prefix("--app-pid=").and_then(|p| p.parse().ok());
            }
        }

        match (port, auth_token, pid) {
            (Some(p), Some(t), Some(pid)) => Some(Self {
                port: p,
                auth_token: t,
                pid,
            }),
            _ => None
        }
    }

    fn find() -> Option<Self> {
        let sys = System::new_all();
        sys.processes().values()
            .find(|process| process.name().contains("LeagueClientUx"))
            .and_then(Self::new)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load config
    let config = std::fs::read_to_string("config.toml")?;
    let config: Value = toml::from_str(&config)?;
    
    let picks = config.get("picks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Missing or invalid picks in config"))?;
    
    let bans = config.get("bans")
        .and_then(|v| v.as_array()) 
        .ok_or_else(|| anyhow!("Missing or invalid bans in config"))?;

    if picks.is_empty() || bans.is_empty() {
        eprintln!("Picks or bans list is empty in config.toml");
        exit(1);
    }

    // Find League client
    let connection = LcuConnection::find()
        .ok_or_else(|| anyhow!("League client not found"))?;

    // Setup HTTP client
    let auth = format!("riot:{}", connection.auth_token);
    let auth = format!("Basic {}", BASE64.encode(auth));

    let client = ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("Authorization", auth.parse()?);
            headers.insert("Content-Type", "application/json".parse()?);
            headers
        })
        .build()?;

    // Get champion data
    let champions = get_champions(&client).await?;
    
    // Connect websocket
    let ws_url = format!("wss://127.0.0.1:{}", connection.port);
    let (mut ws_stream, _) = connect_async(
        Url::parse(&ws_url)?.to_string()
    ).await?;

    // Subscribe to events
    ws_stream.send(Message::Text(json!([5, "OnJsonApiEvent"]).to_string())).await?;

    let game_state = Arc::new(Mutex::new(GameState::default()));

    // Main event loop
    while let Some(msg) = ws_stream.next().await {
        let msg = msg?;
        if let Message::Text(text) = msg {
            handle_message(&client, &text, &champions, picks, bans, Arc::clone(&game_state), connection.port).await?;
        }
    }

    Ok(())
}

async fn get_champions(client: &Client) -> Result<HashMap<String, u32>> {
    let version = client.get("https://ddragon.leagueoflegends.com/api/versions.json")
        .send()
        .await?
        .json::<Vec<String>>()
        .await?;
        
    let version = &version[0];

    let champions = client.get(&format!(
        "https://ddragon.leagueoflegends.com/cdn/{}/data/en_US/champion.json",
        version
    ))
    .send()
    .await?
    .json::<Value>()
    .await?;

    let mut map = HashMap::new();
    if let Some(data) = champions["data"].as_object() {
        for (name, champion) in data {
            if let Some(key) = champion["key"].as_str() {
                if let Ok(key) = key.parse() {
                    map.insert(name.clone(), key);
                }
            }
        }
    }

    println!("len(champions_map)={}, champions_map={:?}", map.len(), map);

    Ok(map)
}

async fn handle_message(
    client: &Client,
    msg: &str,
    champions: &HashMap<String, u32>,
    picks: &[Value],
    bans: &[Value],
    game_state: Arc<Mutex<GameState>>,
    port: u16
) -> Result<()> {
    let data: Value = serde_json::from_str(msg)?;
    
    if let Some(data) = data.get(2) {
        match data["uri"].as_str() {
            Some("/lol-matchmaking/v1/ready-check") => {
                if data["data"]["state"] == "InProgress" && data["data"]["playerResponse"] == "None" {
                    client.post(format!("https://127.0.0.1:{}/lol-matchmaking/v1/ready-check/accept", port))
                        .send()
                        .await?;
                }
            }
            Some("/lol-champ-select/v1/session") => {
                let mut state = game_state.lock().unwrap();
                state.have_i_prepicked = false;

                let lobby_phase = data["data"]["timer"]["phase"].as_str().unwrap_or("");
                let local_player_cell_id = data["data"]["localPlayerCellId"].as_i64().unwrap_or(-1);

                // Track assigned position
                if let Some(my_team) = data["data"]["myTeam"].as_array() {
                    for teammate in my_team {
                        if teammate["cellId"] == local_player_cell_id {
                            let assigned_position = teammate["assignedPosition"].as_str().unwrap_or("");
                            state.am_i_assigned = true;
                            println!("Assigned position: {}", assigned_position);
                        }
                    }
                }

                // Track banned champions
                let mut banned_champions = Vec::new();
                if let Some(actions) = data["data"]["actions"].as_array() {
                    for action_list in actions {
                        if let Some(actions) = action_list.as_array() {
                            for action in actions {
                                if action["type"] == "ban" && action["completed"] == true {
                                    if let Some(champion_id) = action["championId"].as_u64() {
                                        banned_champions.push(champion_id as u32);
                                    }
                                }
                            }
                        }
                    }
                }

                // Find current action
                if let Some(actions) = data["data"]["actions"].as_array() {
                    for action_list in actions {
                        if let Some(actions) = action_list.as_array() {
                            for action in actions {
                                if action["actorCellId"] == local_player_cell_id && action["isInProgress"] == true {
                                    state.phase = action["type"].as_str().unwrap_or("").to_string();
                                    state.action_id = action["id"].as_i64();
                                    
                                    if state.phase == "ban" {
                                        state.am_i_banning = action["isInProgress"].as_bool().unwrap_or(false);
                                    }
                                    if state.phase == "pick" {
                                        state.am_i_picking = action["isInProgress"].as_bool().unwrap_or(false);
                                    }
                                }
                            }
                        }
                    }
                }

                // Handle banning phase
                if state.phase == "ban" && lobby_phase == "BAN_PICK" && state.am_i_banning {
                    while state.am_i_banning && state.ban_number < bans.len() {
                        if let Some(ban) = bans[state.ban_number].as_str() {
                            if let Some(champion_id) = champions.get(ban) {
                                let result = client.patch(format!("https://127.0.0.1:{}/lol-champ-select/v1/session/actions/{}", 
                                    port, state.action_id.unwrap()))
                                    .json(&json!({
                                        "championId": champion_id,
                                        "completed": true
                                    }))
                                    .send()
                                    .await;

                                match result {
                                    Ok(_) => {
                                        println!("Successfully banned {}", ban);
                                        break;
                                    }
                                    Err(e) => {
                                        println!("Failed to ban {}: {}", ban, e);
                                        state.ban_number += 1;
                                        if state.ban_number >= bans.len() {
                                            state.pick_number = 0;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    state.ban_number = 0;
                    state.am_i_banning = false;
                }

                // Handle picking phase
                if state.phase == "pick" && lobby_phase == "BAN_PICK" && state.am_i_picking {
                    while state.am_i_picking && state.pick_number < picks.len() {
                        if let Some(pick) = picks[state.pick_number].as_str() {
                            if let Some(champion_id) = champions.get(pick) {
                                // Check if champion is banned
                                if banned_champions.contains(champion_id) {
                                    println!("{} is banned, trying next pick", pick);
                                    state.pick_number += 1;
                                    continue;
                                }

                                let result = client.patch(format!("https://127.0.0.1:{}/lol-champ-select/v1/session/actions/{}", 
                                    port, state.action_id.unwrap()))
                                    .json(&json!({
                                        "championId": champion_id,
                                        "completed": true
                                    }))
                                    .send()
                                    .await;

                                match result {
                                    Ok(_) => {
                                        println!("Successfully picked {}", pick);
                                        break;
                                    }
                                    Err(e) => {
                                        println!("Failed to pick {}: {}", pick, e);
                                        state.pick_number += 1;
                                        if state.pick_number >= picks.len() {
                                            state.pick_number = 0;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    state.pick_number = 0;
                    state.am_i_picking = false;
                }

                // Handle planning phase
                if lobby_phase == "PLANNING" && !state.have_i_prepicked {
                    if let Some(pick) = picks[0].as_str() {
                        if let Some(champion_id) = champions.get(pick) {
                            let result = client.patch(format!("https://127.0.0.1:{}/lol-champ-select/v1/session/actions/{}", 
                                port, state.action_id.unwrap()))
                                .json(&json!({
                                    "championId": champion_id,
                                    "completed": false
                                }))
                                .send()
                                .await;

                            match result {
                                Ok(_) => {
                                    println!("Pre-picked {}", pick);
                                    state.have_i_prepicked = true;
                                }
                                Err(e) => {
                                    println!("Failed to pre-pick {}: {}", pick, e);
                                }
                            }
                        }
                    }
                }

                // Handle game start
                if lobby_phase == "FINALIZATION" {
                    let game_phase = client.get(format!("https://127.0.0.1:{}/lol-gameflow/v1/gameflow-phase", port))
                        .send()
                        .await?
                        .text()
                        .await?;

                    if game_phase == "InGame" && !state.in_game {
                        println!("Game started! Exiting champion select bot...");
                        state.in_game = true;
                        exit(69);
                    }
                    sleep(Duration::from_secs(2)).await;
                }
            }
            _ => {}
        }
    }

    Ok(())
}
