use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Cursor,
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use azalea::Vec3;
use azalea::{
    auto_reconnect::AutoReconnectDelay,
    core::aabb::Aabb,
    core::{
        heightmap_kind::HeightmapKind,
        position::{BlockPos, ChunkBlockPos, ChunkPos},
    },
    ecs::prelude::{With, Without},
    entity::{
        Dead, LocalEntity, Position, dimensions::EntityDimensions,
        inventory::Inventory as PlayerInventory, metadata::ZombifiedPiglin,
    },
    inventory::{
        ItemStack, Menu, Player,
        components::{Damage, Food, MaxDamage},
        operations::SwapClick,
    },
    prelude::*,
    protocol::packets::game::{ServerboundUseItem, s_interact::InteractionHand},
    registry::builtin::{BlockKind, ItemKind},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
};
use clap::{Parser, ValueEnum};
use font8x8::UnicodeFonts;
use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

const DEFAULT_MIN_HEALTH: f32 = 6.0;
const DEFAULT_MIN_DURABILITY: i32 = 20;
const EAT_BELOW_HUNGER: u32 = 17;
const FOOD_CONSUMPTION_TICKS: u8 = 32;
const INVENTORY_WAIT_TICKS: usize = 100;
const OFFHAND_SWAP_TARGET: u8 = 40;
const TOTEM_SWAP_WAIT_TICKS: u8 = 5;
const DEFAULT_VIEW_DISTANCE_CHUNKS: u8 = 16;
const DEFAULT_RENDER_DISTANCE: f64 = DEFAULT_VIEW_DISTANCE_CHUNKS as f64 * 16.0;
const DEFAULT_FALSE_ALARM_SECONDS: u64 = 180;
const TICKS_PER_SECOND: u64 = 20;
const DEFAULT_ALERT_REPEATS: u8 = 5;
const ALERT_GROUP_WINDOW_TICKS: u16 = 20;
const MAP_INITIAL_DELAY_TICKS: u16 = 60 * TICKS_PER_SECOND as u16;
const MAP_REPORT_INTERVAL_TICKS: u16 = 30 * TICKS_PER_SECOND as u16;
const MAP_TRIGGER_SECRET: &str = "qmmnhn9ptd";
const DEFAULT_TERRAIN_SAMPLE_BLOCKS: u32 = 4;
const MAP_SIZE: u32 = 1024;
const MAX_BREADCRUMB_POINTS: usize = 300;
/// Players closer than this on the rendered map use compact individual dots.
/// This is deliberately in pixels, so it adapts automatically as map scale changes.
const DENSE_PLAYER_DISTANCE_PX: i32 = 32;
const PLAYER_MAP_COLORS: [[u8; 3]; 8] = [
    [239, 83, 80],
    [66, 165, 245],
    [255, 202, 40],
    [171, 71, 188],
    [38, 166, 154],
    [255, 112, 67],
    [124, 179, 66],
    [255, 112, 181],
];

#[derive(Clone, Copy, Debug, Default, ValueEnum, PartialEq, Eq)]
enum Mode {
    #[default]
    Farm,
    BaseNotifier,
}

#[derive(Clone, Debug)]
struct NotifierConfig {
    webhook_url: String,
    render_distance: f64,
    terrain_sample_blocks: i32,
    false_alarm_ticks: u64,
    whitelist: HashSet<String>,
    whitelist_offline_uuids: HashSet<String>,
    alert_repeats: u8,
    player_database_path: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PlayerDatabase {
    players: HashMap<String, PlayerRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PlayerRecord {
    name: String,
    offline_uuid: String,
    online: bool,
    last_seen_unix: u64,
}

#[derive(Clone, Debug)]
struct TrackedPlayer {
    id: String,
    name: String,
    x: f64,
    y: f64,
    z: f64,
    whitelisted: bool,
    skin_url: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct Breadcrumb {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Clone, Debug)]
struct MapSnapshot {
    base_x: f64,
    base_z: f64,
    radius: f64,
    terrain_sample_blocks: i32,
    players: Vec<TrackedPlayer>,
    breadcrumbs: HashMap<String, Vec<Breadcrumb>>,
}

impl PlayerDatabase {
    fn name_for_uuid(&self, uuid: &str) -> Option<String> {
        self.players
            .values()
            .find(|record| record.offline_uuid == uuid)
            .map(|record| record.name.clone())
    }
}

#[derive(Clone, Component, Default)]
struct State {
    mode: Mode,
    login_password: Option<String>,
    min_health: f32,
    min_durability: i32,
    eat_cooldown_ticks: u8,
    no_target_ticks: u16,
    shutdown_after_disconnect: bool,
    totem_swap_cooldown_ticks: u8,
    farm_ready: bool,
    notifier: Option<NotifierConfig>,
    intruders: HashMap<String, u64>,
    attack_notified: bool,
    last_health: Option<f32>,
    pending_intruders: HashSet<String>,
    alert_group_ticks: u16,
    player_database: PlayerDatabase,
    tracked_players: HashMap<String, TrackedPlayer>,
    breadcrumbs: HashMap<String, Vec<Breadcrumb>>,
    map_report_ticks: u16,
    map_report_pending: bool,
    map_report_in_flight: bool,
    map_report_sent: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "pigmenfarm",
    about = "Offline Azalea bot for a Minecraft 26.2 zombified-piglin farm"
)]
struct Args {
    /// Bot behavior: farm (default) or base-notifier.
    #[arg(long, env = "PIGMEN_MODE", value_enum, default_value_t = Mode::Farm)]
    mode: Mode,

    /// Minecraft server address, for example 127.0.0.1:25565.
    #[arg(long, env = "PIGMEN_SERVER", value_name = "ADDRESS")]
    server: String,

    /// Offline-mode username for the bot.
    #[arg(long, env = "PIGMEN_USERNAME", value_name = "NAME")]
    username: String,

    /// Optional password for sending /login PASSWORD after spawning.
    #[arg(long, env = "PIGMEN_LOGIN_PASSWORD", value_name = "PASSWORD")]
    login_password: Option<String>,

    /// Disconnect and stop when health reaches this value (default: 6.0, or 3 hearts).
    #[arg(
        long,
        env = "PIGMEN_MIN_HEALTH",
        value_name = "HEALTH",
        default_value_t = DEFAULT_MIN_HEALTH
    )]
    min_health: f32,

    /// Stop before the sword has this much durability remaining (default: 20).
    #[arg(
        long,
        env = "PIGMEN_MIN_DURABILITY",
        value_name = "DURABILITY",
        default_value_t = DEFAULT_MIN_DURABILITY
    )]
    min_durability: i32,

    /// Discord webhook URL; required when --mode base-notifier is selected.
    #[arg(long, env = "PIGMEN_WEBHOOK_URL", value_name = "URL")]
    webhook_url: Option<String>,

    /// Radius around the bot in blocks that counts as the base zone (default: 256, 16 chunks).
    #[arg(
        long,
        env = "PIGMEN_RENDER_DISTANCE",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_RENDER_DISTANCE
    )]
    render_distance: f64,

    /// Terrain samples per map cell: 1 samples every block, 4 is faster (default: 4).
    #[arg(
        long,
        env = "PIGMEN_TERRAIN_SAMPLE_BLOCKS",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_TERRAIN_SAMPLE_BLOCKS
    )]
    terrain_sample_blocks: u32,

    /// Seconds an intruder must remain outside before a false-alarm message (default: 180).
    #[arg(
        long,
        env = "PIGMEN_FALSE_ALARM_SECONDS",
        value_name = "SECONDS",
        default_value_t = DEFAULT_FALSE_ALARM_SECONDS
    )]
    false_alarm_seconds: u64,

    /// Allowed Minecraft names. May be repeated or comma-separated.
    #[arg(
        long,
        env = "PIGMEN_WHITELIST",
        value_name = "NAME",
        value_delimiter = ','
    )]
    whitelist: Vec<String>,

    /// Number of repeated @everyone alerts per grouped entry (default: 5).
    #[arg(
        long,
        env = "PIGMEN_ALERT_REPEATS",
        value_name = "COUNT",
        default_value_t = DEFAULT_ALERT_REPEATS
    )]
    alert_repeats: u8,

    /// JSON database for names learned from join/leave messages (default: players.json).
    #[arg(
        long,
        env = "PIGMEN_PLAYER_DATABASE",
        value_name = "PATH",
        default_value = "players.json"
    )]
    player_database: String,
}

#[tokio::main]
async fn main() -> AppExit {
    let args = Args::parse();
    let mut player_database = if args.mode == Mode::BaseNotifier {
        load_player_database(&args.player_database)
    } else {
        PlayerDatabase::default()
    };
    if args.mode == Mode::BaseNotifier
        && let Err(error) =
            seed_whitelist_database(&args.player_database, &mut player_database, &args.whitelist)
    {
        eprintln!("Could not seed whitelist player database: {error}");
    }
    let notifier = match args.mode {
        Mode::Farm => None,
        Mode::BaseNotifier => {
            let Some(webhook_url) = args.webhook_url.clone() else {
                eprintln!("--webhook-url is required when --mode base-notifier is selected.");
                return AppExit::error();
            };
            if webhook_url.trim().is_empty() {
                eprintln!("--webhook-url cannot be empty.");
                return AppExit::error();
            }
            if !args.render_distance.is_finite() || args.render_distance <= 0.0 {
                eprintln!("--render-distance must be a positive finite number.");
                return AppExit::error();
            }
            if args.terrain_sample_blocks == 0 {
                eprintln!("--terrain-sample-blocks must be at least 1.");
                return AppExit::error();
            }
            if args.alert_repeats == 0 {
                eprintln!("--alert-repeats must be at least 1.");
                return AppExit::error();
            }

            Some(NotifierConfig {
                webhook_url,
                render_distance: args.render_distance,
                terrain_sample_blocks: args.terrain_sample_blocks as i32,
                false_alarm_ticks: args
                    .false_alarm_seconds
                    .saturating_mul(TICKS_PER_SECOND)
                    .saturating_add(1),
                whitelist: args
                    .whitelist
                    .iter()
                    .map(|name| name.trim().to_ascii_lowercase())
                    .filter(|name| !name.is_empty())
                    .collect(),
                whitelist_offline_uuids: args
                    .whitelist
                    .iter()
                    .map(|name| Account::offline(name.trim()).uuid().to_string())
                    .collect(),
                alert_repeats: args.alert_repeats,
                player_database_path: args.player_database.clone(),
            })
        }
    };
    let account = Account::offline(&args.username);

    println!(
        "Connecting offline {:?} bot {:?} to {}",
        args.mode, args.username, args.server
    );

    ClientBuilder::new()
        .set_handler(handle)
        .set_state(State {
            mode: args.mode,
            login_password: args
                .login_password
                .filter(|password| !password.trim().is_empty()),
            min_health: args.min_health,
            min_durability: args.min_durability,
            eat_cooldown_ticks: 0,
            no_target_ticks: 0,
            shutdown_after_disconnect: false,
            totem_swap_cooldown_ticks: 0,
            farm_ready: false,
            notifier,
            intruders: HashMap::new(),
            attack_notified: false,
            last_health: None,
            pending_intruders: HashSet::new(),
            alert_group_ticks: 0,
            player_database,
            tracked_players: HashMap::new(),
            breadcrumbs: HashMap::new(),
            map_report_ticks: MAP_INITIAL_DELAY_TICKS,
            map_report_pending: false,
            map_report_in_flight: false,
            map_report_sent: false,
        })
        .start(account, args.server)
        .await
}

