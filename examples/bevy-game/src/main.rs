//! Tiny Bevy game built with rustc_codegen_pliron: steer the paddle with
//! A/D or arrows to catch falling stars. `--frames N` exits after N frames
//! (for headless smoke tests) and prints the score.

use bevy::prelude::*;

#[derive(Component)]
struct Paddle;

#[derive(Component)]
struct Star(f32);

#[derive(Resource, Default)]
struct Score(u32);

#[derive(Resource)]
struct FrameLimit(Option<u32>, u32);

#[derive(Component)]
struct ScoreText;

/// `--autoplay`: the paddle chases the lowest star (for unattended smoke runs).
#[derive(Resource)]
struct Autoplay(bool);

const W: f32 = 640.0;
const H: f32 = 480.0;

fn main() {
    pliron_hot::start();
    let limit = std::env::args()
        .skip_while(|a| a != "--frames")
        .nth(1)
        .and_then(|n| n.parse().ok());
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "pliron bevy".into(),
                resolution: (W as u32, H as u32).into(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(Score::default())
        .insert_resource(FrameLimit(limit, 0))
        .insert_resource(Autoplay(std::env::args().any(|a| a == "--autoplay")))
        .add_systems(Startup, setup)
        .add_systems(Update, (move_paddle, spawn_stars, fall_and_catch, show_score, frame_limit))
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.spawn((
        Sprite::from_color(Color::srgb(0.3, 0.7, 1.0), Vec2::new(100.0, 16.0)),
        Transform::from_xyz(0.0, -H / 2.0 + 30.0, 0.0),
        Paddle,
    ));
    commands.spawn((
        Text2d::new("score: 0"),
        Transform::from_xyz(0.0, H / 2.0 - 30.0, 1.0),
        ScoreText,
    ));
}

fn move_paddle(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    auto: Res<Autoplay>,
    stars: Query<&Transform, (With<Star>, Without<Paddle>)>,
    mut q: Query<&mut Transform, (With<Paddle>, Without<Star>)>,
) {
    let mut dir = 0.0;
    if auto.0 {
        let target = stars.iter().min_by(|a, b| a.translation.y.total_cmp(&b.translation.y));
        if let (Some(t), Ok(p)) = (target, q.single()) {
            let dx = t.translation.x - p.translation.x;
            if dx.abs() > 10.0 {
                dir = dx.signum();
            }
        }
    }
    if keys.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) {
        dir -= 1.0;
    }
    if keys.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) {
        dir += 1.0;
    }
    for mut t in &mut q {
        t.translation.x = (t.translation.x + dir * 400.0 * time.delta_secs()).clamp(-W / 2.0 + 50.0, W / 2.0 - 50.0);
    }
}

fn spawn_stars(mut commands: Commands, time: Res<Time>, mut acc: Local<f32>, mut seed: Local<u32>) {
    *acc += time.delta_secs();
    if *acc < 0.6 {
        return;
    }
    *acc = 0.0;
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    let x = (*seed >> 8) as f32 / (1u32 << 24) as f32 * (W - 40.0) - (W / 2.0 - 20.0);
    commands.spawn((
        Sprite::from_color(Color::srgb(1.0, 0.9, 0.2), Vec2::splat(18.0)),
        Transform::from_xyz(x, H / 2.0, 0.0),
        Star(120.0 + (*seed % 120) as f32),
    ));
}

fn fall_and_catch(
    mut commands: Commands,
    time: Res<Time>,
    mut score: ResMut<Score>,
    paddle: Query<&Transform, (With<Paddle>, Without<Star>)>,
    mut stars: Query<(Entity, &mut Transform, &Star)>,
) {
    let Ok(p) = paddle.single() else { return };
    for (e, mut t, s) in &mut stars {
        t.translation.y -= s.0 * time.delta_secs();
        let d = t.translation - p.translation;
        if d.y.abs() < 17.0 && d.x.abs() < 59.0 {
            score.0 += 1;
            commands.entity(e).despawn();
        } else if t.translation.y < -H / 2.0 - 20.0 {
            commands.entity(e).despawn();
        }
    }
}

fn show_score(score: Res<Score>, mut q: Query<&mut Text2d, With<ScoreText>>) {
    if score.is_changed() {
        for mut t in &mut q {
            t.0 = format!("score: {}", score.0);
        }
    }
}

fn frame_limit(mut l: ResMut<FrameLimit>, score: Res<Score>, stars: Query<&Star>, mut exit: MessageWriter<AppExit>) {
    l.1 += 1;
    if Some(l.1) == l.0 {
        println!("frames={} score={} stars_alive={}", l.1, score.0, stars.iter().count());
        exit.write(AppExit::Success);
    }
}
