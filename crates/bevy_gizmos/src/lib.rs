#![allow(clippy::type_complexity)]
#![warn(missing_docs)]

//! This crate adds an immediate mode drawing api to Bevy for visual debugging.
//!
//! # Example
//! ```
//! # use bevy_gizmos::prelude::*;
//! # use bevy_render::prelude::*;
//! # use bevy_math::prelude::*;
//! fn system(mut gizmos: Gizmos) {
//!     gizmos.line(Vec3::ZERO, Vec3::X, Color::GREEN);
//! }
//! # bevy_ecs::system::assert_is_system(system);
//! ```
//!
//! See the documentation on [`Gizmos`](crate::gizmos::Gizmos) for more examples.

use std::mem;

use bevy_app::{Last, Plugin, Update};
use bevy_asset::{load_internal_asset, AddAsset, Assets, Handle, HandleUntyped};
use bevy_core::cast_slice;
use bevy_ecs::{
    change_detection::DetectChanges,
    component::Component,
    entity::Entity,
    query::{ROQueryItem, Without},
    reflect::ReflectComponent,
    schedule::IntoSystemConfigs,
    system::{
        lifetimeless::{Read, SRes},
        Commands, Query, Res, ResMut, Resource, SystemParamItem,
    },
};
use bevy_reflect::{
    std_traits::ReflectDefault, FromReflect, Reflect, ReflectFromReflect, TypePath, TypeUuid,
};
use bevy_render::{
    color::Color,
    extract_component::{ComponentUniforms, DynamicUniformIndex, UniformComponentPlugin},
    primitives::Aabb,
    render_asset::{PrepareAssetError, RenderAsset, RenderAssetPlugin, RenderAssets},
    render_phase::{PhaseItem, RenderCommand, RenderCommandResult, TrackedRenderPass},
    render_resource::{
        BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
        BindGroupLayoutEntry, BindingType, Buffer, BufferBindingType, BufferInitDescriptor,
        BufferUsages, Shader, ShaderStages, ShaderType, VertexAttribute, VertexBufferLayout,
        VertexFormat, VertexStepMode,
    },
    renderer::RenderDevice,
    Extract, ExtractSchedule, Render, RenderApp, RenderSet,
};
use bevy_transform::components::{GlobalTransform, Transform};

pub mod gizmos;

#[cfg(feature = "bevy_sprite")]
mod pipeline_2d;
#[cfg(feature = "bevy_pbr")]
mod pipeline_3d;

use gizmos::{GizmoStorage, Gizmos};

/// The `bevy_gizmos` prelude.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::{gizmos::Gizmos, AabbGizmo, AabbGizmoConfig, GizmoConfig};
}

const LINE_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 7414812689238026784);

/// A [`Plugin`] that provides an immediate mode drawing api for visual debugging.
pub struct GizmoPlugin;

impl Plugin for GizmoPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        load_internal_asset!(app, LINE_SHADER_HANDLE, "lines.wgsl", Shader::from_wgsl);

        app.add_plugin(UniformComponentPlugin::<LineGizmoUniform>::default())
            .add_asset::<LineGizmo>()
            .add_plugin(RenderAssetPlugin::<LineGizmo>::default())
            .init_resource::<LineGizmoHandles>()
            .init_resource::<GizmoConfig>()
            .init_resource::<GizmoStorage>()
            .add_systems(Last, update_gizmo_meshes)
            .add_systems(
                Update,
                (
                    draw_aabbs,
                    draw_all_aabbs.run_if(|config: Res<GizmoConfig>| config.aabb.draw_all),
                ),
            );

        let Ok(render_app) = app.get_sub_app_mut(RenderApp) else { return; };

        render_app
            .add_systems(ExtractSchedule, extract_gizmo_data)
            .add_systems(Render, queue_line_gizmo_bind_group.in_set(RenderSet::Queue));

        #[cfg(feature = "bevy_sprite")]
        app.add_plugin(pipeline_2d::LineGizmo2dPlugin);
        #[cfg(feature = "bevy_pbr")]
        app.add_plugin(pipeline_3d::LineGizmo3dPlugin);
    }

    fn finish(&self, app: &mut bevy_app::App) {
        let Ok(render_app) = app.get_sub_app_mut(RenderApp) else { return; };

        let render_device = render_app.world.resource::<RenderDevice>();
        let layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: Some(LineGizmoUniform::min_size()),
                },
                count: None,
            }],
            label: Some("LineGizmoUniform layout"),
        });

        render_app.insert_resource(LineGizmoUniformBindgroupLayout { layout });
    }
}