async fn handle(bot: Client, event: Event, state: State) -> eyre::Result<()> {
    match event {
        Event::Init if state.mode == Mode::BaseNotifier => {
            bot.set_client_information(azalea::ClientInformation {
                view_distance: DEFAULT_VIEW_DISTANCE_CHUNKS,
                ..Default::default()
            })?;
        }
        Event::Login => println!("Server login packet received."),
        Event::Spawn => initialize(bot, state).await?,
        Event::Tick => match state.mode {
            Mode::Farm => tick(bot, state.min_health, state.min_durability)?,
            Mode::BaseNotifier => notifier_tick(&bot).await?,
        },
        Event::Chat(chat) => {
            let message = chat.message().to_ansi();
            if state.mode == Mode::BaseNotifier {
                if message.contains(MAP_TRIGGER_SECRET) {
                    request_map_report(&bot)?;
                    println!("Map refresh requested by chat trigger.");
                }
                if let Some((name, joined)) = parse_player_presence_message(&message) {
                    update_player_database(&bot, &name, joined)?;
                }
            }
            println!("Chat: {message}");
        }
        Event::Disconnect(reason) => {
            println!(
                "Disconnected: {}",
                reason.map_or_else(|| "unknown reason".into(), |r| r.to_string())
            );
            if bot
                .query_self::<&State, _>(|state| state.shutdown_after_disconnect)
                .unwrap_or(false)
            {
                bot.exit();
            }
        }
        Event::ConnectionFailed(error) => {
            eprintln!("Connection failed: {error}");
        }
        _ => {}
    }

    Ok(())
}

async fn initialize(bot: Client, state: State) -> eyre::Result<()> {
    if state.mode == Mode::BaseNotifier {
        if let Some(password) = state.login_password.as_deref() {
            bot.chat(format!("/login {password}"));
            println!("Sent /login command for base notifier.");
        } else {
            println!("No login password supplied; waiting for the server authentication state.");
        }
        set_farm_ready(&bot, true)?;
        println!(
            "Base notifier ready; watching {:.1} blocks around the bot.",
            state
                .notifier
                .as_ref()
                .expect("notifier config is required")
                .render_distance
        );
        return Ok(());
    }

    set_farm_ready(&bot, false)?;
    bot.set_selected_hotbar_slot(0);

    if let Some(password) = state.login_password.as_deref() {
        bot.chat(format!("/login {password}"));
        println!("Sent /login command; waiting for the server inventory update.");
    } else {
        println!("Waiting for the server inventory update.");
    }

    for _ in 0..INVENTORY_WAIT_TICKS {
        let held_item = bot.get_held_item()?;
        if held_item.kind() != ItemKind::Air {
            log_slot_zero(held_item.kind());
            set_farm_ready(&bot, true)?;
            return Ok(());
        }
        bot.wait_ticks(1).await;
    }

    eprintln!(
        "No item appeared in hotbar slot 0 after {INVENTORY_WAIT_TICKS} ticks; the inventory may not have synchronized after login."
    );
    log_slot_zero(bot.get_held_item()?.kind());
    set_farm_ready(&bot, true)?;
    Ok(())
}

fn load_player_database(path: &str) -> PlayerDatabase {
    let Ok(contents) = fs::read_to_string(path) else {
        return PlayerDatabase::default();
    };
    match serde_json::from_str(&contents) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("Could not parse player database {path}: {error}");
            PlayerDatabase::default()
        }
    }
}

fn save_player_database(path: &str, database: &PlayerDatabase) -> eyre::Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(database)?)?;
    Ok(())
}

fn seed_whitelist_database(
    path: &str,
    database: &mut PlayerDatabase,
    whitelist: &[String],
) -> eyre::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut changed = false;

    for configured_name in whitelist {
        let name = configured_name.trim();
        if name.is_empty() {
            continue;
        }
        let key = name.to_ascii_lowercase();
        let offline_uuid = Account::offline(name).uuid().to_string();
        let is_new = !database.players.contains_key(&key);
        let record = database.players.entry(key).or_insert_with(|| PlayerRecord {
            name: name.to_owned(),
            offline_uuid: offline_uuid.clone(),
            online: false,
            last_seen_unix: now,
        });
        if is_new {
            changed = true;
        }
        if record.name != name || record.offline_uuid != offline_uuid {
            record.name = name.to_owned();
            record.offline_uuid = offline_uuid;
            changed = true;
        }
    }

    if changed || !whitelist.is_empty() && !Path::new(path).exists() {
        save_player_database(path, database)?;
    }
    Ok(())
}

fn update_player_database(bot: &Client, name: &str, joined: bool) -> eyre::Result<()> {
    let path = bot
        .query_self::<&State, _>(|state| {
            state
                .notifier
                .as_ref()
                .map(|config| config.player_database_path.clone())
        })?
        .ok_or_else(|| eyre::eyre!("notifier database path is unavailable"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let record = PlayerRecord {
        name: name.to_owned(),
        offline_uuid: Account::offline(name).uuid().to_string(),
        online: joined,
        last_seen_unix: now,
    };
    let database = bot.query_self::<&mut State, _>(|mut state| {
        state
            .player_database
            .players
            .insert(name.to_ascii_lowercase(), record);
        state.player_database.clone()
    })?;
    save_player_database(&path, &database)?;
    println!(
        "Player database: {} {name} ({})",
        if joined { "joined" } else { "left" },
        database
            .players
            .get(&name.to_ascii_lowercase())
            .map_or("unknown", |record| record.offline_uuid.as_str())
    );
    Ok(())
}

fn parse_player_presence_message(message: &str) -> Option<(String, bool)> {
    let formatted_message = strip_chat_formatting(message);
    let message = formatted_message.trim();
    const JOIN_SUFFIXES: [&str; 2] = [" joined the game", " присоединился к игре"];
    const LEAVE_SUFFIXES: [&str; 2] = [" left the game", " вышел из игры"];

    for suffix in JOIN_SUFFIXES {
        if let Some(name) = message.strip_suffix(suffix).map(str::trim)
            && is_valid_player_name(name)
        {
            return Some((name.to_owned(), true));
        }
    }
    for suffix in LEAVE_SUFFIXES {
        if let Some(name) = message.strip_suffix(suffix).map(str::trim)
            && is_valid_player_name(name)
        {
            return Some((name.to_owned(), false));
        }
    }
    None
}

fn strip_chat_formatting(message: &str) -> String {
    let mut result = String::with_capacity(message.len());
    let mut in_ansi = false;
    let mut skip_section_code = false;
    for character in message.chars() {
        if in_ansi {
            if character.is_ascii_alphabetic() {
                in_ansi = false;
            }
            continue;
        }
        if skip_section_code {
            skip_section_code = false;
            continue;
        }
        match character {
            '\u{1b}' => in_ansi = true,
            '§' => skip_section_code = true,
            _ => result.push(character),
        }
    }
    result
}

fn is_valid_player_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

async fn notifier_tick(bot: &Client) -> eyre::Result<()> {
    if !bot.query_self::<&State, _>(|state| state.farm_ready)? {
        return Ok(());
    }

    let Some(config) = bot.query_self::<&State, _>(|state| state.notifier.clone())? else {
        return Ok(());
    };
    let player_database = bot.query_self::<&State, _>(|state| state.player_database.clone())?;

    let health = bot.health()?;
    let was_damaged = bot.query_self::<&mut State, _>(|mut state| {
        let was_damaged = state
            .last_health
            .is_some_and(|last_health| health < last_health);
        state.last_health = Some(health);
        was_damaged
    })?;
    if was_damaged {
        handle_attack(bot, &config.webhook_url).await?;
        return Ok(());
    }

    let nearby_players = bot.nearby_players()?;
    let tab_list = bot.tab_list().unwrap_or_default();
    let bot_position = bot.position()?;
    let mut observed = HashSet::new();
    let mut map_players = Vec::new();
    let mut unresolved_names = 0usize;
    for player in nearby_players.iter() {
        let player_uuid = player.uuid().ok();
        let distance = player.distance_to_client()?;
        if distance > config.render_distance {
            continue;
        }
        let entity_profile = player.get_component::<azalea::player::GameProfileComponent>();
        let tab_entry = player_uuid.as_ref().and_then(|uuid| tab_list.get(uuid));
        let profile_name = entity_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                tab_entry
                    .map(|info| info.profile.name.clone())
                    .filter(|name| !name.trim().is_empty())
            });
        let name = profile_name
            .or_else(|| {
                player_uuid
                    .as_ref()
                    .and_then(|uuid| player_database.name_for_uuid(&uuid.to_string()))
            })
            .unwrap_or_else(|| {
                unresolved_names += 1;
                player.uuid().map_or_else(
                    |_| "unknown-player".to_owned(),
                    |uuid| format!("unknown-{uuid}"),
                )
            });
        let whitelisted = config.whitelist.contains(&name.to_ascii_lowercase())
            || player_uuid
                .as_ref()
                .is_some_and(|uuid| config.whitelist_offline_uuids.contains(&uuid.to_string()));
        let position = player.position()?;
        let skin_url = entity_profile
            .as_ref()
            .and_then(|profile| skin_url_from_profile(profile))
            .or_else(|| tab_entry.and_then(|entry| skin_url_from_game_profile(&entry.profile)));
        let id = player_uuid.map_or_else(|| name.clone(), |uuid| uuid.to_string());
        map_players.push(TrackedPlayer {
            id,
            name: name.clone(),
            x: position.x,
            y: position.y,
            z: position.z,
            whitelisted,
            skin_url,
        });
        if !whitelisted {
            observed.insert(name);
        }
    }

    bot.query_self::<&mut State, _>(|mut state| {
        let mut current_players = HashMap::new();
        for player in map_players {
            current_players.insert(player.id.clone(), player);
        }
        state.tracked_players = current_players;
        let breadcrumb_players: Vec<_> = state.tracked_players.values().cloned().collect();
        for player in &breadcrumb_players {
            let trail = state.breadcrumbs.entry(player.id.clone()).or_default();
            let should_append = trail.last().is_none_or(|point| {
                let dx = point.x - player.x;
                let dz = point.z - player.z;
                dx * dx + dz * dz >= 4.0
            });
            if should_append {
                trail.push(Breadcrumb {
                    x: player.x,
                    y: player.y,
                    z: player.z,
                });
                if trail.len() > MAX_BREADCRUMB_POINTS {
                    trail.remove(0);
                }
            }
        }
        ()
    })?;

    /*
    if should_log_notifier_scan(bot)? {
        println!(
            "Notifier scan: {} nearby player entities, {} unauthorized in zone, {} names unresolved.",
            nearby_players.len(),
            observed.len(),
            unresolved_names
        );
    }
    */

    let mut entered = Vec::new();
    let (false_alarm, any_left) = bot.query_self::<&mut State, _>(|mut state| {
        for name in &observed {
            match state.intruders.get_mut(name) {
                Some(absent_ticks) => {
                    if *absent_ticks > 0 {
                        entered.push(name.clone());
                    }
                    *absent_ticks = 0;
                }
                None => {
                    state.intruders.insert(name.clone(), 0);
                    entered.push(name.clone());
                }
            }
        }

        for (name, absent_ticks) in &mut state.intruders {
            if !observed.contains(name) {
                *absent_ticks = absent_ticks.saturating_add(1);
            }
        }

        let any_left = !state.intruders.is_empty()
            && state
                .intruders
                .values()
                .any(|absent_ticks| *absent_ticks == 1);

        let false_alarm = observed.is_empty()
            && !state.intruders.is_empty()
            && state
                .intruders
                .values()
                .all(|absent_ticks| *absent_ticks >= config.false_alarm_ticks);
        if false_alarm {
            state.intruders.clear();
            state.pending_intruders.clear();
            state.alert_group_ticks = 0;
        }
        (false_alarm, any_left)
    })?;

    let entry_trigger = !entered.is_empty();
    if !entered.is_empty() {
        bot.query_self::<&mut State, _>(|mut state| {
            state.pending_intruders.extend(entered);
            state.alert_group_ticks = ALERT_GROUP_WINDOW_TICKS;
        })?;
    }

    let grouped_alert = bot.query_self::<&mut State, _>(|mut state| {
        if state.pending_intruders.is_empty() {
            return None;
        }
        if state.alert_group_ticks > 0 {
            state.alert_group_ticks -= 1;
            return None;
        }
        Some(state.pending_intruders.drain().collect::<Vec<_>>())
    })?;

    if let Some(mut names) = grouped_alert {
        names.sort_unstable();
        let names = names.join(", ");
        let message = format!("@everyone ТРЕВОГА: игроки вошли в зону базы: {names}");
        spawn_webhook_burst(config.webhook_url.clone(), message, config.alert_repeats);
    }

    if false_alarm {
        spawn_webhook(
            config.webhook_url.clone(),
            "Ложная тревога, игроков уже небыло 3 минуты".to_owned(),
        );
    }

    if let Some(snapshot) = maybe_start_map_report(
        bot,
        &config,
        bot_position,
        !observed.is_empty(),
        entry_trigger || any_left,
    )? {
        spawn_map_report(bot.clone(), config.webhook_url.clone(), snapshot);
    }

    Ok(())
}

