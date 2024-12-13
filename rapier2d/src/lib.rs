//! # bevy_rapier2d Integration for bevy-tnua
//!
//! In addition to the instruction in bevy-tnua's documentation:
//!
//! * Add [`TnuaRapier2dPlugin`] to the Bevy app.
//! * Add [`TnuaRapier2dIOBundle`] to each character entity controlled by Tnua.
//! * Optionally: Add [`TnuaRapier2dSensorShape`] to the sensor entities. This means the entity of
//!   the characters controlled by Tnua, but also other things like the entity generated by
//!   `TnuaCrouchEnforcer`, that can be affected with a closure.
mod spatial_ext;

use bevy::ecs::schedule::{InternedScheduleLabel, ScheduleLabel};
use bevy::prelude::*;
use bevy::utils::HashSet;
use bevy_rapier2d::prelude::*;
use bevy_rapier2d::rapier;
use bevy_rapier2d::rapier::prelude::InteractionGroups;

use bevy_tnua_physics_integration_layer::data_for_backends::TnuaGhostPlatform;
use bevy_tnua_physics_integration_layer::data_for_backends::TnuaGhostSensor;
use bevy_tnua_physics_integration_layer::data_for_backends::TnuaToggle;
use bevy_tnua_physics_integration_layer::data_for_backends::{
    TnuaMotor, TnuaProximitySensor, TnuaProximitySensorOutput, TnuaRigidBodyTracker,
};
use bevy_tnua_physics_integration_layer::obstacle_radar::TnuaObstacleRadar;
use bevy_tnua_physics_integration_layer::subservient_sensors::TnuaSubservientSensor;
use bevy_tnua_physics_integration_layer::TnuaPipelineStages;
use bevy_tnua_physics_integration_layer::TnuaSystemSet;
pub use spatial_ext::TnuaSpatialExtRapier2d;

/// Add this plugin to use bevy_rapier2d as a physics backend.
///
/// This plugin should be used in addition to `TnuaControllerPlugin`.
pub struct TnuaRapier2dPlugin {
    schedule: InternedScheduleLabel,
}

impl TnuaRapier2dPlugin {
    pub fn new(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: schedule.intern(),
        }
    }
}

impl Default for TnuaRapier2dPlugin {
    fn default() -> Self {
        Self::new(Update)
    }
}

impl Plugin for TnuaRapier2dPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            self.schedule,
            TnuaSystemSet.before(PhysicsSet::SyncBackend).run_if(
                |rapier_config: Single<&RapierConfiguration>| rapier_config.physics_pipeline_active,
            ),
        );
        app.add_systems(
            self.schedule,
            (
                update_rigid_body_trackers_system,
                update_proximity_sensors_system,
                update_obstacle_radars_system,
            )
                .in_set(TnuaPipelineStages::Sensors),
        );
        app.add_systems(
            self.schedule,
            apply_motors_system.in_set(TnuaPipelineStages::Motors),
        );
    }
}

/// `bevy_rapier2d`-specific components required for Tnua to work.
#[derive(Bundle, Default)]
pub struct TnuaRapier2dIOBundle {
    pub velocity: Velocity,
    pub external_force: ExternalForce,
    pub read_mass_properties: ReadMassProperties,
}

/// Add this component to make [`TnuaProximitySensor`] cast a shape instead of a ray.
#[derive(Component)]
pub struct TnuaRapier2dSensorShape(pub Collider);

fn update_rigid_body_trackers_system(
    rapier_config: Single<&RapierConfiguration>,
    mut query: Query<(
        &GlobalTransform,
        &Velocity,
        &mut TnuaRigidBodyTracker,
        Option<&TnuaToggle>,
    )>,
) {
    for (transform, velocity, mut tracker, tnua_toggle) in query.iter_mut() {
        match tnua_toggle.copied().unwrap_or_default() {
            TnuaToggle::Disabled => continue,
            TnuaToggle::SenseOnly => {}
            TnuaToggle::Enabled => {}
        }
        let (_, rotation, translation) = transform.to_scale_rotation_translation();
        *tracker = TnuaRigidBodyTracker {
            translation,
            rotation,
            velocity: velocity.linvel.extend(0.0),
            angvel: Vec3::new(0.0, 0.0, velocity.angvel),
            gravity: rapier_config.gravity.extend(0.0),
        };
    }
}

pub(crate) fn get_collider(
    rapier_context: &RapierContext,
    entity: Entity,
) -> Option<&rapier::geometry::Collider> {
    let collider_handle = rapier_context.entity2collider().get(&entity)?;
    rapier_context.colliders.get(*collider_handle)
    //if let Some(owner_collider) = rapier_context.entity2collider().get(&owner_entity).and_then(|handle| rapier_context.colliders.get(*handle)) {
}