/// A [`Resource`] that stores configuration for gizmos.
#[derive(Resource, Clone)]
pub struct GizmoConfig {
    /// Set to `false` to stop drawing gizmos.
    ///
    /// Defaults to `true`.
    pub enabled: bool,
    /// Line width specified in pixels.
    ///
    /// If `line_perspective` is `true` then this is the size in pixels at the camera's near plane.
    ///
    /// Defaults to `2.0`.
    pub line_width: f32,
    /// Apply perspective to gizmo lines.
    ///
    /// This setting only affects 3D, non-orhographic cameras.
    ///
    /// Defaults to `false`.
    pub line_perspective: bool,
    /// How closer to the camera than real geometry the line should be.
    ///
    /// Value between -1 and 1 (inclusive).
    /// * 0 means that there is no change to the line position when rendering
    /// * 1 means it is furthest away from camera as possible
    /// * -1 means that it will always render in front of other things.
    ///
    /// This is typically useful if you are drawing wireframes on top of polygons
    /// and your wireframe is z-fighting (flickering on/off) with your main model.
    /// You would set this value to a negative number close to 0.0.
    pub depth_bias: f32,
    /// Configuration for the [`AabbGizmo`].
    pub aabb: AabbGizmoConfig,
}

impl Default for GizmoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            line_width: 2.,
            line_perspective: false,
            depth_bias: 0.,
            aabb: Default::default(),
        }
    }
}

/// Configuration for drawing the [`Aabb`] component on entities.
#[derive(Clone, Default)]
pub struct AabbGizmoConfig {
    /// Draws all bounding boxes in the scene when set to `true`.
    ///
    /// To draw a specific entity's bounding box, you can add the [`AabbGizmo`] component.
    ///
    /// Defaults to `false`.
    pub draw_all: bool,
    /// The default color for bounding box gizmos.
    ///
    /// A random color is chosen per box if `None`.
    ///
    /// Defaults to `None`.
    pub default_color: Option<Color>,
}

/// Add this [`Component`] to an entity to draw its [`Aabb`] component.
#[derive(Component, Reflect, FromReflect, Default, Debug)]
#[reflect(Component, FromReflect, Default)]
pub struct AabbGizmo {
    /// The color of the box.
    ///
    /// The default color from the [`GizmoConfig`] resource is used if `None`,
    pub color: Option<Color>,
}

fn draw_aabbs(
    query: Query<(Entity, &Aabb, &GlobalTransform, &AabbGizmo)>,
    config: Res<GizmoConfig>,
    mut gizmos: Gizmos,
) {
    for (entity, &aabb, &transform, gizmo) in &query {
        let color = gizmo
            .color
            .or(config.aabb.default_color)
            .unwrap_or_else(|| color_from_entity(entity));
        gizmos.cuboid(aabb_transform(aabb, transform), color);
    }
}

fn draw_all_aabbs(
    query: Query<(Entity, &Aabb, &GlobalTransform), Without<AabbGizmo>>,
    config: Res<GizmoConfig>,
    mut gizmos: Gizmos,
) {
    for (entity, &aabb, &transform) in &query {
        let color = config
            .aabb
            .default_color
            .unwrap_or_else(|| color_from_entity(entity));
        gizmos.cuboid(aabb_transform(aabb, transform), color);
    }
}

fn color_from_entity(entity: Entity) -> Color {
    let hue = entity.to_bits() as f32 * 100_000. % 360.;
    Color::hsl(hue, 1., 0.5)
}