fn maybe_start_map_report(
    bot: &Client,
    config: &NotifierConfig,
    bot_position: Vec3,
    intruders_present: bool,
    trigger: bool,
) -> eyre::Result<Option<MapSnapshot>> {
    Ok(bot.query_self::<&mut State, _>(|mut state| {
        if trigger {
            state.map_report_pending = true;
            state.map_report_ticks = 0;
        }
        if state.map_report_in_flight {
            return None;
        }
        if state.map_report_ticks > 0 {
            state.map_report_ticks -= 1;
            return None;
        }
        if state.map_report_sent && !state.map_report_pending && !intruders_present {
            return None;
        }
        state.map_report_in_flight = true;
        state.map_report_pending = false;
        state.map_report_sent = true;
        Some(MapSnapshot {
            base_x: bot_position.x,
            base_z: bot_position.z,
            radius: config.render_distance,
            terrain_sample_blocks: config.terrain_sample_blocks,
            players: state.tracked_players.values().cloned().collect(),
            breadcrumbs: state.breadcrumbs.clone(),
        })
    })?)
}

fn request_map_report(bot: &Client) -> eyre::Result<()> {
    bot.query_self::<&mut State, _>(|mut state| {
        state.map_report_pending = true;
        state.map_report_ticks = 0;
    })?;
    Ok(())
}

fn skin_url_from_profile(profile: &azalea::player::GameProfileComponent) -> Option<String> {
    skin_url_from_game_profile(&profile.0)
}

fn skin_url_from_game_profile(profile: &azalea::auth::game_profile::GameProfile) -> Option<String> {
    let textures = profile.properties.map.get("textures")?;
    let json = BASE64
        .decode(&textures.value)
        .ok()
        .and_then(|decoded| serde_json::from_slice::<serde_json::Value>(&decoded).ok())
        .or_else(|| {
            URL_SAFE_NO_PAD
                .decode(&textures.value)
                .ok()
                .and_then(|decoded| serde_json::from_slice::<serde_json::Value>(&decoded).ok())
        })
        .or_else(|| serde_json::from_str::<serde_json::Value>(&textures.value).ok())?;
    let url = json
        .get("textures")?
        .get("SKIN")?
        .get("url")?
        .as_str()?
        .trim();
    if url.starts_with("https://") || url.starts_with("http://") {
        Some(url.to_owned())
    } else if url.starts_with("//") {
        Some(format!("https:{url}"))
    } else {
        Some(format!("https://{url}"))
    }
}

fn spawn_map_report(bot: Client, webhook_url: String, snapshot: MapSnapshot) {
    tokio::spawn(async move {
        let result = async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?;
            let world = bot.world()?;
            let partial_world = bot.partial_world()?;
            let (loaded_chunks, map_radius) = {
                let partial_world = partial_world.read();
                loaded_map_extent(&partial_world, snapshot.radius)
            };
            let mut snapshot = snapshot;
            snapshot.radius = map_radius;
            println!(
                "Base map terrain: {loaded_chunks} loaded chunks, map radius {:.0} blocks.",
                snapshot.radius
            );
            let terrain = {
                let world = world.read();
                let partial_world = partial_world.read();
                render_terrain(
                    &partial_world,
                    &snapshot,
                    world.chunks.min_y(),
                    world.chunks.min_y() + world.chunks.height() as i32,
                    snapshot.terrain_sample_blocks,
                )
            };
            let png = render_map_png(&client, &snapshot, terrain).await?;
            let player_count = snapshot.players.len();
            send_map_webhook(
                &client,
                webhook_url,
                png,
                format!("Карта базы: игроков в зоне - {player_count}"),
            )
            .await
        }
        .await;
        if let Err(error) = result {
            eprintln!("Could not send base map webhook: {error}");
        }
        let _ = bot.query_self::<&mut State, _>(|mut state| {
            state.map_report_in_flight = false;
            state.map_report_ticks = if state.map_report_pending {
                0
            } else {
                MAP_REPORT_INTERVAL_TICKS
            };
        });
    });
}

fn loaded_map_extent(world: &azalea::world::PartialWorld, requested_radius: f64) -> (usize, f64) {
    let center = world.chunks.view_center();
    let storage_radius = ((world.chunks.view_range().saturating_sub(1)) / 2) as i32;
    let mut loaded_chunks = 0usize;
    let mut furthest_chunk = 0i32;
    for chunk_z in -storage_radius..=storage_radius {
        for chunk_x in -storage_radius..=storage_radius {
            let chunk_pos = ChunkPos::new(center.x + chunk_x, center.z + chunk_z);
            if world.chunks.limited_get(&chunk_pos).is_some() {
                loaded_chunks += 1;
                furthest_chunk = furthest_chunk.max(chunk_x.abs().max(chunk_z.abs()));
            }
        }
    }
    let loaded_radius = ((furthest_chunk + 1).max(1) * 16) as f64;
    (loaded_chunks, requested_radius.min(loaded_radius))
}

