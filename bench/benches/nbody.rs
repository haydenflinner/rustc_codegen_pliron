const N: usize = 5;

#[derive(Clone, Copy)]
pub struct Body {
    x: [f64; 3],
    v: [f64; 3],
    m: f64,
}

#[inline(never)]
fn advance(bodies: &mut [Body; N], dt: f64) {
    for i in 0..N {
        for j in (i + 1)..N {
            let dx = bodies[i].x[0] - bodies[j].x[0];
            let dy = bodies[i].x[1] - bodies[j].x[1];
            let dz = bodies[i].x[2] - bodies[j].x[2];
            let d2 = dx * dx + dy * dy + dz * dz;
            let mag = dt / (d2 * d2.sqrt());
            let (a, b) = (bodies[i].m * mag, bodies[j].m * mag);
            bodies[i].v[0] -= dx * b;
            bodies[i].v[1] -= dy * b;
            bodies[i].v[2] -= dz * b;
            bodies[j].v[0] += dx * a;
            bodies[j].v[1] += dy * a;
            bodies[j].v[2] += dz * a;
        }
    }
    for b in bodies.iter_mut() {
        for k in 0..3 {
            b.x[k] += dt * b.v[k];
        }
    }
}

pub fn setup() -> [Body; N] {
    [
        Body {
            x: [0.0; 3],
            v: [0.0; 3],
            m: 39.478,
        },
        Body {
            x: [4.84, -1.62, -0.09],
            v: [0.0007, 0.0035, 0.0],
            m: 0.016,
        },
        Body {
            x: [8.34, 4.12, -0.40],
            v: [-0.0017, 0.0016, 0.0],
            m: 0.0037,
        },
        Body {
            x: [12.89, -15.11, -0.22],
            v: [0.0013, 0.0015, 0.0],
            m: 0.0008,
        },
        Body {
            x: [15.37, 25.35, -0.22],
            v: [-0.0024, 0.0004, 0.0],
            m: 0.00007,
        },
    ]
}

pub fn run(bodies: &mut [Body; N]) -> f64 {
    for _ in 0..2_000_000 {
        advance(bodies, 0.01);
    }
    bodies[0].x[0]
}
