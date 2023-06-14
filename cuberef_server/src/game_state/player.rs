// Copyright 2023 drey7925
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{hash_map::Entry, HashMap},
    mem::swap,
    ops::{Deref, DerefMut},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use cgmath::{vec3, Vector3, Zero};
use cuberef_core::{coordinates::PlayerPositionUpdate, protocol::players::StoredPlayer};

use parking_lot::Mutex;
use prost::Message;
use tokio::{select, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::database::database_engine::{GameDatabase, KeySpace};

use super::{
    client_ui::Popup,
    inventory::{InventoryKey, InventoryView, InventoryViewId},
    GameState,
};

pub struct Player {
    // Player's in-game name
    pub(crate) name: String,
    // Player's inventory (immutable key that can be used to access mutable inventory)
    // The main inventory is a bit special - it's drawn in the HUD and used for interactions
    pub(crate) main_inventory_key: InventoryKey,
    // Mutable state of the player
    pub(crate) state: Mutex<PlayerState>,
}

impl Player {
    pub fn name(&self) -> &str {
        self.name.as_ref()
    }
    /// Returns the inventory key for this player's main inventory.
    /// This can be used with InventoryManager to view or modify the
    /// player's inventory
    pub fn main_inventory(&self) -> InventoryKey {
        self.main_inventory_key
    }
    pub fn hotbar_inventory_view(&self) -> InventoryViewId {
        self.state.lock().hotbar_inventory_view.id
    }
    pub fn last_position(&self) -> PlayerPositionUpdate {
        self.state.lock().last_position
    }
    fn to_server_proto(&self) -> StoredPlayer {
        StoredPlayer {
            name: self.name.clone(),
            last_position: Some(self.last_position().position.try_into().unwrap()),
            main_inventory: self.main_inventory_key.as_bytes().to_vec(),
        }
    }
    fn from_server_proto(game_state: Arc<GameState>, proto: &StoredPlayer) -> Result<Player> {
        let main_inventory_key = InventoryKey::parse_bytes(&proto.main_inventory)?;

        Ok(Player {
            name: proto.name.clone(),
            main_inventory_key,
            state: Mutex::new(PlayerState {
                last_position: PlayerPositionUpdate {
                    tick: game_state.tick(),
                    position: proto
                        .last_position
                        .as_ref()
                        .with_context(|| "Missing last_position in StoredPlayer")?
                        .try_into()?,
                    velocity: vec3(0., 0., 0.),
                    face_direction: (0., 0.),
                },
                active_popups: vec![],
                inventory_popup: make_inventory_popup(game_state.clone(), main_inventory_key)?,
                hotbar_inventory_view: InventoryView::new_stored(
                    main_inventory_key,
                    game_state,
                    false,
                    false,
                )?,
            }),
        })
    }

    fn new_player(name: &str, game_state: Arc<GameState>) -> Result<Player> {
        let main_inventory_key = game_state.inventory_manager().make_inventory(4, 8)?;
        // TODO provide hooks here
        // TODO custom spawn location
        let player = Player {
            name: name.to_string(),
            main_inventory_key,
            state: PlayerState {
                last_position: PlayerPositionUpdate {
                    tick: game_state.tick(),
                    position: vec3(5., 10., 5.),
                    velocity: Vector3::zero(),
                    face_direction: (0., 0.),
                },
                active_popups: vec![],
                inventory_popup: make_inventory_popup(game_state.clone(), main_inventory_key)?,
                hotbar_inventory_view: InventoryView::new_stored(
                    main_inventory_key,
                    game_state,
                    false,
                    false,
                )?,
            }
            .into(),
        };

        Ok(player)
    }
}

fn make_inventory_popup(
    game_state: Arc<GameState>,
    main_inventory_key: InventoryKey,
) -> Result<Popup<'static>> {
    Ok(Popup::new(game_state)
        .inventory_view_stored("main".to_string(), main_inventory_key, true, true)?
        .set_inventory_action_callback(|ctx| {
            println!("testonly {:?}", ctx.keys().collect::<Vec<_>>());
        }))
}

pub(crate) struct PlayerState {
    pub(crate) last_position: PlayerPositionUpdate,
    // The inventory popup is always loaded, even when it's not shown
    pub(crate) inventory_popup: Popup<'static>,
    // Other active popups for the player. These get deleted when closed.
    pub(crate) active_popups: Vec<Popup<'static>>,
    // The player's main inventory, which is shown in the user hotbar
    pub(crate) hotbar_inventory_view: InventoryView<()>,
}

// Struct held by the client contexts for this player. When dropped, the
// PlayerManager is notified of this via the Drop impl
// TODO make this cleaner and more sensible, this is fine for an initial impl
pub(crate) struct PlayerContext {
    pub(crate) player: Arc<Player>,
    manager: Arc<PlayerManager>,
}
impl PlayerContext {
    pub(crate) fn update_position(&self, pos: PlayerPositionUpdate) {
        self.player.state.lock().last_position = pos;
    }
}
impl Deref for PlayerContext {
    type Target = Player;