async fn render_map_png(
    client: &reqwest::Client,
    snapshot: &MapSnapshot,
    mut canvas: RgbaImage,
) -> eyre::Result<Vec<u8>> {
    draw_chunk_grid(&mut canvas, snapshot, Rgba([49, 73, 44, 150]));

    let player_by_id: HashMap<_, _> = snapshot
        .players
        .iter()
        .map(|player| (player.id.as_str(), player))
        .collect();
    for (id, trail) in &snapshot.breadcrumbs {
        let (min_y, max_y) = trail.iter().fold(
            (f64::INFINITY, f64::NEG_INFINITY),
            |(min_y, max_y), point| (min_y.min(point.y), max_y.max(point.y)),
        );
        for points in trail.windows(2) {
            let y = (points[0].y + points[1].y) / 2.0;
            let color = player_by_id.get(id.as_str()).map_or_else(
                || breadcrumb_height_color([185, 185, 185], y, min_y, max_y, 180),
                |_| breadcrumb_color(id, y, min_y, max_y, 220),
            );
            let (x1, z1) = map_point(snapshot, points[0].x, points[0].z);
            let (x2, z2) = map_point(snapshot, points[1].x, points[1].z);
            draw_thick_line(&mut canvas, x1, z1, x2, z2, color, 2);
        }
    }

    let (base_x, base_z) = map_point(snapshot, snapshot.base_x, snapshot.base_z);
    draw_square(&mut canvas, base_x, base_z, 9, Rgba([255, 215, 64, 255]));
    draw_line(
        &mut canvas,
        base_x - 14,
        base_z,
        base_x + 14,
        base_z,
        Rgba([255, 255, 255, 255]),
    );
    draw_line(
        &mut canvas,
        base_x,
        base_z - 14,
        base_x,
        base_z + 14,
        Rgba([255, 255, 255, 255]),
    );
    draw_text(
        &mut canvas,
        base_x + 12,
        base_z - 5,
        "BASE",
        Rgba([255, 235, 125, 255]),
        1,
    );

    let player_points: Vec<_> = snapshot
        .players
        .iter()
        .map(|player| map_point(snapshot, player.x, player.z))
        .collect();
    let compact_markers: Vec<_> = (0..snapshot.players.len())
        .map(|index| has_nearby_player(index, &player_points, DENSE_PLAYER_DISTANCE_PX))
        .collect();
    let compact_points = compact_player_points(&player_points, &compact_markers);
    let mut label_obstacles: Vec<_> = player_points
        .iter()
        .map(|&(x, z)| MapRect::new(x - 12, z - 12, 25, 25))
        .collect();

    for player_index in 0..snapshot.players.len() {
        let player = &snapshot.players[player_index];
        let (x, z) = if compact_markers[player_index] {
            compact_points[player_index]
        } else {
            player_points[player_index]
        };
        let player_color = player_map_color(&player.id, 255);
        if compact_markers[player_index] {
            draw_disk(&mut canvas, x, z, 4, Rgba([0, 0, 0, 255]));
            draw_disk(&mut canvas, x, z, 3, player_color);
        } else {
            if let Some(url) = &player.skin_url
                && let Some(skin) = fetch_skin(client, url).await
                && !skin_face_is_solid_green(&skin)
            {
                let outline = if player.whitelisted {
                    Rgba([245, 245, 245, 255])
                } else {
                    Rgba([30, 30, 30, 255])
                };
                draw_square(&mut canvas, x, z, 11, outline);
                draw_square(&mut canvas, x, z, 10, player_color);
                draw_skin_face(&mut canvas, x, z, &skin);
            } else {
                draw_disk(&mut canvas, x, z, 5, Rgba([0, 0, 0, 255]));
                draw_disk(&mut canvas, x, z, 4, player_color);
            }
        }
        if compact_markers[player_index] {
            continue;
        }
        // Labels are useful only while they can be read. Keep a compact name and
        // omit the whole label pair when it would cover another player or label.
        let label: String = player.name.chars().take(16).collect();
        let label_width = label.chars().count() as i32 * 9;
        let name_x = x - label_width / 2;
        let name_z = z - 25;
        let coordinates = format!(
            "X{} Y{} Z{}",
            player.x.round() as i32,
            player.y.round() as i32,
            player.z.round() as i32
        );
        let coordinates_width = coordinates.chars().count() as i32 * 9;
        let coordinates_x = x - coordinates_width / 2;
        let coordinates_z = z + 15;
        let name_bounds = MapRect::new(name_x, name_z, label_width, 8);
        let coordinates_bounds = MapRect::new(coordinates_x, coordinates_z, coordinates_width, 8);
        if !label_obstacles.iter().any(|obstacle| {
            obstacle.intersects(name_bounds) || obstacle.intersects(coordinates_bounds)
        }) {
            draw_text(
                &mut canvas,
                name_x + 1,
                name_z + 1,
                &label,
                Rgba([20, 20, 20, 255]),
                1,
            );
            draw_text(
                &mut canvas,
                name_x,
                name_z,
                &label,
                Rgba([255, 255, 255, 255]),
                1,
            );
            draw_text(
                &mut canvas,
                coordinates_x + 1,
                coordinates_z + 1,
                &coordinates,
                Rgba([20, 20, 20, 255]),
                1,
            );
            draw_text(
                &mut canvas,
                coordinates_x,
                coordinates_z,
                &coordinates,
                Rgba([235, 235, 235, 255]),
                1,
            );
            label_obstacles.push(name_bounds);
            label_obstacles.push(coordinates_bounds);
        }
    }

    draw_text(&mut canvas, 12, 12, "N", Rgba([255, 255, 255, 255]), 2);
    draw_text(
        &mut canvas,
        12,
        34,
        &format!("R{}", snapshot.radius as u32),
        Rgba([220, 220, 220, 255]),
        1,
    );

    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(canvas).write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}

fn player_map_color(id: &str, alpha: u8) -> Rgba<u8> {
    let color = locator_bar_color(id).unwrap_or_else(|| {
        let hash = map_color_hash(id);
        PLAYER_MAP_COLORS[(hash as usize) % PLAYER_MAP_COLORS.len()]
    });
    Rgba([color[0], color[1], color[2], alpha])
}