fn aabb_transform(aabb: Aabb, transform: GlobalTransform) -> GlobalTransform {
    transform
        * GlobalTransform::from(
            Transform::from_translation(aabb.center.into())
                .with_scale((aabb.half_extents * 2.).into()),
        )
}

#[derive(Resource, Default)]
struct LineGizmoHandles {
    list: Option<Handle<LineGizmo>>,
    strip: Option<Handle<LineGizmo>>,
}

fn update_gizmo_meshes(
    mut line_gizmos: ResMut<Assets<LineGizmo>>,
    mut handles: ResMut<LineGizmoHandles>,
    mut storage: ResMut<GizmoStorage>,
) {
    if storage.list_positions.is_empty() {
        handles.list = None;
    } else if let Some(handle) = handles.list.as_ref() {
        let list = line_gizmos.get_mut(handle).unwrap();

        list.positions = mem::take(&mut storage.list_positions);
        list.colors = mem::take(&mut storage.list_colors);
    } else {
        let mut list = LineGizmo {
            strip: false,
            ..Default::default()
        };

        list.positions = mem::take(&mut storage.list_positions);
        list.colors = mem::take(&mut storage.list_colors);

        handles.list = Some(line_gizmos.add(list));
    }

    if storage.strip_positions.is_empty() {
        handles.strip = None;
    } else if let Some(handle) = handles.strip.as_ref() {
        let strip = line_gizmos.get_mut(handle).unwrap();

        strip.positions = mem::take(&mut storage.strip_positions);
        strip.colors = mem::take(&mut storage.strip_colors);
    } else {
        let mut strip = LineGizmo {
            strip: true,
            ..Default::default()
        };

        strip.positions = mem::take(&mut storage.strip_positions);
        strip.colors = mem::take(&mut storage.strip_colors);

        handles.strip = Some(line_gizmos.add(strip));
    }
}

fn extract_gizmo_data(
    mut commands: Commands,
    handles: Extract<Res<LineGizmoHandles>>,
    config: Extract<Res<GizmoConfig>>,
) {
    if config.is_changed() {
        commands.insert_resource(config.clone());
    }

    if !config.enabled {
        return;
    }

    for handle in [&handles.list, &handles.strip].into_iter().flatten() {
        commands.spawn((
            LineGizmoUniform {
                line_width: config.line_width,
                depth_bias: config.depth_bias,
                #[cfg(feature = "webgl")]
                _padding: Default::default(),
            },
            handle.clone_weak(),
        ));
    }
}

#[derive(Component, ShaderType, Clone, Copy)]
struct LineGizmoUniform {
    line_width: f32,
    depth_bias: f32,
    /// WebGL2 structs must be 16 byte aligned.
    #[cfg(feature = "webgl")]
    _padding: bevy_math::Vec2,
}

#[derive(Debug, Default, Clone, TypeUuid, TypePath)]
#[uuid = "02b99cbf-bb26-4713-829a-aee8e08dedc0"]
struct LineGizmo {
    positions: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    /// Whether this gizmo's topology is a line-strip or line-list
    strip: bool,
}

#[derive(Debug, Clone)]
struct GpuLineGizmo {
    position_buffer: Buffer,
    color_buffer: Buffer,
    vertex_count: u32,
    strip: bool,
}

impl RenderAsset for LineGizmo {
    type ExtractedAsset = LineGizmo;

    type PreparedAsset = GpuLineGizmo;

    type Param = SRes<RenderDevice>;

    fn extract_asset(&self) -> Self::ExtractedAsset {
        self.clone()
    }

    fn prepare_asset(
        line_gizmo: Self::ExtractedAsset,
        render_device: &mut SystemParamItem<Self::Param>,
    ) -> Result<Self::PreparedAsset, PrepareAssetError<Self::ExtractedAsset>> {
        let position_buffer_data = cast_slice(&line_gizmo.positions);
        let position_buffer = render_device.create_buffer_with_data(&BufferInitDescriptor {
            usage: BufferUsages::VERTEX,
            label: Some("LineGizmo Position Buffer"),
            contents: position_buffer_data,
        });

        let color_buffer_data = cast_slice(&line_gizmo.colors);
        let color_buffer = render_device.create_buffer_with_data(&BufferInitDescriptor {
            usage: BufferUsages::VERTEX,
            label: Some("LineGizmo Color Buffer"),
            contents: color_buffer_data,
        });

        Ok(GpuLineGizmo {
            position_buffer,
            color_buffer,
            vertex_count: line_gizmo.positions.len() as u32,
            strip: line_gizmo.strip,
        })
    }
}