    fn deref(&self) -> &Self::Target {
        &self.player
    }
}
impl Drop for PlayerContext {
    fn drop(&mut self) {
        self.manager.drop_disconnect(&self.player.name)
    }
}

/// Manages players and provides access to them and their data.
pub struct PlayerManager {
    db: Arc<dyn GameDatabase>,
    game_state: Weak<GameState>,
    // To avoid leaks, we should avoid situations where arbitrary code can
    // clone Player instances. To do this, we keep Arc<Player> in private fields
    // and only provide &Player borrows
    //
    // This should probably be cleaned up somehow in the future; current approach seems ugly
    active_players: Mutex<HashMap<String, Arc<Player>>>,

    shutdown: CancellationToken,
    writeback: Mutex<Option<JoinHandle<Result<()>>>>,
}
impl PlayerManager {
    fn db_key(player: &str) -> Vec<u8> {
        KeySpace::Player.make_key(player.as_bytes())
    }
    fn game_state(&self) -> Arc<GameState> {
        self.game_state.upgrade().unwrap()
    }
    pub(crate) fn connect(self: Arc<Self>, name: &str) -> Result<PlayerContext> {
        let mut lock = self.active_players.lock();
        if lock.contains_key(name) {
            bail!("Player {name} already connected");
        }
        let player = match self.db.get(&Self::db_key(name))? {
            Some(player_proto) => Player::from_server_proto(
                self.game_state().clone(),
                &StoredPlayer::decode(player_proto.as_slice())?,
            )?,
            None => {
                log::info!("New player {name} joining");
                let player = Player::new_player(name, self.game_state().clone())?;
                self.write_back(&player)?;
                player
            }
        };
        let player = Arc::new(player);
        lock.insert(name.to_string(), player.clone());

        Ok(PlayerContext {
            player,
            manager: self.clone(),
        })
    }
    fn drop_disconnect(&self, name: &str) {
        match self.active_players.lock().entry(name.to_string()) {
            Entry::Occupied(entry) => {
                let count = Arc::strong_count(entry.get());
                // We expect 2 - one in the map, and one that we're holding while calling drop_disconnect
                if count != 2 {
                    log::error!("Arc<Player> seems to be leaking; {count} remaining references");
                }
                match self.write_back(entry.get()) {
                    Ok(_) => {
                        log::info!("{name} disconnected; data written back")
                    }
                    Err(e) => {
                        log::error!("Writeback for {name} failed: {:?}", e);
                    }
                }
                entry.remove();
            }
            Entry::Vacant(_) => {
                log::error!("Trying to disconnect player {name} but they're not present. This is a sign of a serious inconsistency.");
                if cfg!(debug_assertions) {
                    panic!("Trying to disconnect player {name} but they're not present. This is a sign of a serious inconsistency.")
                }
            }
        }
    }
    fn write_back(&self, player: &Player) -> Result<()> {
        self.db.put(
            &Self::db_key(&player.name),
            &player.to_server_proto().encode_to_vec(),
        )
    }

    pub(crate) fn new(
        game_state: Weak<GameState>,
        db: Arc<dyn GameDatabase>,
    ) -> Arc<PlayerManager> {
        let result = Arc::new(PlayerManager {
            game_state,
            db,
            active_players: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            writeback: Mutex::new(None),
        });
        result.start_writeback();
        result
    }

    pub(crate) async fn writeback_loop(self: Arc<Self>) -> Result<()> {
        while !self.shutdown.is_cancelled() {
            let deadline = Instant::now() + PLAYER_WRITEBACK_INTERVAL;
            select! {
                _ = tokio::time::sleep_until(deadline.into()) => {},
                _ = self.shutdown.cancelled() => {
                    log::info!("Player writeback detected cancellation");
                }
            }
            tokio::task::block_in_place(|| self.flush())?;
        }
        log::info!("Player writeback exiting");
        Ok(())
    }

    fn start_writeback(self: &Arc<Self>) {
        let clone = self.clone();
        *(self.writeback.lock()) = Some(tokio::spawn(async { clone.writeback_loop().await }));
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    pub(crate) async fn await_shutdown(&self) -> Result<()> {
        let mut lock = self.writeback.lock();
        let mut writeback_handle = None;
        swap(lock.deref_mut(), &mut writeback_handle);
        // lock must be dropped before the await point
        drop(lock);
        writeback_handle.unwrap().await??;
        tokio::task::block_in_place(|| self.flush())?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        for player in self.active_players.lock().values() {
            self.write_back(player)?;
        }
        Ok(())
    }
}

const PLAYER_WRITEBACK_INTERVAL: Duration = Duration::from_secs(10);
