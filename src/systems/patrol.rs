use avian3d::prelude::{LinearVelocity, Position};
use bevy::picking::events::PointerClick;
use bevy::picking::prelude::{Pickable, PointerButton};
use bevy::prelude::*;

use crate::config::SimulationConfig;

pub struct PatrolPlugin;

impl Plugin for PatrolPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PatrolState>()
            .add_systems(Startup, spawn_button)
            .add_systems(FixedUpdate, move_robot);
    }
}

#[derive(Component)]
pub struct PatrolRobot;

#[derive(Component)]
struct PatrolLabel;

#[derive(Resource, Default)]
pub struct PatrolState(Option<Patrol>);

impl PatrolState {
    pub fn active(&self) -> bool {
        self.0.is_some()
    }
}

struct Patrol {
    center: Vec3,
    span: f32,
    speed: f32,
    phase: f64,
}

fn spawn_button(mut commands: Commands, config: Res<SimulationConfig>) {
    if !config.preview.enabled {
        return;
    }
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: px(12),
                right: px(12),
                padding: UiRect::axes(px(12), px(8)),
                ..default()
            },
            BackgroundColor(Color::srgb(0.10, 0.19, 0.27)),
            GlobalZIndex(1),
            Pickable::default(),
        ))
        .with_child((PatrolLabel, Text::new("Start patrol"), Pickable::IGNORE))
        .observe(toggle_patrol);
}

fn toggle_patrol(
    click: On<PointerClick>,
    mut label: Query<&mut Text, With<PatrolLabel>>,
    mut robot: Query<(&Position, &mut LinearVelocity), With<PatrolRobot>>,
    mut state: ResMut<PatrolState>,
    config: Res<SimulationConfig>,
) {
    if click.button != PointerButton::Primary {
        return;
    }
    let Ok((position, mut velocity)) = robot.single_mut() else {
        return;
    };
    if state.0.take().is_some() {
        velocity.x = 0.0;
        velocity.z = 0.0;
    } else {
        let defaults = crate::config::PatrolConfig::default();
        let span = positive_or(config.patrol.span_m, defaults.span_m);
        let speed = positive_or(config.patrol.speed_mps, defaults.speed_mps);
        state.0 = Some(Patrol {
            center: position.0.into(),
            span,
            speed,
            phase: f64::from(span) * 0.5,
        });
    }
    for mut text in &mut label {
        **text = if state.active() {
            "Stop patrol"
        } else {
            "Start patrol"
        }
        .into();
    }
}

fn positive_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn move_robot(
    time: Res<Time<Fixed>>,
    mut state: ResMut<PatrolState>,
    mut robot: Query<(&Position, &mut LinearVelocity), With<PatrolRobot>>,
) {
    let Some(patrol) = state.0.as_mut() else {
        return;
    };
    let Ok((position, mut velocity)) = robot.single_mut() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let span = f64::from(patrol.span);
    patrol.phase = (patrol.phase + f64::from(patrol.speed) * f64::from(dt)).rem_euclid(2.0 * span);
    let offset = (span * 0.5 - (patrol.phase - span).abs()) as f32;
    let target = patrol.center + Vec3::X * offset;
    let current: Vec3 = position.0.into();
    velocity.x = (target.x - current.x) / dt;
    velocity.z = (target.z - current.z) / dt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use avian3d::prelude::{Collider, Mass, PhysicsPlugins, RigidBody};
    use bevy::camera::RenderTarget;
    use bevy::picking::{
        backend::HitData,
        events::Pointer,
        pointer::{Location, PointerId},
    };
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    #[test]
    fn click_moves_robot_between_both_endpoints_and_stops() {
        let fixed = Time::<Fixed>::from_hz(120.0);
        let step = fixed.timestep();
        let mut app = App::new();
        let mut config = SimulationConfig::default();
        config.preview.enabled = true;
        config.patrol = crate::config::PatrolConfig::default();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            PhysicsPlugins::default(),
            PatrolPlugin,
        ))
        .insert_resource(fixed)
        .insert_resource(TimeUpdateStrategy::ManualDuration(step))
        .insert_resource(config);
        app.finish();
        app.cleanup();
        app.world_mut().spawn((
            RigidBody::Static,
            Collider::cuboid(20.0, 1.0, 20.0),
            Transform::from_xyz(0.0, -0.5, 0.0),
        ));
        let robot = app
            .world_mut()
            .spawn((
                PatrolRobot,
                RigidBody::Dynamic,
                Collider::cylinder(0.2593615, 0.231298),
                Mass(15.0),
                Transform::from_xyz(1.0, 0.25, 2.0),
            ))
            .id();
        for _ in 0..120 {
            app.update();
        }
        let world = app.world_mut();
        let label = world
            .query_filtered::<Entity, With<PatrolLabel>>()
            .single(world)
            .unwrap();
        let button = world.get::<ChildOf>(label).unwrap().parent();
        let click = PointerClick {
            entity: button,
            pointer: Pointer::new(
                PointerId::Mouse,
                Location {
                    target: RenderTarget::default()
                        .normalize(Some(Entity::PLACEHOLDER))
                        .unwrap(),
                    position: Vec2::ZERO,
                },
            ),
            button: PointerButton::Primary,
            hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            duration: Duration::ZERO,
            count: 1,
        };
        let start: Vec3 = world.get::<Position>(robot).unwrap().0.into();
        assert!(world.get::<LinearVelocity>(robot).is_some());
        world.trigger(click.clone());
        assert!(world.resource::<PatrolState>().active());
        assert_eq!(world.get::<Text>(label).unwrap().as_str(), "Stop patrol");

        let mut low = start.x;
        let mut high = start.x;
        let mut prev = start.x;
        let mut cruise = [0, 0];
        for _ in 0..1200 {
            app.update();
            let p: Vec3 = app.world().get::<Position>(robot).unwrap().0.into();
            low = low.min(p.x);
            high = high.max(p.x);
            assert!((p.z - start.z).abs() < 0.02, "lane drift: {p:?}");
            let speed = (p.x - prev) / step.as_secs_f32();
            if (speed - 1.0).abs() < 0.04 {
                cruise[0] += 1;
            } else if (speed + 1.0).abs() < 0.04 {
                cruise[1] += 1;
            }
            prev = p.x;
        }
        assert!((low - (start.x - 2.5)).abs() < 0.03, "low endpoint: {low}");
        assert!((high - (start.x + 2.5)).abs() < 0.03, "high endpoint: {high}");
        assert!(
            cruise.into_iter().all(|ticks| ticks > 500),
            "cruise ticks: {cruise:?}"
        );

        app.world_mut().trigger(click);
        assert_eq!(app.world().get::<Text>(label).unwrap().as_str(), "Start patrol");
        let stopped: Vec3 = app.world().get::<Position>(robot).unwrap().0.into();
        for _ in 0..120 {
            app.update();
        }
        let final_position: Vec3 = app.world().get::<Position>(robot).unwrap().0.into();
        assert!((final_position.x - stopped.x).abs() < 0.01);
    }
}
