#![allow(clippy::type_complexity)]

mod app;
mod input;
mod meta;
mod runner;
pub use app::*;
pub use input::*;
pub use meta::*;
use runner::erase_jobs;

use core::marker::PhantomData;

use bevy_app::{App, Plugin, PostUpdate};
use bevy_ecs::{
    component::Component,
    entity::Entity,
    query::Added,
    schedule::IntoSystemConfigs,
    system::{Commands, Query, Resource},
    world::World,
};
use bevy_render::{
    render_resource::CommandEncoder, renderer::RenderDevice, sync_component::SyncComponentPlugin,
    ExtractSchedule, Render, RenderApp, RenderSet,
};
use bevy_render::{sync_world::RenderEntity, Extract};

pub struct GraphicsJobsPlugin {
    settings: JobExecutionSettings,
}

impl Plugin for GraphicsJobsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, compute_priorities);

        app.insert_resource(self.settings);
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(ExtractSchedule, extract_job_meta);
        }
    }
}

pub enum GraphicsJobSet {
    Check,
    Execute,
    Cleanup,
}

#[derive(Copy, Clone, Resource)]
pub struct JobExecutionSettings {
    pub max_jobs_per_frame: u32,
    pub max_job_stall_frames: u32,
}

pub struct SpecializedGraphicsJobPlugin<J: GraphicsJob>(PhantomData<J>);

impl<J: GraphicsJob> Default for SpecializedGraphicsJobPlugin<J> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<J: GraphicsJob> Plugin for SpecializedGraphicsJobPlugin<J> {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            SyncComponentPlugin::<J>::default(),
            <J as GraphicsJob>::In::plugin(),
        ));

        app.register_required_components::<J, JobMarker>();

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .add_systems(ExtractSchedule, extract_jobs::<J>)
                .add_systems(Render, erase_jobs::<J>.in_set(RenderSet::Queue));
        }
    }
}

pub struct JobError;

pub trait GraphicsJob: Component + Clone {
    type In: JobInput<Self>;

    fn run(
        &self,
        world: &World,
        render_device: &RenderDevice,
        commands: &mut CommandEncoder,
        input: JobInputItem<Self, Self::In>,
    ) -> Result<(), JobError>;
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct JobId(Entity);

impl JobId {
    pub fn new_unchecked(id: Entity) -> Self {
        Self(id)
    }

    pub fn id(&self) -> Entity {
        self.0
    }
}

fn extract_jobs<J: GraphicsJob>(
    jobs: Extract<Query<(RenderEntity, &J), Added<JobMarker>>>,
    mut commands: Commands,
) {
    let cloned_jobs = jobs
        .iter()
        .map(|(entity, job)| (entity, job.clone()))
        .collect::<Vec<_>>();
    commands.insert_batch(cloned_jobs);
}

fn extract_job_meta(
    jobs: Extract<
        Query<
            (
                RenderEntity,
                &JobPriority,
                &ComputedPriority,
                &JobDependencies,
            ),
            Added<JobMarker>,
        >,
    >,
    mut commands: Commands,
) {
    for (render_entity, priority, computed_priority, deps) in &jobs {
        commands.entity(render_entity).insert((
            if deps.0.is_empty() {
                JobStatus::Waiting
            } else {
                JobStatus::Blocked
            },
            *priority,
            *computed_priority,
            deps.clone(), //FIXME: entities contained have main world ids, not render world ids
        ));
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Component)]
enum JobStatus {
    Blocked,
    Waiting,
    Ready,
    Done,
}

fn compute_priorities(jobs: Query<(&JobPriority, &JobDependencies)>) {}

fn check_dependencies(mut commands: Commands) {}
