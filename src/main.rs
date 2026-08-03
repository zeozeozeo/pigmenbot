use azalea::Vec3;
use azalea::{
    auto_reconnect::AutoReconnectDelay,
    core::aabb::Aabb,
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
    registry::builtin::ItemKind,
};
use clap::Parser;

const DEFAULT_MIN_HEALTH: f32 = 6.0;
const DEFAULT_MIN_DURABILITY: i32 = 20;
const EAT_BELOW_HUNGER: u32 = 17;
const FOOD_CONSUMPTION_TICKS: u8 = 32;
const INVENTORY_WAIT_TICKS: usize = 100;
const OFFHAND_SWAP_TARGET: u8 = 40;
const TOTEM_SWAP_WAIT_TICKS: u8 = 5;

#[derive(Clone, Component, Default)]
struct State {
    login_password: Option<String>,
    min_health: f32,
    min_durability: i32,
    eat_cooldown_ticks: u8,
    no_target_ticks: u16,
    shutdown_after_disconnect: bool,
    totem_swap_cooldown_ticks: u8,
    farm_ready: bool,
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

    /// Stop before the sword has this much durability remaining (default: 20).
    #[arg(long, value_name = "DURABILITY", default_value_t = DEFAULT_MIN_DURABILITY)]
    min_durability: i32,
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
            min_durability: args.min_durability,
            eat_cooldown_ticks: 0,
            no_target_ticks: 0,
            shutdown_after_disconnect: false,
            totem_swap_cooldown_ticks: 0,
            farm_ready: false,
        })
        .start(account, args.server)
        .await
}

async fn handle(bot: Client, event: Event, state: State) -> eyre::Result<()> {
    match event {
        Event::Login => println!("Server login packet received."),
        Event::Spawn => initialize(bot, state).await?,
        Event::Tick => tick(bot, state.min_health, state.min_durability)?,
        Event::Chat(chat) => println!("Chat: {}", chat.message().to_ansi()),
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
}
