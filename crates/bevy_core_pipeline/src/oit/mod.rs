use bevy_app::prelude::*;
use bevy_asset::{load_internal_asset, Handle};
use bevy_ecs::prelude::*;
use bevy_math::UVec2;
use bevy_render::{
    camera::ExtractedCamera,
    extract_component::{ExtractComponent, ExtractComponentPlugin},
    render_graph::{RenderGraphApp, ViewNodeRunner},
    render_resource::{BufferUsages, BufferVec, Shader, TextureUsages},
    renderer::{RenderDevice, RenderQueue},
    view::Msaa,
    Render, RenderApp, RenderSet,
};
use bevy_utils::{tracing::trace, warn_once, Instant};
use resolve::{
    node::{OitResolveNode, OitResolvePass},
    OitResolvePlugin,
};

use crate::core_3d::{
    graph::{Core3d, Node3d},
    Camera3d,
};

pub mod resolve;

pub const OIT_DRAW_SHADER_HANDLE: Handle<Shader> = Handle::weak_from_u128(4042527984320512);

// TODO consider supporting multiple OIT techniques like WBOIT, Moment Based OIT,
// depth peeling, stochastic transparency, ray tracing etc.
// This should probably be done by adding an enum to this component
#[derive(Component, Clone, Copy, ExtractComponent)]
pub struct OrderIndependentTransparencySettings {
    // TODO actually send that value to the shader
    layer_count: u8,
}

impl Default for OrderIndependentTransparencySettings {
    fn default() -> Self {
        Self { layer_count: 8 }
    }
}

pub struct OrderIndependentTransparencyPlugin;
impl Plugin for OrderIndependentTransparencyPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        load_internal_asset!(
            app,
            OIT_DRAW_SHADER_HANDLE,
            "oit_draw.wgsl",
            Shader::from_wgsl
        );

        app.add_plugins((
            ExtractComponentPlugin::<OrderIndependentTransparencySettings>::default(),
            OitResolvePlugin,
        ))
        .add_systems(Update, check_msaa)
        .add_systems(Last, configure_depth_texture_usages);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app.add_systems(
            Render,
            prepare_oit_buffers.in_set(RenderSet::PrepareResources),
        );

        render_app
            .add_render_graph_node::<ViewNodeRunner<OitResolveNode>>(Core3d, OitResolvePass)
            .add_render_graph_edges(Core3d, (Node3d::MainTransparentPass, OitResolvePass));
    }

    fn finish(&self, app: &mut bevy_app::App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app.init_resource::<OitBuffers>();
    }
}

// WARN This should only happen for cameras with the [`OrderIndependentTransparencySettings`]
// but when multiple cameras are present on the same window
// bevy reuses the same depth texture so we need to set this on all cameras.
fn configure_depth_texture_usages(mut new_cameras: Query<&mut Camera3d, Added<Camera3d>>) {
    for mut camera in &mut new_cameras {
        let mut usages = TextureUsages::from(camera.depth_texture_usages);
        usages |= TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING;
        camera.depth_texture_usages = usages.into();
    }
}

fn check_msaa(cameras: Query<&Msaa, With<OrderIndependentTransparencySettings>>) {
    for msaa in &cameras {
        if msaa.samples() > 1 {
            warn_once!(
                "MSAA should be disabled when using Order Independent Transparency. \
                It will cause some rendering issues on some platform. Consider using another AA method."
            );
        }
    }
}

#[derive(Resource)]
pub struct OitBuffers {
    pub layers: BufferVec<UVec2>,
    pub layer_ids: BufferVec<i32>,
}

impl FromWorld for OitBuffers {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();
        let render_queue = world.resource::<RenderQueue>();

        // initialize buffers with something so there's a valid binding

        let mut layers = BufferVec::new(BufferUsages::COPY_DST | BufferUsages::STORAGE);
        layers.reserve(0, render_device);
        layers.write_buffer(render_device, render_queue);

        let mut layer_ids = BufferVec::new(BufferUsages::COPY_DST | BufferUsages::STORAGE);
        layer_ids.reserve(0, render_device);
        layer_ids.write_buffer(render_device, render_queue);

        Self { layers, layer_ids }
    }
}

/// This creates or resizes the oit buffers for each camera
/// It will always create one big buffer that's as big as the biggest buffer needed
/// Cameras with smaller viewports or less layers will simply use the big buffer and ignore the rest
#[allow(clippy::type_complexity)]
pub fn prepare_oit_buffers(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    cameras: Query<
        (&ExtractedCamera, &OrderIndependentTransparencySettings),
        (
            Changed<ExtractedCamera>,
            Changed<OrderIndependentTransparencySettings>,
        ),
    >,
    mut buffers: ResMut<OitBuffers>,
) {
    let mut max_layer_ids_size = usize::MIN;
    let mut max_layers_size = usize::MIN;
    for (camera, settings) in &cameras {
        let Some(size) = camera.physical_target_size else {
            continue;
        };

        let layer_count = settings.layer_count as usize;
        let size = (size.x * size.y) as usize;
        max_layer_ids_size = max_layer_ids_size.max(size);
        max_layers_size = max_layers_size.max(size * layer_count);
    }

    if buffers.layers.capacity() < max_layers_size {
        let start = Instant::now();
        buffers.layers.reserve(max_layers_size, &device);
        let remaining = max_layers_size - buffers.layers.capacity();
        for _ in 0..remaining {
            buffers.layers.push(UVec2::ZERO);
        }
        buffers.layers.write_buffer(&device, &queue);
        trace!(
            "OIT layers buffer updated in {:.01}ms with total size {} MiB",
            start.elapsed().as_millis(),
            buffers.layers.capacity() * std::mem::size_of::<UVec2>() / 1024 / 1024,
        );
    }

    if buffers.layer_ids.capacity() < max_layer_ids_size {
        let start = Instant::now();
        buffers.layer_ids.reserve(max_layer_ids_size, &device);
        let remaining = max_layer_ids_size - buffers.layer_ids.capacity();
        for _ in 0..remaining {
            buffers.layer_ids.push(0);
        }
        buffers.layer_ids.write_buffer(&device, &queue);
        trace!(
            "OIT layer ids buffer updated in {:.01}ms with total size {} MiB",
            start.elapsed().as_millis(),
            buffers.layer_ids.capacity() * std::mem::size_of::<UVec2>() / 1024 / 1024,
        );
    }
}