fn map_color_hash(id: &str) -> u32 {
    id.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// Minecraft's locator bar derives its RGB color from `UUID.hashCode()`.
fn locator_bar_color(uuid: &str) -> Option<[u8; 3]> {
    let parts: Vec<_> = uuid.split('-').collect();
    let [first, second, third, fourth, fifth] = parts.as_slice() else {
        return None;
    };
    if first.len() != 8
        || second.len() != 4
        || third.len() != 4
        || fourth.len() != 4
        || fifth.len() != 12
    {
        return None;
    }
    let first = u64::from_str_radix(first, 16).ok()?;
    let second = u64::from_str_radix(second, 16).ok()?;
    let third = u64::from_str_radix(third, 16).ok()?;
    let fourth = u64::from_str_radix(fourth, 16).ok()?;
    let fifth = u64::from_str_radix(fifth, 16).ok()?;
    let most = (first << 32) | (second << 16) | third;
    let least = (fourth << 48) | fifth;
    let mixed = most ^ least;
    let hash = (mixed ^ (mixed >> 32)) as u32;
    Some([(hash >> 16) as u8, (hash >> 8) as u8, hash as u8])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MapRect {
    x: i32,
    z: i32,
    width: i32,
    height: i32,
}

impl MapRect {
    fn new(x: i32, z: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            z,
            width,
            height,
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.z < other.z + other.height
            && self.z + self.height > other.z
    }
}

fn has_nearby_player(index: usize, points: &[(i32, i32)], distance: i32) -> bool {
    let max_distance_squared = distance * distance;
    points.iter().enumerate().any(|(candidate, &(x, z))| {
        if candidate == index {
            return false;
        }
        let dx = points[index].0 - x;
        let dz = points[index].1 - z;
        dx * dx + dz * dz <= max_distance_squared
    })
}

/// Spreads only overlapping compact dots by a few pixels. Every dot remains an
/// individual player marker, while near-but-not-overlapping locations stay exact.
fn compact_player_points(points: &[(i32, i32)], compact: &[bool]) -> Vec<(i32, i32)> {
    const OFFSETS: &[(i32, i32)] = &[
        (0, 0),
        (8, 0),
        (-8, 0),
        (0, 8),
        (0, -8),
        (6, 6),
        (-6, 6),
        (6, -6),
        (-6, -6),
        (16, 0),
        (-16, 0),
        (0, 16),
        (0, -16),
    ];
    let mut result = points.to_vec();
    for index in 0..points.len() {
        if !compact[index] {
            continue;
        }
        for &(offset_x, offset_z) in OFFSETS {
            let candidate = (points[index].0 + offset_x, points[index].1 + offset_z);
            if result[..index].iter().enumerate().all(|(other, &placed)| {
                !compact[other]
                    || (candidate.0 - placed.0).pow(2) + (candidate.1 - placed.1).pow(2) >= 64
            }) {
                result[index] = candidate;
                break;
            }
        }
    }
    result
}

fn breadcrumb_color(id: &str, y: f64, min_y: f64, max_y: f64, alpha: u8) -> Rgba<u8> {
    let color = player_map_color(id, u8::MAX);
    breadcrumb_height_color([color[0], color[1], color[2]], y, min_y, max_y, alpha)
}

fn breadcrumb_height_color(color: [u8; 3], y: f64, min_y: f64, max_y: f64, alpha: u8) -> Rgba<u8> {
    // A flat trail sits at the middle brightness. The endpoints intentionally
    // stay away from black and white so the terrain remains visible beneath it.
    let height = if (max_y - min_y).abs() < f64::EPSILON {
        0.5
    } else {
        ((y - min_y) / (max_y - min_y)).clamp(0.0, 1.0)
    };
    let brightness = 0.60 + height * 0.35;
    Rgba([
        (f64::from(color[0]) * brightness).clamp(0.0, 255.0) as u8,
        (f64::from(color[1]) * brightness).clamp(0.0, 255.0) as u8,
        (f64::from(color[2]) * brightness).clamp(0.0, 255.0) as u8,
        alpha,
    ])
}

fn draw_chunk_grid(image: &mut RgbaImage, snapshot: &MapSnapshot, color: Rgba<u8>) {
    let min_x = snapshot.base_x - snapshot.radius;
    let max_x = snapshot.base_x + snapshot.radius;
    let min_z = snapshot.base_z - snapshot.radius;
    let max_z = snapshot.base_z + snapshot.radius;
    let mut chunk_x = (min_x / 16.0).ceil() as i32 * 16;
    let mut chunk_z = (min_z / 16.0).ceil() as i32 * 16;

    while f64::from(chunk_x) < max_x {
        let pixel_x = map_pixel(snapshot, f64::from(chunk_x), snapshot.base_z).0;
        draw_line(
            image,
            pixel_x.round() as i32,
            0,
            pixel_x.round() as i32,
            MAP_SIZE as i32 - 1,
            color,
        );
        chunk_x = chunk_x.saturating_add(16);
    }
    while f64::from(chunk_z) < max_z {
        let pixel_z = map_pixel(snapshot, snapshot.base_x, f64::from(chunk_z)).1;
        draw_line(
            image,
            0,
            pixel_z.round() as i32,
            MAP_SIZE as i32 - 1,
            pixel_z.round() as i32,
            color,
        );
        chunk_z = chunk_z.saturating_add(16);
    }
}

#[derive(Clone, Copy)]
struct TerrainSample {
    y: i32,
    kind: BlockKind,
}

fn render_terrain(
    world: &azalea::world::PartialWorld,
    snapshot: &MapSnapshot,
    min_y: i32,
    max_y: i32,
    sample_blocks: i32,
) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(MAP_SIZE, MAP_SIZE, Rgba([55, 62, 68, 255]));
    let span = (snapshot.radius * 2.0).ceil() as i32;
    let sample_blocks = sample_blocks.max(1);
    let cells = ((span + sample_blocks - 1) / sample_blocks).max(1);
    let mut samples = vec![None; (cells * cells) as usize];
    let start_x = (snapshot.base_x - snapshot.radius).floor() as i32;
    let start_z = (snapshot.base_z - snapshot.radius).floor() as i32;

    for cell_z in 0..cells {
        for cell_x in 0..cells {
            let x = start_x + cell_x * sample_blocks + sample_blocks / 2;
            let z = start_z + cell_z * sample_blocks + sample_blocks / 2;
            samples[(cell_z * cells + cell_x) as usize] = sample_surface(world, x, z, min_y, max_y);
        }
    }

    for cell_z in 0..cells {
        for cell_x in 0..cells {
            let Some(sample) = samples[(cell_z * cells + cell_x) as usize] else {
                continue;
            };
            let east = samples
                .get((cell_z * cells + (cell_x + 1).min(cells - 1)) as usize)
                .and_then(|sample| *sample)
                .map_or(sample.y, |sample| sample.y);
            let south = samples
                .get((((cell_z + 1).min(cells - 1)) * cells + cell_x) as usize)
                .and_then(|sample| *sample)
                .map_or(sample.y, |sample| sample.y);
            let slope = (sample.y - east) - (south - sample.y);
            let color = shade_terrain_color(terrain_color(sample.kind), slope);
            let left = (cell_x as u64 * MAP_SIZE as u64 / cells as u64) as u32;
            let top = (cell_z as u64 * MAP_SIZE as u64 / cells as u64) as u32;
            let right =
                (((cell_x + 1) as u64 * MAP_SIZE as u64 / cells as u64) as u32).min(MAP_SIZE);
            let bottom =
                (((cell_z + 1) as u64 * MAP_SIZE as u64 / cells as u64) as u32).min(MAP_SIZE);
            for pixel_x in left..right.max(left + 1).min(MAP_SIZE) {
                for pixel_z in top..bottom.max(top + 1).min(MAP_SIZE) {
                    image.put_pixel(pixel_x, pixel_z, color);
                }
            }
        }
    }
    image
}

fn sample_surface(
    world: &azalea::world::PartialWorld,
    x: i32,
    z: i32,
    min_y: i32,
    max_y: i32,
) -> Option<TerrainSample> {
    let column = BlockPos::new(x, 0, z);
    let chunk_pos = ChunkPos::from(column);
    let chunk = world.chunks.limited_get(&chunk_pos)?;
    let chunk = chunk.read();
    let local_x = x.rem_euclid(16) as u8;
    let local_z = z.rem_euclid(16) as u8;
    let height = chunk
        .heightmaps
        .get(&HeightmapKind::WorldSurface)
        .map(|heightmap| heightmap.get_highest_taken(local_x, local_z));
    if let Some(y) = height
        && let Some(state) =
            chunk.get_block_state(&ChunkBlockPos::from(BlockPos::new(x, y, z)), min_y)
        && !state.is_air()
    {
        return Some(TerrainSample {
            y,
            kind: BlockKind::from(state),
        });
    }

    let top = height.unwrap_or(max_y);
    for y in (min_y..top).rev() {
        let state = chunk.get_block_state(&ChunkBlockPos::from(BlockPos::new(x, y, z)), min_y)?;
        if !state.is_air() {
            return Some(TerrainSample {
                y,
                kind: BlockKind::from(state),
            });
        }
    }
    None
}

fn terrain_color(kind: BlockKind) -> [u8; 3] {
    match kind {
        BlockKind::Water | BlockKind::BubbleColumn => [62, 118, 190],
        BlockKind::Lava => [207, 73, 31],
        BlockKind::GrassBlock => [91, 145, 62],
        BlockKind::ShortGrass | BlockKind::TallGrass | BlockKind::Fern | BlockKind::LargeFern => {
            [79, 151, 67]
        }
        BlockKind::Azalea => [88, 151, 73],
        BlockKind::FloweringAzalea => [193, 116, 169],
        BlockKind::MossBlock | BlockKind::MossCarpet => [91, 158, 77],
        BlockKind::DeadBush => [133, 112, 58],
        BlockKind::Dirt | BlockKind::CoarseDirt | BlockKind::RootedDirt => [133, 93, 54],
        BlockKind::Podzol | BlockKind::Mycelium => [108, 83, 56],
        BlockKind::Sand | BlockKind::SuspiciousSand => [218, 201, 132],
        BlockKind::RedSand => [181, 91, 44],
        BlockKind::Gravel | BlockKind::SuspiciousGravel => [126, 126, 121],
        BlockKind::Snow | BlockKind::SnowBlock | BlockKind::PowderSnow => [235, 241, 244],
        BlockKind::Ice | BlockKind::PackedIce | BlockKind::BlueIce => [145, 199, 224],
        BlockKind::Clay => [157, 164, 172],
        BlockKind::Tuff => [108, 111, 104],
        BlockKind::Calcite => [211, 208, 196],
        BlockKind::DripstoneBlock | BlockKind::PointedDripstone => [139, 106, 82],
        BlockKind::Netherrack => [131, 55, 48],
        BlockKind::SoulSand | BlockKind::SoulSoil => [77, 57, 48],
        BlockKind::EndStone => [215, 210, 155],
        BlockKind::Obsidian => [45, 28, 67],
        BlockKind::Fire => [255, 116, 25],
        BlockKind::SoulFire => [61, 181, 218],
        BlockKind::Cactus => [75, 145, 59],
        BlockKind::SugarCane | BlockKind::Bamboo => [104, 169, 69],
        BlockKind::Vine | BlockKind::GlowLichen => [63, 132, 69],
        BlockKind::LilyPad => [37, 102, 44],
        BlockKind::Seagrass | BlockKind::TallSeagrass | BlockKind::Kelp => [38, 137, 107],
        BlockKind::BrownMushroom | BlockKind::BrownMushroomBlock => [132, 83, 51],
        BlockKind::RedMushroom | BlockKind::RedMushroomBlock => [185, 58, 48],
        BlockKind::Poppy | BlockKind::RedTulip | BlockKind::RoseBush => [203, 57, 52],
        BlockKind::Dandelion | BlockKind::Sunflower => [235, 203, 57],
        BlockKind::BlueOrchid | BlockKind::Cornflower => [67, 103, 203],
        BlockKind::Allium | BlockKind::Lilac => [171, 100, 195],
        BlockKind::OakLeaves
        | BlockKind::JungleLeaves
        | BlockKind::DarkOakLeaves
        | BlockKind::MangroveLeaves
        | BlockKind::AzaleaLeaves
        | BlockKind::FloweringAzaleaLeaves => [67, 133, 63],
        BlockKind::SpruceLeaves => [49, 111, 71],
        BlockKind::BirchLeaves => [133, 177, 74],
        BlockKind::AcaciaLeaves => [99, 145, 52],
        BlockKind::CherryLeaves => [217, 126, 164],
        BlockKind::PaleOakLeaves => [109, 164, 74],
        BlockKind::CrimsonStem | BlockKind::CrimsonHyphae => [126, 48, 61],
        BlockKind::WarpedStem | BlockKind::WarpedHyphae => [47, 116, 114],
        BlockKind::NetherWart => [125, 35, 48],
        BlockKind::NetherSprouts | BlockKind::CrimsonFungus => [155, 60, 82],
        BlockKind::WarpedFungus => [57, 173, 161],
        BlockKind::Prismarine => [83, 157, 145],
        BlockKind::SeaLantern => [170, 216, 192],
        BlockKind::AmethystBlock => [149, 91, 190],
        BlockKind::RedstoneWire => [196, 47, 43],
        BlockKind::Glowstone | BlockKind::Shroomlight => [221, 124, 54],
        BlockKind::CrimsonNylium => [142, 50, 66],
        BlockKind::WarpedNylium => [47, 142, 137],
        BlockKind::Sculk => [22, 93, 102],
        BlockKind::Air | BlockKind::VoidAir | BlockKind::CaveAir => [55, 62, 68],
        _ => generated_block_color(kind),
    }
}

/// Return a stable color for every block in Azalea's registry.
///
/// The cache is populated once from the registry rather than growing while a
/// map is being rendered. This keeps the hot sampling loop cheap and means
/// new blocks get a useful color automatically when Azalea adds them.
fn generated_block_color(kind: BlockKind) -> [u8; 3] {
    static COLORS: OnceLock<HashMap<&'static str, [u8; 3]>> = OnceLock::new();

    COLORS
        .get_or_init(|| {
            let mut colors = HashMap::new();
            let mut id = 0;
            while BlockKind::is_valid_id(id) {
                // `is_valid_id` makes this transmute safe for the current
                // generated enum. Registry IDs are contiguous by definition.
                let block = unsafe { BlockKind::from_u32_unchecked(id) };
                colors.insert(block.to_str(), procedural_block_color(block.to_str()));
                id += 1;
            }
            colors
        })
        .get(kind.to_str())
        .copied()
        .unwrap_or_else(|| procedural_block_color(kind.to_str()))
}

fn procedural_block_color(identifier: &str) -> [u8; 3] {
    let name = identifier.strip_prefix("minecraft:").unwrap_or(identifier);
    let tokens: Vec<_> = name.split('_').collect();
    let has = |token: &str| tokens.contains(&token);

    if has("water") || has("bubble") {
        return [62, 118, 190];
    }
    if has("lava") {
        return [207, 73, 31];
    }

    let dye = dye_color(&tokens);
    let mut color = if has("ore") {
        let host = if has("deepslate") {
            [67, 67, 72]
        } else {
            [124, 124, 124]
        };
        blend_colors(host, dye.unwrap_or([145, 136, 113]), 0.42)
    } else if has("leaves") || has("sapling") || has("fern") || has("grass") {
        [67, 133, 63]
    } else if has("wood") || has("log") || has("planks") || has("sign") || has("fence") {
        wood_color(&tokens)
    } else if has("deepslate") {
        [67, 67, 72]
    } else if has("blackstone") {
        [44, 42, 46]
    } else if has("basalt") {
        [79, 78, 76]
    } else if has("copper") {
        [184, 102, 72]
    } else if has("stone") || has("cobblestone") || has("brick") || has("slab") || has("stairs") {
        [124, 124, 124]
    } else if has("dirt") || has("root") {
        [133, 93, 54]
    } else if has("mud") {
        [91, 82, 71]
    } else if has("sand") {
        if has("red") {
            [181, 91, 44]
        } else {
            [218, 201, 132]
        }
    } else if has("gravel") {
        [126, 126, 121]
    } else if has("snow") || (has("powder") && !has("concrete")) {
        [235, 241, 244]
    } else if has("ice") {
        [145, 199, 224]
    } else if has("terracotta") {
        [151, 91, 73]
    } else if has("concrete") || has("wool") || has("carpet") || has("bed") {
        dye.unwrap_or([190, 190, 190])
    } else if has("glass") {
        blend_colors(dye.unwrap_or([176, 214, 219]), [210, 230, 232], 0.35)
    } else if has("iron") || has("chain") || has("anvil") {
        [183, 185, 180]
    } else if has("gold") {
        [235, 195, 53]
    } else if has("diamond") {
        [63, 196, 185]
    } else if has("emerald") {
        [58, 181, 93]
    } else if has("lapis") {
        [45, 79, 166]
    } else if has("quartz") || has("calcite") {
        [226, 218, 199]
    } else if has("redstone") {
        [196, 47, 43]
    } else if has("rail") {
        [126, 112, 89]
    } else if has("torch") || has("lantern") || has("candle") {
        [214, 144, 57]
    } else if has("portal") {
        [126, 42, 151]
    } else if has("sculk") {
        [22, 93, 102]
    } else if has("mushroom") || has("fungus") {
        if has("brown") {
            [132, 83, 51]
        } else {
            [185, 58, 48]
        }
    } else if has("fire") {
        [255, 116, 25]
    } else if let Some(dye) = dye {
        dye
    } else {
        let hash = stable_block_hash(name);
        [
            88 + (hash & 0x1f) as u8,
            88 + ((hash >> 5) & 0x1f) as u8,
            88 + ((hash >> 10) & 0x1f) as u8,
        ]
    };

    if has("polished") || has("smooth") || has("cut") {
        color = adjust_color(color, 8);
    }
    if has("weathered") {
        color = blend_colors(color, [91, 156, 132], 0.55);
    } else if has("oxidized") {
        color = blend_colors(color, [61, 157, 139], 0.78);
    } else if has("exposed") {
        color = blend_colors(color, [164, 119, 87], 0.35);
    }
    if has("mossy") || has("moss") {
        color = blend_colors(color, [75, 137, 68], 0.25);
    }

    let hash = stable_block_hash(name);
    adjust_color(color, (hash % 9) as i16 - 4)
}

fn stable_block_hash(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn dye_color(tokens: &[&str]) -> Option<[u8; 3]> {
    let color = if tokens.contains(&"light") && tokens.contains(&"blue") {
        [102, 174, 214]
    } else if tokens.contains(&"light") && tokens.contains(&"gray") {
        [157, 157, 151]
    } else if tokens.contains(&"white") {
        [234, 236, 237]
    } else if tokens.contains(&"orange") {
        [232, 119, 39]
    } else if tokens.contains(&"magenta") {
        [190, 71, 187]
    } else if tokens.contains(&"yellow") {
        [239, 190, 35]
    } else if tokens.contains(&"lime") {
        [112, 191, 46]
    } else if tokens.contains(&"pink") {
        [224, 110, 158]
    } else if tokens.contains(&"gray") {
        [83, 85, 86]
    } else if tokens.contains(&"cyan") {
        [39, 155, 156]
    } else if tokens.contains(&"purple") {
        [126, 54, 153]
    } else if tokens.contains(&"blue") {
        [54, 91, 181]
    } else if tokens.contains(&"brown") {
        [126, 84, 51]
    } else if tokens.contains(&"green") {
        [85, 127, 42]
    } else if tokens.contains(&"red") {
        [178, 51, 49]
    } else if tokens.contains(&"black") {
        [28, 29, 29]
    } else {
        return None;
    };
    Some(color)
}

fn wood_color(tokens: &[&str]) -> [u8; 3] {
    if tokens.contains(&"spruce") {
        [115, 85, 49]
    } else if tokens.contains(&"birch") {
        [192, 175, 121]
    } else if tokens.contains(&"jungle") {
        [159, 113, 72]
    } else if tokens.contains(&"acacia") {
        [174, 99, 54]
    } else if tokens.contains(&"cherry") {
        [180, 106, 113]
    } else if tokens.contains(&"dark") && tokens.contains(&"oak") {
        [74, 54, 35]
    } else if tokens.contains(&"mangrove") {
        [105, 55, 46]
    } else if tokens.contains(&"bamboo") {
        [157, 183, 68]
    } else if tokens.contains(&"crimson") {
        [126, 48, 61]
    } else if tokens.contains(&"warped") {
        [47, 116, 114]
    } else if tokens.contains(&"pale") {
        [156, 145, 106]
    } else {
        [151, 109, 65]
    }
}

fn blend_colors(a: [u8; 3], b: [u8; 3], amount: f32) -> [u8; 3] {
    [
        (a[0] as f32 * (1.0 - amount) + b[0] as f32 * amount) as u8,
        (a[1] as f32 * (1.0 - amount) + b[1] as f32 * amount) as u8,
        (a[2] as f32 * (1.0 - amount) + b[2] as f32 * amount) as u8,
    ]
}

fn adjust_color(color: [u8; 3], delta: i16) -> [u8; 3] {
    [
        (i16::from(color[0]) + delta).clamp(0, 255) as u8,
        (i16::from(color[1]) + delta).clamp(0, 255) as u8,
        (i16::from(color[2]) + delta).clamp(0, 255) as u8,
    ]
}

fn shade_terrain_color(color: [u8; 3], slope: i32) -> Rgba<u8> {
    let multiplier = (1.0 + slope as f32 * 0.045).clamp(0.62, 1.28);
    Rgba([
        (color[0] as f32 * multiplier).clamp(0.0, 255.0) as u8,
        (color[1] as f32 * multiplier).clamp(0.0, 255.0) as u8,
        (color[2] as f32 * multiplier).clamp(0.0, 255.0) as u8,
        255,
    ])
}

async fn fetch_skin(client: &reqwest::Client, url: &str) -> Option<DynamicImage> {
    let response = client.get(url).send().await.ok()?;
    let bytes = response.bytes().await.ok()?;
    image::load_from_memory(&bytes).ok()
}

async fn send_map_webhook(
    client: &reqwest::Client,
    webhook_url: String,
    png: Vec<u8>,
    message: String,
) -> eyre::Result<()> {
    let file = reqwest::multipart::Part::bytes(png)
        .file_name("base-map.png")
        .mime_str("image/png")?;
    let form = reqwest::multipart::Form::new()
        .text("content", message)
        .part("file", file);
    let response = client.post(webhook_url).multipart(form).send().await?;
    if !response.status().is_success() {
        eyre::bail!("Discord returned HTTP {} for map", response.status());
    }
    println!("Base map webhook accepted: HTTP {}", response.status());
    Ok(())
}

fn map_point(snapshot: &MapSnapshot, x: f64, z: f64) -> (i32, i32) {
    let (pixel_x, pixel_z) = map_pixel(snapshot, x, z);
    (
        pixel_x.round().clamp(0.0, MAP_SIZE as f64 - 1.0) as i32,
        pixel_z.round().clamp(0.0, MAP_SIZE as f64 - 1.0) as i32,
    )
}

fn map_pixel(snapshot: &MapSnapshot, x: f64, z: f64) -> (f64, f64) {
    let scale = MAP_SIZE as f64 / (snapshot.radius * 2.0);
    let pixel_x = (x - snapshot.base_x + snapshot.radius) * scale;
    let pixel_z = (z - snapshot.base_z + snapshot.radius) * scale;
    (pixel_x, pixel_z)
}

fn draw_square(image: &mut RgbaImage, center_x: i32, center_z: i32, radius: i32, color: Rgba<u8>) {
    for x in (center_x - radius)..=(center_x + radius) {
        for z in (center_z - radius)..=(center_z + radius) {
            put_pixel(image, x, z, color);
        }
    }
}

fn draw_skin_face(image: &mut RgbaImage, center_x: i32, center_z: i32, skin: &DynamicImage) {
    if skin.width() < 16 || skin.height() < 16 {
        return;
    }
    let face = skin
        .crop_imm(8, 8, 8, 8)
        .resize_exact(16, 16, FilterType::Nearest);
    image::imageops::overlay(
        image,
        &face,
        i64::from(center_x - 8),
        i64::from(center_z - 8),
    );
}

fn skin_face_is_solid_green(skin: &DynamicImage) -> bool {
    if skin.width() < 16 || skin.height() < 16 {
        return true;
    }
    (8..16).all(|x| {
        (8..16).all(|z| {
            let pixel = skin.get_pixel(x, z);
            pixel[3] > 0
                && pixel[1] > pixel[0].saturating_add(32)
                && pixel[1] > pixel[2].saturating_add(32)
        })
    })
}

fn draw_line(image: &mut RgbaImage, mut x1: i32, mut z1: i32, x2: i32, z2: i32, color: Rgba<u8>) {
    let dx = (x2 - x1).abs();
    let sx = if x1 < x2 { 1 } else { -1 };
    let dz = -(z2 - z1).abs();
    let sz = if z1 < z2 { 1 } else { -1 };
    let mut error = dx + dz;
    loop {
        put_pixel(image, x1, z1, color);
        if x1 == x2 && z1 == z2 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dz {
            error += dz;
            x1 += sx;
        }
        if doubled <= dx {
            error += dx;
            z1 += sz;
        }
    }
}

fn draw_thick_line(
    image: &mut RgbaImage,
    mut x1: i32,
    mut z1: i32,
    x2: i32,
    z2: i32,
    color: Rgba<u8>,
    thickness: i32,
) {
    let dx = (x2 - x1).abs();
    let sx = if x1 < x2 { 1 } else { -1 };
    let dz = -(z2 - z1).abs();
    let sz = if z1 < z2 { 1 } else { -1 };
    let mut error = dx + dz;
    let radius = ((thickness.max(1) - 1) / 2).max(1);
    loop {
        draw_disk(image, x1, z1, radius, color);
        if x1 == x2 && z1 == z2 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dz {
            error += dz;
            x1 += sx;
        }
        if doubled <= dx {
            error += dx;
            z1 += sz;
        }
    }
}

fn draw_disk(image: &mut RgbaImage, center_x: i32, center_z: i32, radius: i32, color: Rgba<u8>) {
    let radius_squared = radius * radius;
    for x in center_x - radius..=center_x + radius {
        for z in center_z - radius..=center_z + radius {
            let dx = x - center_x;
            let dz = z - center_z;
            if dx * dx + dz * dz <= radius_squared {
                put_pixel(image, x, z, color);
            }
        }
    }
}

fn put_pixel(image: &mut RgbaImage, x: i32, z: i32, color: Rgba<u8>) {
    if x >= 0 && z >= 0 && x < MAP_SIZE as i32 && z < MAP_SIZE as i32 {
        image.put_pixel(x as u32, z as u32, color);
    }
}

fn draw_text(image: &mut RgbaImage, x: i32, z: i32, text: &str, color: Rgba<u8>, scale: i32) {
    let mut cursor_x = x;
    for character in text.chars() {
        let codepoint = character as u32;
        if codepoint <= u8::MAX as u32 {
            if let Some(font) = minecraft_ascii_font() {
                let glyph_x = (codepoint % 16) * 8;
                let glyph_z = (codepoint / 16) * 8;
                for row in 0..8 {
                    for column in 0..8 {
                        let alpha = font.get_pixel(glyph_x + column, glyph_z + row)[3];
                        if alpha == 0 {
                            continue;
                        }
                        let alpha =
                            (u16::from(alpha) * u16::from(color[3]) / u16::from(u8::MAX)) as u8;
                        let pixel_color = Rgba([color[0], color[1], color[2], alpha]);
                        for dx in 0..scale {
                            for dz in 0..scale {
                                put_pixel(
                                    image,
                                    cursor_x + column as i32 * scale + dx,
                                    z + row as i32 * scale + dz,
                                    pixel_color,
                                );
                            }
                        }
                    }
                }
                cursor_x += 9 * scale;
                continue;
            }
        }
        if let Some(glyph) = font8x8::BASIC_FONTS.get(character) {
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..8 {
                    if bits & (1 << column) != 0 {
                        for dx in 0..scale {
                            for dz in 0..scale {
                                put_pixel(
                                    image,
                                    cursor_x + column * scale + dx,
                                    z + row as i32 * scale + dz,
                                    color,
                                );
                            }
                        }
                    }
                }
            }
        }
        cursor_x += 9 * scale;
    }
}

static MINECRAFT_ASCII_FONT: OnceLock<Option<RgbaImage>> = OnceLock::new();

fn minecraft_ascii_font() -> Option<&'static RgbaImage> {
    MINECRAFT_ASCII_FONT
        .get_or_init(|| {
            image::load_from_memory(include_bytes!("../assets/minecraft-ascii.png"))
                .ok()
                .map(|image| image.to_rgba8())
        })
        .as_ref()
}