#[derive(Resource)]
struct LineGizmoUniformBindgroupLayout {
    layout: BindGroupLayout,
}

#[derive(Resource)]
struct LineGizmoUniformBindgroup {
    bindgroup: BindGroup,
}

fn queue_line_gizmo_bind_group(
    mut commands: Commands,
    line_gizmo_uniform_layout: Res<LineGizmoUniformBindgroupLayout>,
    render_device: Res<RenderDevice>,
    line_gizmo_uniforms: Res<ComponentUniforms<LineGizmoUniform>>,
) {
    if let Some(binding) = line_gizmo_uniforms.uniforms().binding() {
        commands.insert_resource(LineGizmoUniformBindgroup {
            bindgroup: render_device.create_bind_group(&BindGroupDescriptor {
                entries: &[BindGroupEntry {
                    binding: 0,
                    resource: binding,
                }],
                label: Some("LineGizmoUniform bindgroup"),
                layout: &line_gizmo_uniform_layout.layout,
            }),
        });
    }
}

struct SetLineGizmoBindGroup<const I: usize>;
impl<const I: usize, P: PhaseItem> RenderCommand<P> for SetLineGizmoBindGroup<I> {
    type ViewWorldQuery = ();
    type ItemWorldQuery = Read<DynamicUniformIndex<LineGizmoUniform>>;
    type Param = SRes<LineGizmoUniformBindgroup>;

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewWorldQuery>,
        uniform_index: ROQueryItem<'w, Self::ItemWorldQuery>,
        bind_group: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(
            I,
            &bind_group.into_inner().bindgroup,
            &[uniform_index.index()],
        );
        RenderCommandResult::Success
    }
}

struct DrawLineGizmo;
impl<P: PhaseItem> RenderCommand<P> for DrawLineGizmo {
    type ViewWorldQuery = ();
    type ItemWorldQuery = Read<Handle<LineGizmo>>;
    type Param = SRes<RenderAssets<LineGizmo>>;

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewWorldQuery>,
        handle: ROQueryItem<'w, Self::ItemWorldQuery>,
        line_gizmos: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let Some(line_gizmo) = line_gizmos.into_inner().get(handle) else {
            return RenderCommandResult::Failure;
        };

        pass.set_vertex_buffer(0, line_gizmo.position_buffer.slice(..));
        pass.set_vertex_buffer(1, line_gizmo.color_buffer.slice(..));

        let instances = if line_gizmo.strip {
            u32::max(line_gizmo.vertex_count, 1) - 1
        } else {
            line_gizmo.vertex_count / 2
        };

        pass.draw(0..6, 0..instances);

        RenderCommandResult::Success
    }
}

fn line_gizmo_vertex_buffer_layouts(strip: bool) -> Vec<VertexBufferLayout> {
    let stride_multiplier = if strip { 1 } else { 2 };
    use VertexFormat::*;
    vec![
        // Positions
        VertexBufferLayout {
            array_stride: Float32x3.size() * stride_multiplier,
            step_mode: VertexStepMode::Instance,
            attributes: vec![
                VertexAttribute {
                    format: Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                VertexAttribute {
                    format: Float32x3,
                    offset: Float32x3.size(),
                    shader_location: 1,
                },
            ],
        },
        // Colors
        VertexBufferLayout {
            array_stride: Float32x4.size() * stride_multiplier,
            step_mode: VertexStepMode::Instance,
            attributes: vec![
                VertexAttribute {
                    format: Float32x4,
                    offset: 0,
                    shader_location: 2,
                },
                VertexAttribute {
                    format: Float32x4,
                    offset: Float32x4.size(),
                    shader_location: 3,
                },
            ],
        },
    ]
}