use core::marker::PhantomData;

use bevy_app::{App, Plugin};
use bevy_ecs::{
    component::Component,
    entity::Entity,
    query::{Changed, QueryItem, ReadOnlyQueryData, WorldQuery},
    schedule::IntoSystemConfigs,
    system::{lifetimeless::Read, Commands, Query, Res, ResMut, Resource, StaticSystemParam},
    world::{FromWorld, World},
};
use bevy_utils::all_tuples;

use bevy_render::{
    extract_component::{ExtractComponent, ExtractComponentPlugin},
    render_resource::{
        AsBindGroup, BindGroupLayout, CachedComputePipelineId, CachedPipelineState,
        CachedRenderPipelineId, ComputePipeline, PipelineCache, PreparedBindGroup, RenderPipeline,
        SpecializedComputePipeline, SpecializedComputePipelines, SpecializedRenderPipeline,
        SpecializedRenderPipelines,
    },
    renderer::RenderDevice,
    sync_world::MainEntity,
    Render, RenderApp, RenderSet,
};

use super::GraphicsJob;

/// The status of a job input
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum JobInputStatus {
    /// Signals that input is ready
    Ready,
    /// Signals that input is not ready, but should be awaited
    Wait,
    /// Signals that input is not ready, and shouldn't be awaited
    Fail,
}

impl JobInputStatus {
    fn combine(self, rhs: Self) -> Self {
        match (self, rhs) {
            (JobInputStatus::Fail, _) | (_, JobInputStatus::Fail) => JobInputStatus::Fail,
            (JobInputStatus::Ready, JobInputStatus::Ready) => JobInputStatus::Ready,
            _ => JobInputStatus::Wait,
        }
    }
}

pub type JobInputItem<'a, J, In> = <In as JobInput<J>>::Item<'a>;

/// A trait describing input to a graphics job.
///
/// This trait may be thought of as similar to [`QueryData`](bevy_ecs::query::QueryData),
/// but which also sets more things up behind the scenes. For example,
/// an implementor may prepare a bind group using data from the job
/// entity and the [`World`].
///
/// Note: while there is no blanket impl for [`JobInput`] for all
/// [`ReadOnlyQueryData`] types, it *is* implemented for all single
/// components, [`Entity`], [`MainEntity`], and [`Option`].
pub trait JobInput<J: GraphicsJob> {
    type Data: ReadOnlyQueryData;
    type Item<'a>;

    /// a plugin to register on the app, to setup behind the scenes processing
    /// for this implementor.
    fn plugin() -> impl Plugin {
        |_: &mut App| {}
    }

    /// the status of the resources needed by this job input. For example,
    /// an implementor may check the status of a queued pipeline, and wait
    /// until it's done compiling.
    fn status(data: QueryItem<Self::Data>, world: &World) -> JobInputStatus;

    /// returns the actual job input item.
    fn get<'a>(data: QueryItem<'a, Self::Data>, world: &'a World) -> Self::Item<'a>;
}

macro_rules! impl_job_input_tuple {
    ($(($T: ident, $t: ident)),*) => {
        impl <J: GraphicsJob, $($T: JobInput<J>),*> JobInput<J> for ($($T,)*) {
            type Data = ($(<$T as JobInput<J>>::Data,)*);
            type Item<'a> = ($(<$T as JobInput<J>>::Item<'a>,)*);


            fn plugin() -> impl Plugin {
                |app: &mut App| {
                    app.add_plugins(($(<$T as JobInput<J>>::plugin()),*));
                }
            }

            #[allow(unused_variables)]
            fn status(data: QueryItem<Self::Data>, world: &World) -> JobInputStatus {
                let ($($t,)*) = data;
                JobInputStatus::Ready
                    $(.combine(<$T as JobInput<J>>::status($t, world)))*
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a>(data: QueryItem<'a, Self::Data>, world: &'a World) -> Self::Item<'a> {
                let ($($t,)*) = data;
                ($(<$T as JobInput<J>>::get($t, world),)*)
            }
        }
    }
}

all_tuples!(impl_job_input_tuple, 0, 15, T, t);

impl<J: GraphicsJob> JobInput<J> for Entity {
    type Data = Entity;

    type Item<'a> = Entity;

    fn status(_data: QueryItem<Self::Data>, _world: &World) -> JobInputStatus {
        JobInputStatus::Ready
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, _world: &'a World) -> Self::Item<'a> {
        data
    }
}

impl<J: GraphicsJob> JobInput<J> for MainEntity {
    type Data = MainEntity;

    type Item<'a> = Entity;

    fn status(_data: QueryItem<Self::Data>, _world: &World) -> JobInputStatus {
        JobInputStatus::Ready
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, _world: &'a World) -> Self::Item<'a> {
        data
    }
}

impl<'t, T: Component, J: GraphicsJob> JobInput<J> for &'t T {
    type Data = &'t T;

    type Item<'a> = &'a T;