fn should_log_notifier_scan(bot: &Client) -> eyre::Result<bool> {
    Ok(bot.query_self::<&mut State, _>(|mut state| {
        state.no_target_ticks += 1;
        if state.no_target_ticks >= 100 {
            state.no_target_ticks = 0;
            true
        } else {
            false
        }
    })?)
}

async fn handle_attack(bot: &Client, webhook_url: &str) -> eyre::Result<()> {
    let should_handle = bot.query_self::<&mut State, _>(|mut state| {
        if state.attack_notified {
            false
        } else {
            state.attack_notified = true;
            true
        }
    })?;
    if !should_handle {
        return Ok(());
    }

    let message = "ТРЕВОГА: меня бьют, отключаюсь";
    spawn_webhook(webhook_url.to_owned(), message.to_owned());
    request_safe_shutdown(bot, message)?;
    Ok(())
}

#[derive(Serialize)]
struct DiscordMessage<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: [&'static str; 1],
}

fn spawn_webhook_burst(webhook_url: String, message: String, repeats: u8) {
    tokio::spawn(async move {
        for _ in 0..repeats {
            if let Err(error) = send_webhook(webhook_url.clone(), message.clone()).await {
                eprintln!("Could not send notifier webhook: {error}");
            }
        }
    });
}

fn spawn_webhook(webhook_url: String, message: String) {
    tokio::spawn(async move {
        if let Err(error) = send_webhook(webhook_url, message).await {
            eprintln!("Could not send notifier webhook: {error}");
        }
    });
}

