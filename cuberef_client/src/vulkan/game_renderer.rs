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

use std::sync::Arc;

use anyhow::{Context, Result};

use log::info;

use vulkano::{
    command_buffer::PrimaryAutoCommandBuffer,
    swapchain::{self, AcquireError, SwapchainPresentInfo},
    sync::{future::FenceSignalFuture, FlushError, GpuFuture},
};

use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
};

use crate::{
    game_state::{ClientState, FrameState},
    net_client,
};

use super::{
    shaders::{cube_geometry, flat_texture, PipelineProvider, PipelineWrapper},
    CommandBufferBuilder, VulkanContext,
};

pub struct CuberefRenderer {
    ctx: VulkanContext,
    cube_provider: cube_geometry::CubePipelineProvider,
    cube_pipeline: cube_geometry::CubePipelineWrapper,

    flat_provider: flat_texture::FlatTexPipelineProvider,
    flat_pipeline: flat_texture::FlatTexPipelineWrapper,

    client_state: Arc<ClientState>,
    async_runtime: tokio::runtime::Runtime,
}
impl CuberefRenderer {
    pub(crate) fn create(event_loop: &EventLoop<()>, server_addr: &str) -> Result<CuberefRenderer> {
        let ctx = VulkanContext::create(event_loop).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let cs_handle = rt.spawn(net_client::connect_game(
            server_addr.to_string(),
            ctx.clone(),
        ));

        let client_state = rt.block_on(cs_handle).unwrap().unwrap();

        let cube_provider = cube_geometry::CubePipelineProvider::new(ctx.vk_device.clone())?;
        let cube_pipeline = cube_provider
            .make_pipeline(&ctx, client_state.cube_renderer.texture())
            .unwrap();

        let flat_provider = flat_texture::FlatTexPipelineProvider::new(ctx.vk_device.clone())?;
        let flat_pipeline = flat_provider
            .make_pipeline(&ctx, client_state.game_ui.lock().texture())
            .unwrap();

        Ok(CuberefRenderer {
            ctx,
            cube_provider,
            cube_pipeline,
            flat_provider,
            flat_pipeline,
            client_state,
            async_runtime: rt,
        })
    }