    fn status(_data: QueryItem<Self::Data>, _world: &World) -> JobInputStatus {
        JobInputStatus::Ready
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, _world: &'a World) -> Self::Item<'a> {
        data
    }
}

impl<T: ReadOnlyQueryData, J: GraphicsJob> JobInput<J> for Option<T> {
    type Data = Option<T>;

    type Item<'a> = Option<<T as WorldQuery>::Item<'a>>;

    fn status(_data: QueryItem<Self::Data>, _world: &World) -> JobInputStatus {
        JobInputStatus::Ready
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, _world: &'a World) -> Self::Item<'a> {
        data
    }
}

pub struct JobAsBindGroup;

impl<J: GraphicsJob + AsBindGroup> JobInput<J> for JobAsBindGroup {
    type Data = Option<Read<PreparedJobBindGroup<J>>>;

    type Item<'a> = &'a PreparedBindGroup<<J as AsBindGroup>::Data>;

    fn plugin() -> impl Plugin {
        JobAsBindGroupPlugin::<J>(PhantomData)
    }

    fn status(data: QueryItem<Self::Data>, _world: &World) -> JobInputStatus {
        match data {
            Some(_) => JobInputStatus::Ready,
            None => JobInputStatus::Wait,
        }
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, _world: &'a World) -> Self::Item<'a> {
        &data.unwrap().0
    }
}

struct JobAsBindGroupPlugin<J>(PhantomData<J>);

impl<J: GraphicsJob + AsBindGroup> Plugin for JobAsBindGroupPlugin<J> {
    fn build(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(
                Render,
                prepare_job_bind_group::<J>.in_set(RenderSet::PrepareBindGroups),
            );
        }
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<JobBindGroupLayout<J>>();
        }
    }
}

/// A [`JobInput`] type that prepares the graphics job type *itself* as a bind group,
/// using its [`AsBindGroup`] implementation.
#[derive(Component)]
pub struct PreparedJobBindGroup<J: GraphicsJob + AsBindGroup>(
    PreparedBindGroup<<J as AsBindGroup>::Data>,
);

#[derive(Resource)]
struct JobBindGroupLayout<J: GraphicsJob + AsBindGroup>(BindGroupLayout, PhantomData<J>);

impl<J: GraphicsJob + AsBindGroup> FromWorld for JobBindGroupLayout<J> {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();
        Self(J::bind_group_layout(render_device), PhantomData)
    }
}

fn prepare_job_bind_group<J: GraphicsJob + AsBindGroup>(
    jobs: Query<(Entity, &J)>,
    layout: Res<JobBindGroupLayout<J>>,
    render_device: Res<RenderDevice>,
    mut param: StaticSystemParam<<J as AsBindGroup>::Param>,
    mut commands: Commands,
) {
    for (entity, job) in &jobs {
        if let Ok(bind_group) = job.as_bind_group(&layout.0, &render_device, &mut param) {
            commands
                .entity(entity)
                .insert(PreparedJobBindGroup::<J>(bind_group));
        }
    }
}

#[doc(hidden)]
pub trait SpecializedJobRenderPipeline:
    SpecializedRenderPipeline<Key: Send + Sync> + Resource + FromWorld
{
}
impl<P: SpecializedRenderPipeline<Key: Send + Sync> + Resource + FromWorld>
    SpecializedJobRenderPipeline for P
{
}

/// A [`JobInput`] type that sets up a [`RenderPipeline`] for a job. This component must be
/// added to a job as it is spawned in order to setup the pipeline.
#[derive(Component)]
pub struct JobRenderPipeline<P: SpecializedJobRenderPipeline>(pub P::Key);

impl<P: SpecializedJobRenderPipeline<Key: Default>> Default for JobRenderPipeline<P> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<J: GraphicsJob, P: SpecializedJobRenderPipeline> JobInput<J> for JobRenderPipeline<P> {
    type Data = Option<Read<JobRenderPipelineId<P>>>;

    type Item<'a> = &'a RenderPipeline;

    fn plugin() -> impl Plugin {
        JobRenderPipelinePlugin::<P>(PhantomData)
    }

    fn status(data: QueryItem<Self::Data>, world: &World) -> JobInputStatus {
        let Some(JobRenderPipelineId(id, _)) = data else {
            return JobInputStatus::Wait;
        };
        if matches!(
            world
                .resource::<PipelineCache>()
                .get_render_pipeline_state(*id),
            CachedPipelineState::Ok(_)
        ) {
            JobInputStatus::Ready
        } else {
            JobInputStatus::Wait
        }
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, world: &'a World) -> Self::Item<'a> {
        let id = data.unwrap().0;
        world
            .resource::<PipelineCache>()
            .get_render_pipeline(id)
            .expect("pipeline should be ready by this point")
    }
}

impl<P: SpecializedJobRenderPipeline> Clone for JobRenderPipeline<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<P: SpecializedJobRenderPipeline> ExtractComponent for JobRenderPipeline<P> {
    type QueryData = Read<JobRenderPipeline<P>>;

    type QueryFilter = ();

    type Out = JobRenderPipeline<P>;