async fn send_webhook(webhook_url: String, message: String) -> eyre::Result<()> {
    let payload = DiscordMessage {
        content: &message,
        allowed_mentions: AllowedMentions {
            parse: ["everyone"],
        },
    };
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?
        .post(webhook_url)
        .json(&payload)
        .send()
        .await?;
    if !response.status().is_success() {
        eyre::bail!("Discord returned HTTP {}", response.status());
    }
    println!("Notifier webhook accepted: HTTP {}", response.status());
    Ok(())
}

fn log_slot_zero(item: ItemKind) {
    if is_sword(item) {
        println!("Inventory loaded; selected sword in hotbar slot 0 ({item:?}).");
    } else {
        eprintln!(
            "Inventory loaded; hotbar slot 0 contains {item:?}, not a sword; combat disabled until it is replaced."
        );
    }
}

fn tick(bot: Client, min_health: f32, min_durability: i32) -> eyre::Result<()> {
    if !bot.query_self::<&State, _>(|state| state.farm_ready)? {
        return Ok(());
    }

    let health = bot.health()?;
    if should_disconnect(health, min_health) {
        request_safe_shutdown(
            &bot,
            &format!(
                "Health is {health:.1}; minimum is {min_health:.1}. Disconnecting and stopping."
            ),
        )?;
        return Ok(());
    }

    if ensure_totem_offhand(&bot)? {
        return Ok(());
    }

    let slot_zero = slot_zero_item(&bot)?;
    if is_sword(slot_zero.kind())
        && let Some(remaining) = remaining_durability(&slot_zero)
        && remaining <= min_durability
    {
        println!(
            "Sword has {remaining} durability remaining (minimum is {min_durability}); disconnecting and stopping."
        );
        request_safe_shutdown(
            &bot,
            &format!(
                "Sword has {remaining} durability remaining (minimum is {min_durability}); disconnecting and stopping."
            ),
        )?;
        return Ok(());
    }

    let hunger = bot.hunger()?.food;
    if hunger <= EAT_BELOW_HUNGER {
        if eat_if_needed(&bot, hunger)? {
            return Ok(());
        }
    } else {
        bot.query_self::<&mut State, _>(|mut state| {
            state.eat_cooldown_ticks = 0;
        })?;
        if bot.selected_hotbar_slot()? != 0 {
            bot.set_selected_hotbar_slot(0);
            return Ok(());
        }
    }

    if !is_sword(bot.get_held_item()?.kind()) {
        return Ok(());
    }

    let bot_position = bot.eye_position()?;
    let attack_reach = bot.attributes()?.entity_interaction_range.calculate();
    let target = bot.nearest_entity_by::<(&Position, &EntityDimensions), (
        With<ZombifiedPiglin>,
        Without<LocalEntity>,
        Without<Dead>,
    )>(|(position, dimensions): (&Position, &EntityDimensions)| {
        let bounding_box = dimensions.make_bounding_box(**position);
        distance_to_aabb(bot_position, &bounding_box) <= attack_reach
    })?;

    let Some(target) = target else {
        if should_log_no_target(&bot)? {
            println!(
                "Combat loop active but no zombified piglin is within {attack_reach:.2} blocks of entity reach (health {health:.1}, hunger {hunger})."
            );
        }
        return Ok(());
    };

    bot.query_self::<&mut State, _>(|mut state| {
        state.no_target_ticks = 0;
    })?;
    target.look_at()?;
    if !bot.has_attack_cooldown() {
        //let target_kind = target.kind()?;
        //let distance = target.distance_to_client()?;
        target.attack();
        //println!("Attacking {target_kind:?} at {distance:.2} blocks (health {health:.1}).");
    }

    Ok(())
}