#[allow(clippy::type_complexity)]
fn update_proximity_sensors_system(
    rapier_context_query: RapierContextAccess,
    mut query: Query<(
        Entity,
        &RapierContextEntityLink,
        &GlobalTransform,
        &mut TnuaProximitySensor,
        Option<&TnuaRapier2dSensorShape>,
        Option<&mut TnuaGhostSensor>,
        Option<&TnuaSubservientSensor>,
        Option<&TnuaToggle>,
    )>,
    ghost_platforms_query: Query<(), With<TnuaGhostPlatform>>,
    other_object_query_query: Query<(&GlobalTransform, &Velocity)>,
) {
    query.par_iter_mut().for_each(
        |(
            owner_entity,
            rapier_context_entity_link,
            transform,
            mut sensor,
            shape,
            mut ghost_sensor,
            subservient,
            tnua_toggle,
        )| {
            match tnua_toggle.copied().unwrap_or_default() {
                TnuaToggle::Disabled => return,
                TnuaToggle::SenseOnly => {}
                TnuaToggle::Enabled => {}
            }

            let Some(rapier_context) = rapier_context_query.try_context(rapier_context_entity_link)
            else {
                return;
            };

            let cast_origin = transform.transform_point(sensor.cast_origin);
            let cast_direction = sensor.cast_direction;

            struct CastResult {
                entity: Entity,
                proximity: f32,
                intersection_point: Vec2,
                // Use 3D and not 2D because converting a direction from 2D to 3D is more painful
                // than it should be.
                normal: Dir3,
            }

            let owner_entity = if let Some(subservient) = subservient {
                subservient.owner_entity
            } else {
                owner_entity
            };

            let mut query_filter = QueryFilter::new().exclude_rigid_body(owner_entity);
            let owner_solver_groups: InteractionGroups;

            if let Some(owner_collider) = get_collider(rapier_context, owner_entity) {
                let collision_groups = owner_collider.collision_groups();
                query_filter.groups = Some(CollisionGroups {
                    memberships: Group::from_bits_truncate(collision_groups.memberships.bits()),
                    filters: Group::from_bits_truncate(collision_groups.filter.bits()),
                });
                owner_solver_groups = owner_collider.solver_groups();
            } else {
                owner_solver_groups = InteractionGroups::all();
            }

            let mut already_visited_ghost_entities = HashSet::<Entity>::default();

            let has_ghost_sensor = ghost_sensor.is_some();

            let do_cast = |cast_range_skip: f32,
                           already_visited_ghost_entities: &HashSet<Entity>|
             -> Option<CastResult> {
                let predicate = |other_entity: Entity| {
                    if let Some(other_collider) = get_collider(rapier_context, other_entity) {
                        if !other_collider.solver_groups().test(owner_solver_groups) {
                            if has_ghost_sensor && ghost_platforms_query.contains(other_entity) {
                                if already_visited_ghost_entities.contains(&other_entity) {
                                    return false;
                                }
                            } else {
                                return false;
                            }
                        }
                        if other_collider.is_sensor() {
                            return false;
                        }
                    }

                    // This fixes https://github.com/idanarye/bevy-tnua/issues/14
                    if let Some(contact) = rapier_context.contact_pair(owner_entity, other_entity) {
                        let same_order = owner_entity == contact.collider1();
                        for manifold in contact.manifolds() {
                            if 0 < manifold.num_points() {
                                let manifold_normal = if same_order {
                                    manifold.local_n2()
                                } else {
                                    manifold.local_n1()
                                };
                                if sensor.intersection_match_prevention_cutoff
                                    < manifold_normal.dot(cast_direction.truncate())
                                {
                                    return false;
                                }
                            }
                        }
                    }
                    true
                };
                let query_filter = query_filter.predicate(&predicate);
                let cast_origin = cast_origin + cast_range_skip * *cast_direction;
                let cast_range = sensor.cast_range - cast_range_skip;
                if let Some(TnuaRapier2dSensorShape(shape)) = shape {
                    rapier_context
                        .cast_shape(
                            cast_origin.truncate(),
                            0.0,
                            cast_direction.truncate(),
                            shape,
                            ShapeCastOptions {
                                max_time_of_impact: cast_range,
                                target_distance: 0.0,
                                stop_at_penetration: false,
                                compute_impact_geometry_on_penetration: false,
                            },
                            query_filter,
                        )
                        .and_then(|(entity, hit)| {
                            let details = hit.details?;
                            Some(CastResult {
                                entity,
                                proximity: hit.time_of_impact + cast_range_skip,
                                intersection_point: details.witness1,
                                normal: Dir3::new(details.normal1.extend(0.0))
                                    .unwrap_or_else(|_| -cast_direction),
                            })
                        })
                } else {
                    rapier_context
                        .cast_ray_and_get_normal(
                            cast_origin.truncate(),
                            cast_direction.truncate(),
                            cast_range,
                            false,
                            query_filter,
                        )
                        .map(|(entity, hit)| CastResult {
                            entity,
                            proximity: hit.time_of_impact + cast_range_skip,
                            intersection_point: hit.point,
                            normal: Dir3::new(hit.normal.extend(0.0))
                                .unwrap_or_else(|_| -cast_direction),
                        })
                }
            };

            let mut cast_range_skip = 0.0;
            if let Some(ghost_sensor) = ghost_sensor.as_mut() {
                ghost_sensor.0.clear();
            }
            sensor.output = 'sensor_output: loop {
                if let Some(CastResult {
                    entity,
                    proximity,
                    intersection_point,
                    normal,
                }) = do_cast(cast_range_skip, &already_visited_ghost_entities)
                {
                    let entity_linvel;
                    let entity_angvel;
                    if let Ok((entity_transform, entity_velocity)) =
                        other_object_query_query.get(entity)
                    {
                        entity_angvel = Vec3::new(0.0, 0.0, entity_velocity.angvel);
                        entity_linvel = entity_velocity.linvel.extend(0.0)
                            + if 0.0 < entity_velocity.angvel.abs() {
                                let relative_point =
                                    intersection_point - entity_transform.translation().truncate();
                                // NOTE: no need to project relative_point on the rotation plane, it will not
                                // affect the cross product.
                                entity_angvel.cross(relative_point.extend(0.0))
                            } else {
                                Vec3::ZERO
                            };
                    } else {
                        entity_angvel = Vec3::ZERO;
                        entity_linvel = Vec3::ZERO;
                    }
                    let sensor_output = TnuaProximitySensorOutput {
                        entity,
                        proximity,
                        normal,
                        entity_linvel,
                        entity_angvel,
                    };
                    if ghost_platforms_query.contains(entity) {
                        cast_range_skip = proximity;
                        already_visited_ghost_entities.insert(entity);
                        if let Some(ghost_sensor) = ghost_sensor.as_mut() {
                            ghost_sensor.0.push(sensor_output);
                        }
                    } else {
                        break 'sensor_output Some(sensor_output);
                    }
                } else {
                    break 'sensor_output None;
                }
            };
        },
    );
}

