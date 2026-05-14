use super::common::pos;
use crate::config::SpatialPartitionType;
use crate::overlay::dnve3::spatial_density::{SpatialDensityData, SpatialDensityUtils};

#[test]
fn spatial_density_empty_nodes_all_zeros() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 2, 0.0);
    assert!(
        data.density_map.iter().all(|&v| v == 0.0),
        "ノードが空なら density_map は全て 0 であるべき"
    );
}

#[test]
fn spatial_density_single_node_has_nonzero_entry() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let has_nonzero = data.density_map.iter().any(|&v| v > 0.0);
    assert!(
        has_nonzero,
        "1 つのノードがあれば density_map に非ゼロ値があるべき"
    );
}

#[test]
fn spatial_density_dir_count_matches_resolution() {
    let resolution = 8;
    let utils = SpatialDensityUtils::new(resolution);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 3, 0.0);
    assert_eq!(data.dir_count, resolution);
    assert_eq!(data.layer_count, 3);
    assert_eq!(data.density_map.len(), resolution * 3);
}

#[test]
fn spatial_partition_type_direction_counts_use_face_normals() {
    let cases = [
        (SpatialPartitionType::Tetrahedron, 4),
        (SpatialPartitionType::Cube, 6),
        (SpatialPartitionType::Octahedron, 8),
        (SpatialPartitionType::Dodecahedron, 12),
        (SpatialPartitionType::Icosahedron, 20),
    ];

    for (partition_type, expected_count) in cases {
        let utils = SpatialDensityUtils::new_with_partition(99, partition_type);
        assert_eq!(
            utils.directions.len(),
            expected_count,
            "{partition_type:?} should use one direction per face"
        );
        assert!(
            utils
                .directions
                .iter()
                .all(|dir| (dir.magnitude() - 1.0).abs() < 1e-6),
            "{partition_type:?} directions should be normalized"
        );
    }
}

#[test]
fn cube_partition_face_normals_are_axis_aligned() {
    let utils = SpatialDensityUtils::new_with_partition(99, SpatialPartitionType::Cube);

    for expected in [
        pos(1.0, 0.0, 0.0),
        pos(-1.0, 0.0, 0.0),
        pos(0.0, 1.0, 0.0),
        pos(0.0, -1.0, 0.0),
        pos(0.0, 0.0, 1.0),
        pos(0.0, 0.0, -1.0),
    ] {
        assert!(
            contains_direction(&utils.directions, expected),
            "cube partition should include axis direction {expected:?}"
        );
    }
}

#[test]
fn polyhedron_partition_directions_are_unique() {
    for partition_type in [
        SpatialPartitionType::Tetrahedron,
        SpatialPartitionType::Cube,
        SpatialPartitionType::Octahedron,
        SpatialPartitionType::Dodecahedron,
        SpatialPartitionType::Icosahedron,
    ] {
        let utils = SpatialDensityUtils::new_with_partition(99, partition_type);
        for (i, a) in utils.directions.iter().enumerate() {
            for b in utils.directions.iter().skip(i + 1) {
                assert!(
                    (*a - *b).magnitude() > 1e-5,
                    "{partition_type:?} should not contain duplicate directions: {a:?}"
                );
            }
        }
    }
}

#[test]
fn centrally_symmetric_polyhedron_partitions_have_opposite_directions() {
    for partition_type in [
        SpatialPartitionType::Cube,
        SpatialPartitionType::Octahedron,
        SpatialPartitionType::Dodecahedron,
        SpatialPartitionType::Icosahedron,
    ] {
        let utils = SpatialDensityUtils::new_with_partition(99, partition_type);
        for direction in &utils.directions {
            assert!(
                utils
                    .directions
                    .iter()
                    .any(|other| direction.dot(*other) < -0.999_99),
                "{partition_type:?} should include the opposite face normal for {direction:?}"
            );
        }
    }
}

#[test]
fn spatial_density_polyhedron_partition_uses_fixed_direction_count() {
    let utils = SpatialDensityUtils::new_with_partition(6, SpatialPartitionType::Icosahedron);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 3, 0.0);

    assert_eq!(data.dir_count, 20);
    assert_eq!(data.layer_count, 3);
    assert_eq!(data.density_map.len(), 20 * 3);
}

fn contains_direction(
    directions: &[crate::overlay::dnve3::spatial_density::Vector3],
    expected: crate::overlay::dnve3::spatial_density::Vector3,
) -> bool {
    directions
        .iter()
        .any(|direction| (*direction - expected).magnitude() < 1e-6)
}

#[test]
fn spatial_density_byte_roundtrip_keeps_shape_and_approximates_values() {
    let utils = SpatialDensityUtils::new(12);
    let original = utils.create_spatial_density(
        pos(0.0, 0.0, 0.0),
        &[
            pos(10.0, 0.0, 0.0),
            pos(-4.0, 7.0, 2.0),
            pos(3.0, -6.0, 1.0),
        ],
        3,
        0.0,
    );

    let encoded = original.to_byte_encoded();
    let decoded = SpatialDensityData::from_byte_encoded(&encoded);

    assert_eq!(decoded.dir_count, original.dir_count);
    assert_eq!(decoded.layer_count, original.layer_count);
    assert_eq!(decoded.density_map.len(), original.density_map.len());

    let max_value = original
        .density_map
        .iter()
        .copied()
        .fold(0.0_f32, |acc, v| acc.max(v));
    let tolerance = if max_value <= 0.0 {
        1e-6
    } else {
        (max_value / 255.0) + 1e-6
    };
    for (o, d) in original.density_map.iter().zip(decoded.density_map.iter()) {
        assert!(
            (o - d).abs() <= tolerance,
            "byte roundtrip error too large: original={o}, decoded={d}, tolerance={tolerance}"
        );
    }
}

#[test]
fn project_spatial_density_same_center_returns_clone() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let projected = utils.project_spatial_density(&data, pos(5.0, 0.0, 0.0));
    assert_eq!(
        data.density_map, projected.density_map,
        "同じ中心への projection は完全に同じマップを返すべき"
    );
}

#[test]
fn project_spatial_density_different_center_does_not_panic() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let _projected = utils.project_spatial_density(&data, pos(20.0, 0.0, 0.0));
}

#[test]
fn merge_spatial_density_increases_values() {
    let utils = SpatialDensityUtils::new(6);
    let a = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(5.0, 0.0, 0.0)], 2, 0.0);
    let b = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(-5.0, 0.0, 0.0)], 2, 0.0);
    let merged = utils.merge_spatial_density(&a, &b);

    let sum_a: f32 = a.density_map.iter().sum();
    let sum_b: f32 = b.density_map.iter().sum();
    let sum_m: f32 = merged.density_map.iter().sum();
    assert!(
        sum_m >= sum_a,
        "マージ後の合計は A の合計以上であるべき(sum_m={sum_m}, sum_a={sum_a}, sum_b={sum_b})"
    );
}
