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

/// Names for well-known block groups. By using these, different plugins
/// can interoperate effectively.
pub mod block_groups {
    /// Block group for all solid blocks, e.g. dirt, glass, sand
    pub const DEFAULT_SOLID: &str = "default:solid";
    /// Block group for all liquid/fluid blocks, e.g. water, lava
    pub const DEFAULT_LIQUID: &str = "default:liquid";

    /// Blocks that cannot be dug by hand or using a generic non-tool item
    pub const TOOL_REQUIRED: &str = "default:tool_required";
    /// Blocks that cannot be dug under any circumstances (other than admon intervention)
    /// Tools available to normal users should specify a dig_behavior of None for this block.
    pub const NOT_DIGGABLE: &str = "default:not_diggable";
}

pub mod blocks {
    pub const AIR: &str = "builtin:air";
}

pub mod items {
    use crate::protocol::items::{interaction_rule::DigBehavior, InteractionRule};

    use super::block_groups::*;
    /// Get the default interaction rules for generic items that aren't some kind of special tool
    /// e.g. stacks of random items/blocks, no item held in the hand, etc
    pub fn default_item_interaction_rules() -> Vec<InteractionRule> {
        vec![
            InteractionRule {
                block_group: vec![NOT_DIGGABLE.to_string()],
                dig_behavior: None,
            },
            InteractionRule {
                block_group: vec![TOOL_REQUIRED.to_string()],
                dig_behavior: None,
            },
            InteractionRule {
                block_group: vec![DEFAULT_SOLID.to_string()],
                dig_behavior: Some(DigBehavior::ConstantTime(1.0)),
            },
        ]
    }
}

pub mod textures {
    /// A simple fallback texture.
    pub const FALLBACK_UNKNOWN_TEXTURE: &str = "builtin:unknown";
}
