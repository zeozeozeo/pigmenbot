use azalea::{
    ecs::prelude::{With, Without},
    entity::{Dead, LocalEntity, Position, metadata::ZombifiedPiglin},
    inventory::{ItemStack, Menu, components::Food, operations::SwapClick},
    prelude::*,
    registry::builtin::ItemKind,
};
use clap::Parser;

const DEFAULT_MIN_HEALTH: f32 = 6.0;
const EAT_BELOW_HUNGER: u32 = 17;
const FOOD_CONSUMPTION_TICKS: u8 = 32;
const INVENTORY_WAIT_TICKS: usize = 100;

#[derive(Clone, Component, Default)]
struct State {
    login_password: Option<String>,
    min_health: f32,
    eat_cooldown_ticks: u8,
    no_target_ticks: u16,
}

#[derive(Debug, Parser)]
#[command(
    name = "pigmenfarm",
    about = "Offline Azalea bot for a Minecraft 26.2 zombified-piglin farm"
)]
struct Args {
    /// Minecraft server address, for example 127.0.0.1:25565.
    #[arg(long, value_name = "ADDRESS")]
    server: String,

    /// Offline-mode username for the bot.
    #[arg(long, value_name = "NAME")]
    username: String,

    /// Optional password for sending /login PASSWORD after spawning.
    #[arg(long, value_name = "PASSWORD")]
    login_password: Option<String>,

    /// Disconnect and stop when health reaches this value (default: 6.0, or 3 hearts).
    #[arg(long, value_name = "HEALTH", default_value_t = DEFAULT_MIN_HEALTH)]
    min_health: f32,
}

#[tokio::main]
async fn main() -> AppExit {
    let args = Args::parse();
    let account = Account::offline(&args.username);

    println!(
        "Connecting offline bot {:?} to {}",
        args.username, args.server
    );

    ClientBuilder::new()
        .set_handler(handle)
        .set_state(State {
            login_password: args.login_password,
            min_health: args.min_health,
            eat_cooldown_ticks: 0,
            no_target_ticks: 0,
        })
        .start(account, args.server)
        .await
}

async fn handle(bot: Client, event: Event, state: State) -> eyre::Result<()> {
    match event {
        Event::Login => println!("Server login packet received."),
        Event::Spawn => initialize(bot, state).await?,
        Event::Tick => tick(bot, state.min_health)?,
        Event::Chat(chat) => println!("Chat: {}", chat.message().to_ansi()),
        Event::Disconnect(reason) => {
            println!(
                "Disconnected: {}",
                reason.map_or_else(|| "unknown reason".into(), |r| r.to_string())
            );
        }
        Event::ConnectionFailed(error) => {
            eprintln!("Connection failed: {error}");
        }
        _ => {}
    }

    Ok(())
}

async fn initialize(bot: Client, state: State) -> eyre::Result<()> {
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
            return Ok(());
        }
        bot.wait_ticks(1).await;
    }

    eprintln!(
        "No item appeared in hotbar slot 0 after {INVENTORY_WAIT_TICKS} ticks; the inventory may not have synchronized after login."
    );
    log_slot_zero(bot.get_held_item()?.kind());
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

fn tick(bot: Client, min_health: f32) -> eyre::Result<()> {
    let health = bot.health()?;
    if should_disconnect(health, min_health) {
        println!("Health is {health:.1}; minimum is {min_health:.1}. Disconnecting and stopping.");
        bot.disconnect();
        bot.exit();
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
    let target = bot.nearest_entity_by::<&Position, (
        With<ZombifiedPiglin>,
        Without<LocalEntity>,
        Without<Dead>,
    )>(|position: &Position| bot_position.distance_to(**position) < 3.0)?;

    let Some(target) = target else {
        if should_log_no_target(&bot)? {
            println!(
                "Combat loop active but no zombified piglin is within 3 blocks (health {health:.1}, hunger {hunger})."
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
        bot.start_use_item();
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
}