fn update_obstacle_radars_system(
    rapier_context: Res<RapierContext>,
    rapier_config: Res<RapierConfiguration>,
    mut radars_query: Query<(Entity, &mut TnuaObstacleRadar, &GlobalTransform)>,
) {
    if radars_query.is_empty() {
        return;
    }
    for (radar_owner_entity, mut radar, radar_transform) in radars_query.iter_mut() {
        let (_radar_scale, radar_rotation, radar_translation) =
            radar_transform.to_scale_rotation_translation();
        radar.pre_marking_update(
            radar_owner_entity,
            radar_translation,
            Dir3::new(rapier_config.gravity.extend(0.0)).unwrap_or(Dir3::Y),
        );
        rapier_context.intersections_with_shape(
            radar_translation.truncate(),
            radar_rotation.to_euler(EulerRot::ZYX).0,
            &Collider::cuboid(radar.radius, 0.5 * radar.height),
            Default::default(),
            |obstacle_entity| {
                if radar_owner_entity == obstacle_entity {
                    return true;
                }
                radar.mark_seen(obstacle_entity);
                true
            },
        );
    }
}

fn apply_motors_system(
    mut query: Query<(
        &TnuaMotor,
        &mut Velocity,
        &ReadMassProperties,
        &mut ExternalForce,
        Option<&TnuaToggle>,
    )>,
) {
    for (motor, mut velocity, mass_properties, mut external_force, tnua_toggle) in query.iter_mut()
    {
        match tnua_toggle.copied().unwrap_or_default() {
            TnuaToggle::Disabled | TnuaToggle::SenseOnly => {
                *external_force = Default::default();
                return;
            }
            TnuaToggle::Enabled => {}
        }
        if motor.lin.boost.is_finite() {
            velocity.linvel += motor.lin.boost.truncate();
        }
        if motor.lin.acceleration.is_finite() {
            external_force.force = motor.lin.acceleration.truncate() * mass_properties.get().mass;
        }
        if motor.ang.boost.is_finite() {
            velocity.angvel += motor.ang.boost.z;
        }
        if motor.ang.acceleration.is_finite() {
            external_force.torque =
                motor.ang.acceleration.z * mass_properties.get().principal_inertia;
        }
    }
}