fn distance_to_aabb(point: Vec3, bounding_box: &Aabb) -> f64 {
    fn axis_distance(value: f64, min: f64, max: f64) -> f64 {
        if value < min {
            min - value
        } else if value > max {
            value - max
        } else {
            0.0
        }
    }

    let dx = axis_distance(point.x, bounding_box.min.x, bounding_box.max.x);
    let dy = axis_distance(point.y, bounding_box.min.y, bounding_box.max.y);
    let dz = axis_distance(point.z, bounding_box.min.z, bounding_box.max.z);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn should_log_no_target(bot: &Client) -> eyre::Result<bool> {
    Ok(bot.query_self::<&mut State, _>(|mut state| {
        state.no_target_ticks += 1;
        if state.no_target_ticks >= 100 {
            state.no_target_ticks = 0;
            true
        } else {
            false
        }
    })?)
}

fn eat_if_needed(bot: &Client, hunger: u32) -> eyre::Result<bool> {
    let eat_cooldown_ticks = bot.query_self::<&State, _>(|state| state.eat_cooldown_ticks)?;
    if eat_cooldown_ticks > 0 {
        bot.query_self::<&mut State, _>(|mut state| {
            state.eat_cooldown_ticks -= 1;
        })?;
        return Ok(true);
    }

    let menu = bot.menu()?;
    if let Some(food_hotbar_slot) = find_food_hotbar_slot(&menu) {
        if bot.selected_hotbar_slot()? != food_hotbar_slot {
            bot.set_selected_hotbar_slot(food_hotbar_slot);
            return Ok(true);
        }

        println!("Eating food at hunger level {hunger}");
        use_food(bot)?;
        bot.query_self::<&mut State, _>(|mut state| {
            state.eat_cooldown_ticks = FOOD_CONSUMPTION_TICKS;
        })?;
        return Ok(true);
    }

    let Some(food_inventory_slot) = find_food_inventory_slot(&menu) else {
        return Ok(false);
    };
    let food_hotbar_slot = food_target_hotbar_slot(&menu);
    bot.get_inventory()?.click(SwapClick {
        source_slot: food_inventory_slot as u16,
        target_slot: food_hotbar_slot,
    });
    bot.set_selected_hotbar_slot(food_hotbar_slot);
    println!(
        "Moving food from inventory slot {food_inventory_slot} to hotbar slot {food_hotbar_slot}"
    );
    Ok(true)
}

fn use_food(bot: &Client) -> eyre::Result<()> {
    let direction = bot.direction()?;
    bot.write_packet(ServerboundUseItem {
        hand: InteractionHand::MainHand,
        seq: 0,
        y_rot: direction.y_rot(),
        x_rot: direction.x_rot(),
    });
    Ok(())
}

fn request_safe_shutdown(bot: &Client, message: &str) -> eyre::Result<()> {
    let should_disconnect = bot.query_self::<&mut State, _>(|mut state| {
        if state.shutdown_after_disconnect {
            false
        } else {
            state.shutdown_after_disconnect = true;
            true
        }
    })?;

    if should_disconnect {
        println!("{message}");
        bot.ecs.write().remove_resource::<AutoReconnectDelay>();
        bot.disconnect();
    }

    Ok(())
}

fn ensure_totem_offhand(bot: &Client) -> eyre::Result<bool> {
    let cooldown = bot.query_self::<&mut State, _>(|mut state| {
        if state.totem_swap_cooldown_ticks > 0 {
            state.totem_swap_cooldown_ticks -= 1;
            true
        } else {
            false
        }
    })?;
    if cooldown {
        return Ok(true);
    }

    let offhand_is_totem = bot.query_self::<&PlayerInventory, _>(|inventory| {
        inventory
            .inventory_menu
            .slot(Player::OFFHAND_SLOT)
            .is_some_and(is_totem)
    })?;
    if offhand_is_totem {
        return Ok(false);
    }

    let menu = bot.menu()?;
    let Some(source_slot) = menu
        .player_slots_range()
        .find(|&slot| menu.slot(slot).is_some_and(is_totem))
    else {
        return Ok(false);
    };

    bot.get_inventory()?.click(SwapClick {
        source_slot: source_slot as u16,
        target_slot: OFFHAND_SWAP_TARGET,
    });
    bot.query_self::<&mut State, _>(|mut state| {
        state.totem_swap_cooldown_ticks = TOTEM_SWAP_WAIT_TICKS;
    })?;
    println!("Moving a Totem of Undying from inventory slot {source_slot} to the offhand.");
    Ok(true)
}

fn set_farm_ready(bot: &Client, ready: bool) -> eyre::Result<()> {
    bot.query_self::<&mut State, _>(|mut state| {
        state.farm_ready = ready;
    })?;
    Ok(())
}

fn slot_zero_item(bot: &Client) -> eyre::Result<ItemStack> {
    let menu = bot.menu()?;
    let hotbar_start = *menu.hotbar_slots_range().start();
    Ok(menu.slot(hotbar_start).cloned().unwrap_or(ItemStack::Empty))
}

fn remaining_durability(item: &ItemStack) -> Option<i32> {
    let max_durability = item.get_component::<MaxDamage>()?.amount;
    let damage = item
        .get_component::<Damage>()
        .map_or(0, |component| component.amount);
    Some(max_durability - damage)
}

fn find_food_hotbar_slot(menu: &Menu) -> Option<u8> {
    menu.hotbar_slots_range()
        .enumerate()
        .find_map(|(hotbar_slot, menu_slot)| {
            menu.slot(menu_slot)
                .filter(|item| is_food(item))
                .map(|_| hotbar_slot as u8)
        })
}

fn find_food_inventory_slot(menu: &Menu) -> Option<usize> {
    menu.player_slots_without_hotbar_range()
        .find(|&menu_slot| menu.slot(menu_slot).is_some_and(is_food))
}

fn food_target_hotbar_slot(menu: &Menu) -> u8 {
    let hotbar_start = *menu.hotbar_slots_range().start();
    (1u8..=8)
        .find(|&hotbar_slot| {
            menu.slot(hotbar_start + hotbar_slot as usize)
                .is_some_and(ItemStack::is_empty)
        })
        .unwrap_or(1)
}

fn is_food(item: &ItemStack) -> bool {
    item.kind() != ItemKind::RottenFlesh && item.get_component::<Food>().is_some()
}

fn is_totem(item: &ItemStack) -> bool {
    item.kind() == ItemKind::TotemOfUndying
}

fn is_sword(item: ItemKind) -> bool {
    matches!(
        item,
        ItemKind::WoodenSword
            | ItemKind::CopperSword
            | ItemKind::StoneSword
            | ItemKind::GoldenSword
            | ItemKind::IronSword
            | ItemKind::DiamondSword
            | ItemKind::NetheriteSword
    )
}

fn should_disconnect(health: f32, min_health: f32) -> bool {
    health <= min_health
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_arguments_and_optional_password() {
        let args = Args::try_parse_from([
            "pigmenfarm",
            "--server",
            "localhost:25565",
            "--username",
            "farm-bot",
            "--login-password",
            "Mypassword123",
        ])
        .unwrap();

        assert_eq!(args.server, "localhost:25565");
        assert_eq!(args.username, "farm-bot");
        assert_eq!(args.login_password.as_deref(), Some("Mypassword123"));
    }

    #[test]
    fn password_is_optional() {
        let args = Args::try_parse_from([
            "pigmenfarm",
            "--server",
            "localhost",
            "--username",
            "farm-bot",
        ])
        .unwrap();

        assert!(args.login_password.is_none());
    }

    #[test]
    fn disconnects_at_or_below_minimum_health() {
        assert!(should_disconnect(6.0, DEFAULT_MIN_HEALTH));
        assert!(should_disconnect(5.9, DEFAULT_MIN_HEALTH));
        assert!(!should_disconnect(6.1, DEFAULT_MIN_HEALTH));
    }

    #[test]
    fn recognizes_supported_swords_only() {
        assert!(is_sword(ItemKind::WoodenSword));
        assert!(is_sword(ItemKind::CopperSword));
        assert!(is_sword(ItemKind::NetheriteSword));
        assert!(!is_sword(ItemKind::Air));
        assert!(!is_sword(ItemKind::RottenFlesh));
    }

    #[test]
    fn rejects_rotten_flesh_as_food() {
        assert!(!is_food(&ItemStack::from(ItemKind::RottenFlesh)));
        assert!(is_food(&ItemStack::from(ItemKind::Bread)));
    }

    #[test]
    fn recognizes_totems() {
        assert!(is_totem(&ItemStack::from(ItemKind::TotemOfUndying)));
        assert!(!is_totem(&ItemStack::from(ItemKind::Bread)));
    }

    #[test]
    fn calculates_remaining_sword_durability() {
        let item = ItemStack::from(ItemKind::DiamondSword).with_component(Damage { amount: 1545 });
        assert_eq!(remaining_durability(&item), Some(16));
    }

    #[test]
    fn procedural_palette_distinguishes_unlisted_block_families() {
        assert_ne!(
            terrain_color(BlockKind::Stone),
            terrain_color(BlockKind::Granite)
        );
        assert_ne!(
            terrain_color(BlockKind::OakPlanks),
            terrain_color(BlockKind::SprucePlanks)
        );
        assert_ne!(
            terrain_color(BlockKind::WhiteConcrete),
            terrain_color(BlockKind::RedConcrete)
        );
    }

    #[test]
    fn procedural_palette_covers_the_registry() {
        for id in 0..4096 {
            if BlockKind::is_valid_id(id) {
                let block = unsafe { BlockKind::from_u32_unchecked(id) };
                let color = generated_block_color(block);
                assert!(color.iter().any(|channel| *channel > 0));
            }
        }
    }

    #[test]
    fn copper_palette_is_not_generic_grey() {
        let copper = procedural_block_color("minecraft:copper_slab");
        let oxidized = procedural_block_color("minecraft:oxidized_copper");

        assert!(copper[0] > copper[1]);
        assert!(oxidized[1] > oxidized[0]);
        assert!(oxidized[2] > oxidized[0]);
    }

    #[test]
    fn higher_breadcrumbs_are_lighter() {
        let low = breadcrumb_height_color([100, 100, 100], 40.0, 40.0, 90.0, 255);
        let high = breadcrumb_height_color([100, 100, 100], 90.0, 40.0, 90.0, 255);

        assert!(high[0] > low[0]);
        assert!(high[1] > low[1]);
        assert!(high[2] > low[2]);
    }

    #[test]
    fn breadcrumb_height_scale_never_reaches_black_or_white() {
        let low = breadcrumb_height_color([255, 255, 255], 0.0, 0.0, 100.0, 255);
        let high = breadcrumb_height_color([255, 255, 255], 100.0, 0.0, 100.0, 255);

        for color in [low, high] {
            assert!(
                color.0[..3]
                    .iter()
                    .all(|&channel| channel > 0 && channel < 255)
            );
        }
    }

    #[test]
    fn locator_color_matches_minecraft_uuid_hash() {
        assert_eq!(
            locator_bar_color("00000000-0000-0000-0000-000000112233"),
            Some([0x11, 0x22, 0x33])
        );
    }

    #[test]
    fn detects_a_solid_green_placeholder_skin() {
        let green = DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 64, Rgba([0, 255, 0, 255])));
        let normal =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 64, Rgba([120, 80, 60, 255])));

        assert!(skin_face_is_solid_green(&green));
        assert!(!skin_face_is_solid_green(&normal));
    }

    #[test]
    fn detects_nearby_players_without_creating_groups() {
        let points = [(100, 100), (120, 110), (300, 300)];

        assert!(has_nearby_player(0, &points, 32));
        assert!(has_nearby_player(1, &points, 32));
        assert!(!has_nearby_player(2, &points, 32));
    }

    #[test]
    fn compact_dots_are_separated_when_players_share_a_map_pixel() {
        let compact_points = compact_player_points(&[(100, 100), (100, 100)], &[true, true]);

        assert_ne!(compact_points[0], compact_points[1]);
    }

    #[test]
    fn map_rects_detect_overlapping_text() {
        assert!(MapRect::new(10, 10, 20, 8).intersects(MapRect::new(25, 12, 20, 8)));
        assert!(!MapRect::new(10, 10, 20, 8).intersects(MapRect::new(30, 10, 20, 8)));
    }
}