    fn extract_component(item: QueryItem<'_, Self::QueryData>) -> Option<Self::Out> {
        Some(item.clone())
    }
}

#[derive(Component)]
#[doc(hidden)]
pub struct JobRenderPipelineId<P: SpecializedJobRenderPipeline>(
    CachedRenderPipelineId,
    PhantomData<P>,
);

struct JobRenderPipelinePlugin<P: SpecializedJobRenderPipeline>(PhantomData<P>);

impl<P: SpecializedJobRenderPipeline> Plugin for JobRenderPipelinePlugin<P> {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractComponentPlugin::<JobRenderPipeline<P>>::default());

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .init_resource::<SpecializedRenderPipelines<P>>()
                .add_systems(
                    Render,
                    queue_job_render_pipelines::<P>.in_set(RenderSet::Queue),
                );
        }
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<P>();
        }
    }
}

fn queue_job_render_pipelines<P: SpecializedJobRenderPipeline>(
    job_pipelines: Query<(Entity, &JobRenderPipeline<P>), Changed<JobRenderPipeline<P>>>,
    pipeline_cache: Res<PipelineCache>,
    base_pipeline: Res<P>,
    mut specializer: ResMut<SpecializedRenderPipelines<P>>,
    mut commands: Commands,
) {
    for (entity, job_pipeline) in &job_pipelines {
        let id = specializer.specialize(&pipeline_cache, &base_pipeline, job_pipeline.0.clone());
        commands
            .entity(entity)
            .insert(JobRenderPipelineId::<P>(id, PhantomData));
    }
}

#[doc(hidden)]
pub trait SpecializedJobComputePipeline:
    SpecializedComputePipeline<Key: Send + Sync> + Resource + FromWorld
{
}
impl<P: SpecializedComputePipeline<Key: Send + Sync> + Resource + FromWorld>
    SpecializedJobComputePipeline for P
{
}

/// A [`JobInput`] type that sets up a [`ComputePipeline`] for a job. This component must be
/// added to a job as it is spawned in order to setup the pipeline.
#[derive(Component)]
pub struct JobComputePipeline<P: SpecializedJobComputePipeline>(P::Key);

impl<P: SpecializedJobComputePipeline<Key: Default>> Default for JobComputePipeline<P> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<J: GraphicsJob, P: SpecializedJobComputePipeline> JobInput<J> for JobComputePipeline<P> {
    type Data = Option<Read<JobComputePipelineId<P>>>;

    type Item<'a> = &'a ComputePipeline;

    fn plugin() -> impl Plugin {
        JobComputePipelinePlugin::<P>(PhantomData)
    }

    fn status(data: QueryItem<Self::Data>, world: &World) -> JobInputStatus {
        let Some(JobComputePipelineId(id, _)) = data else {
            return JobInputStatus::Wait;
        };
        if matches!(
            world
                .resource::<PipelineCache>()
                .get_compute_pipeline_state(*id),
            CachedPipelineState::Ok(_)
        ) {
            JobInputStatus::Ready
        } else {
            JobInputStatus::Wait
        }
    }

    fn get<'a>(data: QueryItem<'a, Self::Data>, world: &'a World) -> Self::Item<'a> {
        let id = data.unwrap().0;
        world
            .resource::<PipelineCache>()
            .get_compute_pipeline(id)
            .expect("pipeline should be ready by this point")
    }
}

impl<P: SpecializedJobComputePipeline> Clone for JobComputePipeline<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<P: SpecializedJobComputePipeline> ExtractComponent for JobComputePipeline<P> {
    type QueryData = Read<JobComputePipeline<P>>;

    type QueryFilter = ();

    type Out = JobComputePipeline<P>;

    fn extract_component(item: QueryItem<'_, Self::QueryData>) -> Option<Self::Out> {
        Some(item.clone())
    }
}

#[derive(Component)]
#[doc(hidden)]
pub struct JobComputePipelineId<P: SpecializedJobComputePipeline>(
    CachedComputePipelineId,
    PhantomData<P>,
);

struct JobComputePipelinePlugin<P: SpecializedJobComputePipeline>(PhantomData<P>);

impl<P: SpecializedJobComputePipeline> Plugin for JobComputePipelinePlugin<P> {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractComponentPlugin::<JobComputePipeline<P>>::default());

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .init_resource::<SpecializedComputePipelines<P>>()
                .add_systems(
                    Render,
                    queue_job_compute_pipelines::<P>.in_set(RenderSet::Queue),
                );
        }
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<P>();
        }
    }
}

fn queue_job_compute_pipelines<P: SpecializedJobComputePipeline>(
    job_pipelines: Query<(Entity, &JobComputePipeline<P>), Changed<JobComputePipeline<P>>>,
    pipeline_cache: Res<PipelineCache>,
    base_pipeline: Res<P>,
    mut specializer: ResMut<SpecializedComputePipelines<P>>,
    mut commands: Commands,
) {
    for (entity, job_pipeline) in &job_pipelines {
        let id = specializer.specialize(&pipeline_cache, &base_pipeline, job_pipeline.0.clone());
        commands
            .entity(entity)
            .insert(JobComputePipelineId::<P>(id, PhantomData));
    }
}
