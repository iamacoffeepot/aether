use aether_math::{Rigid, Vec3};

const EPSILON: f32 = 1.0e-6;

fn assert_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= EPSILON,
        "expected {expected}, got {actual}"
    );
}

fn assert_vec3_close(actual: Vec3, expected: Vec3) {
    assert_close(actual.x, expected.x);
    assert_close(actual.y, expected.y);
    assert_close(actual.z, expected.z);
}

#[test]
fn sampled_rigid_maps_blend_as_weighted_affine_maps() {
    // Quarter-turn about Z followed by a translation.
    let first = Rigid::sample(|point| Vec3::new(-point.y + 4.0, point.x - 2.0, point.z + 1.0));
    // Quarter-turn about X followed by a different translation.
    let second = Rigid::sample(|point| Vec3::new(point.x - 3.0, -point.z + 5.0, point.y - 4.0));
    let first_weight = 0.25;
    let second_weight = 0.75;

    let blended = Rigid::ZERO
        .add_scaled(&first, first_weight)
        .add_scaled(&second, second_weight);

    let probe = Vec3::new(2.0, -1.0, 3.0);
    let expected_point = first.point(probe) * first_weight + second.point(probe) * second_weight;
    assert_vec3_close(blended.point(probe), expected_point);

    let first_rows = first.rows();
    let second_rows = second.rows();
    let blended_rows = blended.rows();
    for row in 0..3 {
        for column in 0..4 {
            assert_close(
                blended_rows[row][column],
                first_rows[row][column] * first_weight + second_rows[row][column] * second_weight,
            );
        }
    }

    let rows_point = Vec3::new(
        blended_rows[0][0] * probe.x
            + blended_rows[0][1] * probe.y
            + blended_rows[0][2] * probe.z
            + blended_rows[0][3],
        blended_rows[1][0] * probe.x
            + blended_rows[1][1] * probe.y
            + blended_rows[1][2] * probe.z
            + blended_rows[1][3],
        blended_rows[2][0] * probe.x
            + blended_rows[2][1] * probe.y
            + blended_rows[2][2] * probe.z
            + blended_rows[2][3],
    );
    assert_vec3_close(rows_point, expected_point);
}
