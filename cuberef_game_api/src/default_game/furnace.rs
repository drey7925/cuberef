use std::{any::Any, collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use cuberef_core::constants;
use cuberef_server::game_state::{
    blocks::{
        BlockTypeHandle, CustomData, ExtDataHandling, ExtendedData, ExtendedDataHolder,
        InlineContext,
    },
    client_ui::Popup,
    game_map::{TimerCallback, TimerInlineCallback, TimerSettings},
    items::{ItemStack, MaybeStack},
};
use prost::Message;

use crate::{
    blocks::BlockBuilder,
    game_builder::{Block, GameBuilder, Tex},
    include_texture_bytes,
};

use super::{
    basic_blocks::{DIRT, DIRT_WITH_GRASS, STONE},
    block_groups::BRITTLE,
    recipes::{RecipeBook, RecipeImpl, RecipeSlot},
    DefaultGameBuilder,
};

/// Furnace that's not current lit
pub const FURNACE: Block = Block("default:furnace");
/// Furnace that's lit
pub const FURNACE_ON: Block = Block("default:furnace_on");

const FURNACE_TEXTURE: Tex = Tex("default:furnace");
const FURNACE_FRONT_TEXTURE: Tex = Tex("default:furnace_front");
const FURNACE_ON_FRONT_TEXTURE: Tex = Tex("default:furnace_on_front");

/// Extended data for a furnace. One tick is 0.25 seconds.
#[derive(Clone, Message)]
pub struct FurnaceState {
    /// Number of ticks of flame left
    #[prost(uint32, tag = "1")]
    flame_left: u32,

    /// Total amount of flame provided from the last fuel that was burned
    /// This is used as the denominator for displaying a fuel usage icon (not implemented)
    #[prost(uint32, tag = "2")]
    total_flame: u32,

    /// Number of ticks of progress toward smelting the current input
    #[prost(uint32, tag = "3")]
    smelt_progress: u32,

    /// Total smelt ticks before smelting is done
    #[prost(uint32, tag = "4")]
    smelt_ticks: u32,

    /// The item being smelted. If this doesn't match the inventory on a tick, we will
    /// reset the smelt progress to 0.
    #[prost(string, tag = "5")]
    smelted_item: String,
}

struct FurnaceTimerCallback {
    recipes: Arc<RecipeBook<1, u32>>,
    fuels: Arc<RecipeBook<1, u32>>,
    furnace_off_handle: BlockTypeHandle,
    furnace_on_handle: BlockTypeHandle,
}
impl TimerInlineCallback for FurnaceTimerCallback {
    fn inline_callback(
        &self,
        coordinate: cuberef_core::coordinates::BlockCoordinate,
        missed_timers: u64,
        block_type: &mut BlockTypeHandle,
        data: &mut ExtendedDataHolder,
        ctx: &InlineContext,
    ) -> Result<()> {
        if missed_timers > 0 {
            log::warn!("Unimplemented: FurnaceTimerCallback with missed_timers");
        }
        let mut set_dirty = false;
        let extended_data = data.get_or_insert_with(|| ExtendedData {
            custom_data: Some(Box::new(FurnaceState::default())),
            inventories: hashbrown::HashMap::new(),
        });
        let state: &mut FurnaceState = match extended_data
            .custom_data
            .get_or_insert_with(|| Box::new(FurnaceState::default()))
            .as_mut()
            .downcast_mut()
        {
            Some(x) => x,
            None => {
                log::error!("Furnace data corrupted at {coordinate:?}: FurnaceTimerCallback found a different type. This should not happen.");
                return Ok(());
            }
        };

        let [input_inventory, output_inventory, fuel_inventory] = match extended_data
            .inventories
            .get_many_mut([FURNACE_INPUT, FURNACE_OUTPUT, FURNACE_FUEL])
        {
            Some(x) => x,
            None => {
                return Ok(());
            }
        };
        let input_stack = &mut input_inventory.contents_mut()[0];
        let output_stack = &mut output_inventory.contents_mut()[0];
        let fuel_stack = &mut fuel_inventory.contents_mut()[0];
        let prev_flame = state.flame_left;
        if state.flame_left == 0 {
            // Try to light the furnace, if we have both a valid fuel and a valid input
            if let Some(fueling_recipe) = self.fuels.find(ctx.items(), &[fuel_stack]) {
                if self.recipes.find(ctx.items(), &[input_stack]).is_some() {
                    if fuel_stack
                        .try_take_all(Some(fueling_recipe.slots[0].quantity()))
                        .is_some()
                    {
                        state.flame_left = fueling_recipe.metadata;
                        set_dirty = true;
                        self.set_appearance_on(block_type);
                    }
                } else {
                    if self.shut_down_furnace(block_type, state) {
                        set_dirty = true;
                    }
                }
            } else {
                if self.shut_down_furnace(block_type, state) {
                    set_dirty = true;
                }
            }
        } else {
            state.flame_left -= 1;
            if self.set_appearance_on(block_type) {
                set_dirty = true;
            }
        }

        let current_input = input_stack
            .as_ref()
            .map(|x| x.proto.item_name.as_str())
            .unwrap_or("");
        if &state.smelted_item != current_input {
            state.smelt_progress = 0;
            set_dirty = true;
            // Check if we have a recipe for this input
            let recipe = self.recipes.find(ctx.items(), &[input_stack]);
            match recipe {
                Some(recipe) => {
                    state.smelt_ticks = recipe.metadata;
                }
                None => {
                    state.smelted_item = "".to_string();
                    return Ok(());
                }
            }

            state.smelt_progress = 0;
            state.smelted_item = current_input.to_string();
        } else if prev_flame > 0 && !state.smelted_item.is_empty() {
            set_dirty = true;
            // We're still smelting the same item
            state.smelt_progress += 1;
            if state.smelt_progress >= state.smelt_ticks {
                // unwrap should be fine - if we started smelting it, we have a recipe (see above)
                let recipe = self.recipes.find(ctx.items(), &[input_stack]).unwrap();
                let can_take_input = input_stack.as_ref().is_some_and(|x| x.proto.quantity >= recipe.slots[0].quantity());
                if !can_take_input {
                    // The input item got yoinked
                    // This can only happen once recipes support taking more than one item, but we still want to be ready for that feature
                    state.smelted_item = "".to_string();
                    state.smelt_progress = 0;
                    return Ok(());
                } else if output_stack.try_merge_all(Some(recipe.result.clone())) {
                    if input_stack.try_take_all(Some(recipe.slots[0].quantity())).is_none() {
                        log::warn!("Couldn't take items we thought were in the stack. This should not happen.");
                    }

                    state.smelt_progress = 0;
                    // We should recheck the recipe; our input stack may have run out, or in the future recipes might require more than one input
                    let recipe = self.recipes.find(ctx.items(), &[input_stack]);
                    match recipe {
                        Some(recipe) => {
                            state.smelt_ticks = recipe.metadata;
                        }
                        None => {
                            state.smelted_item = "".to_string();
                            return Ok(());
                        }
                    }
                }
            }
        }
        if set_dirty {
            // todo diagnose the deadlock that happens during heavy levels of writeback traffic
            data.set_dirty();
        }
        Ok(())
    }
}

impl FurnaceTimerCallback {
    fn shut_down_furnace(&self, block_type: &mut BlockTypeHandle, state: &mut FurnaceState) -> bool {

        let dirty_from_blocktype = if block_type.equals_ignore_variant(self.furnace_on_handle) {
            *block_type = self
                .furnace_off_handle
                .with_variant(block_type.variant())
                .unwrap();
            true
        } else {
            false
        };
        let dirty_from_state = if state.smelt_progress > 0 {
            state.smelt_progress = 0;
            true
        } else {
            false
        };
        dirty_from_blocktype || dirty_from_state
    }
    fn set_appearance_on(&self, block_type: &mut BlockTypeHandle) -> bool {
        if block_type.equals_ignore_variant(self.furnace_off_handle) {
            *block_type = self
                .furnace_on_handle
                .with_variant(block_type.variant())
                .unwrap();
            true
        } else {
            false
        }
    }
}

pub(crate) fn register_furnace(game_builder: &mut DefaultGameBuilder) -> Result<()> {
    include_texture_bytes!(game_builder.inner, FURNACE_TEXTURE, "textures/furnace.png")?;
    include_texture_bytes!(
        game_builder.inner,
        FURNACE_FRONT_TEXTURE,
        "textures/furnace_front.png"
    )?;
    include_texture_bytes!(
        game_builder.inner,
        FURNACE_ON_FRONT_TEXTURE,
        "textures/furnace_on_front.png"
    )?;

    let furnace_off_handle = game_builder.inner.add_block(
        BlockBuilder::new(FURNACE)
            .add_block_group(BRITTLE)
            .set_individual_textures(
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_FRONT_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_ON_FRONT_TEXTURE,
            )
            .set_inventory_display_name("Furnace")
            .set_modifier(Box::new(|bt| {
                bt.extended_data_handling = ExtDataHandling::ServerSide;
                bt.interact_key_handler = Some(Box::new(make_furnace_popup));
                bt.dig_handler_inline = Some(Box::new(furnace_dig_handler));
                bt.deserialize_extended_data_handler = Some(Box::new(furnace_deserialize));
                bt.serialize_extended_data_handler = Some(Box::new(furnace_serialize));
            })),
    )?;
    let furnace_on_handle = game_builder.inner.add_block(
        BlockBuilder::new(FURNACE_ON)
            .add_block_group(BRITTLE)
            .set_individual_textures(
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_ON_FRONT_TEXTURE,
                FURNACE_TEXTURE,
                FURNACE_ON_FRONT_TEXTURE,
            )
            .set_inventory_display_name("Lit furnace (should not see this)")
            .set_dropped_item(FURNACE.0, 1)
            .set_modifier(Box::new(|bt| {
                bt.extended_data_handling = ExtDataHandling::ServerSide;
                bt.interact_key_handler = Some(Box::new(make_furnace_popup));
                bt.dig_handler_inline = Some(Box::new(furnace_dig_handler));
                bt.deserialize_extended_data_handler = Some(Box::new(furnace_deserialize));
                bt.serialize_extended_data_handler = Some(Box::new(furnace_serialize));
            })),
    )?;

    let timer_handler = FurnaceTimerCallback {
        recipes: game_builder.smelting_recipes.clone(),
        fuels: game_builder.smelting_fuels.clone(),
        furnace_off_handle: furnace_off_handle.0,
        furnace_on_handle: furnace_on_handle.0,
    };
    game_builder.inner.inner.add_timer(
        "default:furnace_timer",
        TimerSettings {
            interval: Duration::from_millis(250),
            shards: 32,
            spreading: 1.0,
            block_types: vec![furnace_off_handle.0, furnace_on_handle.0],
            per_block_probability: 1.0,
            ..Default::default()
        },
        TimerCallback::InlineLocked(Box::new(timer_handler)),
    );
    // testonly
    game_builder.smelting_fuels.register_recipe(RecipeImpl {
        slots: [RecipeSlot::Group("testonly_wet".to_string())],
        result: ItemStack {
            proto: Default::default(),
        },
        shapeless: false,
        metadata: 10,
    });
    game_builder.smelting_recipes.register_recipe(RecipeImpl {
        slots: [RecipeSlot::Exact(DIRT_WITH_GRASS.0.to_string())],
        result: ItemStack {
            proto: cuberef_core::protocol::items::ItemStack {
                item_name: DIRT.0.to_string(),
                quantity: 1,
                max_stack: 256,
                stackable: true,
            },
        },
        shapeless: false,
        metadata: 4,
    });
    game_builder.smelting_recipes.register_recipe(RecipeImpl {
        slots: [RecipeSlot::Exact(DIRT.0.to_string())],
        result: ItemStack {
            proto: cuberef_core::protocol::items::ItemStack {
                item_name: STONE.0.to_string(),
                quantity: 1,
                max_stack: 256,
                stackable: true,
            },
        },
        shapeless: false,
        metadata: 20,
    });
    Ok(())
}

fn furnace_deserialize(ctx: InlineContext, data: &[u8]) -> Result<Option<CustomData>> {
    let furnace = FurnaceState::decode(data)?;
    Ok(Some(Box::new(furnace)))
}
fn furnace_serialize(ctx: InlineContext, state: &CustomData) -> Result<Option<Vec<u8>>> {
    let furnace = state
        .downcast_ref::<FurnaceState>()
        .context("FurnaceState downcast failed")?;
    Ok(Some(furnace.encode_to_vec()))
}

fn furnace_dig_handler(
    ctx: InlineContext,
    bt: &mut BlockTypeHandle,
    extended_data: &mut ExtendedDataHolder,
    _dig_stack: Option<&ItemStack>,
) -> Result<Vec<ItemStack>> {
    if extended_data
        .as_ref()
        .map(|e| {
            e.inventories
                .values()
                .any(|x| x.contents().iter().any(|x| x.is_some()))
        })
        .unwrap_or(false)
    {
        // One of the inventories is non-empty. Refuse to dig the furnace.
        // TODO: Send the user a chat message warning them about this.
        // TODO: Find a way to prevent the user's item from taking wear
        //    This would entail making server changes to return a second value from the on-dig handler.
        return Ok(vec![]);
    }
    let air = ctx
        .block_types()
        .get_by_name(constants::blocks::AIR)
        .unwrap();
    extended_data.clear();
    *bt = air;
    Ok(vec![ItemStack::new(
        ctx.items().get_item(FURNACE.0).unwrap(),
        1,
    )])
}

const FURNACE_INPUT: &str = "furnace_input";
const FURNACE_FUEL: &str = "furnace_fuel";
const FURNACE_OUTPUT: &str = "furnace_output";

fn make_furnace_popup(
    ctx: cuberef_server::game_state::event::HandlerContext<'_>,
    coord: cuberef_core::coordinates::BlockCoordinate,
) -> Result<Option<Popup>> {
    match ctx.initiator() {
        cuberef_server::game_state::event::EventInitiator::Engine => Ok(None),
        cuberef_server::game_state::event::EventInitiator::Player(p) => Ok(Some(
            ctx.new_popup()
                .title("Furnace")
                .label("Input material:")
                .inventory_view_block(
                    FURNACE_INPUT,
                    (1, 1),
                    coord,
                    FURNACE_INPUT.to_string(),
                    true,
                    true,
                    false,
                )?
                .label("Fuel:")
                .inventory_view_block(
                    FURNACE_FUEL,
                    (1, 1),
                    coord,
                    FURNACE_FUEL.to_string(),
                    true,
                    true,
                    false,
                )?
                .label("Output:")
                .inventory_view_block(
                    FURNACE_OUTPUT,
                    (1, 1),
                    coord,
                    FURNACE_OUTPUT.to_string(),
                    false,
                    true,
                    false,
                )?
                .inventory_view_stored("player_inv", p.main_inventory(), true, true)?,
        )),
    }
}