    pub fn run_loop(mut self, event_loop: EventLoop<()>) {
        let mut resized = false;
        let mut recreate_swapchain = false;
        let frames_in_flight = self.ctx.swapchain_images.len();
        let mut fences: Vec<Option<Arc<FenceSignalFuture<_>>>> = vec![None; frames_in_flight];
        let mut previous_fence_i = 0;

        event_loop.run(move |event, _, control_flow| {
            self.client_state.window_event(&event);
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    *control_flow = ControlFlow::Exit;
                }
                Event::WindowEvent {
                    event: WindowEvent::Resized(_),
                    ..
                } => {
                    resized = true;
                }
                Event::MainEventsCleared => {
                    if self.ctx.window.has_focus() && self.client_state.should_capture() {
                        let size = self.ctx.window.inner_size();
                        self.ctx
                            .window
                            .set_cursor_position(PhysicalPosition::new(
                                size.width / 2,
                                size.height / 2,
                            ))
                            .unwrap();
                        self.ctx
                            .window
                            .set_cursor_grab(winit::window::CursorGrabMode::Confined)
                            .unwrap();
                    } else {
                        self.ctx.window.set_cursor_visible(true);
                        self.ctx
                            .window
                            .set_cursor_grab(winit::window::CursorGrabMode::None)
                            .unwrap();
                    }
                    if resized || recreate_swapchain {
                        let size = self.ctx.window.inner_size();
                        recreate_swapchain = false;
                        self.ctx.recreate_swapchain(size).unwrap();
                        if resized {
                            resized = false;
                            self.handle_resize(size).unwrap();
                        }
                    }
                    let (image_i, suboptimal, acquire_future) =
                        match swapchain::acquire_next_image(self.ctx.swapchain.clone(), None) {
                            Ok(r) => r,
                            Err(AcquireError::OutOfDate) => {
                                info!("Swapchain out of date");
                                recreate_swapchain = true;
                                return;
                            }
                            Err(e) => panic!("failed to acquire next image: {e}"),
                        };
                    if suboptimal {
                        recreate_swapchain = true;
                    }
                    if let Some(image_fence) = &fences[image_i as usize] {
                        image_fence.wait(None).unwrap();
                    }
                    let previous_future = match fences[previous_fence_i as usize].clone() {
                        // Create a NowFuture
                        None => {
                            let mut now = vulkano::sync::now(self.ctx.vk_device.clone());
                            now.cleanup_finished();
                            now.boxed()
                        }
                        // Use the existing FenceSignalFuture
                        Some(fence) => fence.boxed(),
                    };
                    let window_size = self.ctx.window.inner_size();

                    let FrameState {
                        view_proj_matrix,
                        player_position,
                        tool_state
                    } = self
                        .client_state
                        .next_frame((window_size.width as f64) / (window_size.height as f64));
                    // From https://vulkano.rs/compute_pipeline/descriptor_sets.html:
                    // Once you have created a descriptor set, you may also use it with other pipelines,
                    // as long as the bindings' types match those the pipelines' shaders expect.
                    // But Vulkan requires that you provide a pipeline whenever you create a descriptor set;
                    // you cannot create one independently of any particular pipeline.
                    let mut command_buf_builder = self
                        .ctx
                        .start_command_buffer(self.ctx.framebuffers[image_i as usize].clone())
                        .unwrap();

                    self.cube_pipeline
                        .bind(&self.ctx, view_proj_matrix, &mut command_buf_builder)
                        .unwrap();

                    let mut cube_draw_calls: Vec<_> = self
                        .client_state
                        .chunks
                        .lock()
                        .iter()
                        .filter_map(|(_, chunk)| chunk.make_draw_call(player_position))
                        .collect();

                    // todo consider having separate passes for transparent vs solid blocks
                    // Proper transparency requires us to discard fragments, but that messes with early depth
                    // test on some graphics hardware.
                    if let Some(pointee) = tool_state.pointee {
                        cube_draw_calls.push(self.client_state.cube_renderer.make_pointee_cube(player_position, pointee).unwrap());
                    }
                    // test only
                    if let Some(neighbor) = tool_state.neighbor {
                        cube_draw_calls.push(self.client_state.cube_renderer.make_pointee_cube(player_position, neighbor).unwrap());
                    }

                    if !cube_draw_calls.is_empty() {
                        self.cube_pipeline
                            .draw(&mut command_buf_builder, &cube_draw_calls)
                            .unwrap();
                    }

                    self.flat_pipeline
                        .bind(&self.ctx, (), &mut command_buf_builder)
                        .unwrap();
                    self.flat_pipeline
                        .draw(
                            &mut command_buf_builder,
                            &self
                                .client_state
                                .game_ui
                                .lock()
                                .render(&self.ctx, &self.client_state)
                                .unwrap(),
                        )
                        .unwrap();

                    let command_buffers = self.finish_command_buffer(command_buf_builder).unwrap();

                    let future = previous_future
                        .join(acquire_future)
                        .then_execute(self.ctx.queue.clone(), command_buffers)
                        .unwrap()
                        .then_swapchain_present(
                            self.ctx.queue.clone(),
                            SwapchainPresentInfo::swapchain_image_index(
                                self.ctx.swapchain.clone(),
                                image_i,
                            ),
                        )
                        .then_signal_fence_and_flush();

                    fences[image_i as usize] = match future {
                        Ok(value) => Some(Arc::new(value)),
                        Err(FlushError::OutOfDate) => {
                            recreate_swapchain = true;
                            None
                        }
                        Err(e) => {
                            println!("failed to flush future: {e}");
                            None
                        }
                    };

                    previous_fence_i = image_i;
                }
                _ => {}
            }
        })
    }

    fn handle_resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        self.ctx.viewport.dimensions = size.into();
        self.cube_pipeline = self
            .cube_provider
            .make_pipeline(&self.ctx, self.client_state.cube_renderer.texture())
            .unwrap();
        self.flat_pipeline = self
            .flat_provider
            .make_pipeline(&self.ctx, self.client_state.game_ui.lock().texture())?;
        Ok(())
    }

    fn finish_command_buffer(
        &self,
        mut builder: CommandBufferBuilder,
    ) -> Result<PrimaryAutoCommandBuffer> {
        builder.end_render_pass()?;

        builder
            .build()
            .with_context(|| "Command buffer build failed")
    }
}
