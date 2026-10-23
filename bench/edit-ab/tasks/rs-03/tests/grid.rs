use grid::Grid;

fn sample() -> Grid {
    Grid::from_rows(&[vec![1, 2, 3], vec![4, 5, 6]])
}

#[test]
fn basics() {
    let g = sample();
    assert_eq!((g.width(), g.height()), (3, 2));
    assert_eq!(g.get(2, 1), Some(6));
    assert_eq!(g.get(3, 0), None);
    assert_eq!(g.row(1), vec![4, 5, 6]);
    assert_eq!(g.col(0), vec![1, 4]);
}

#[test]
fn transpose_swaps_the_shape() {
    let t = sample().transpose();
    assert_eq!((t.width(), t.height()), (2, 3));
    assert_eq!(t.to_rows(), vec![vec![1, 4], vec![2, 5], vec![3, 6]]);
}

#[test]
fn flip_horizontal_keeps_the_shape() {
    let f = sample().flip_horizontal();
    assert_eq!((f.width(), f.height()), (3, 2));
    assert_eq!(f.to_rows(), vec![vec![3, 2, 1], vec![6, 5, 4]]);
}

#[test]
fn flip_vertical() {
    let f = sample().flip_vertical();
    assert_eq!(f.to_rows(), vec![vec![4, 5, 6], vec![1, 2, 3]]);
}

#[test]
fn neighbors_inside_the_grid() {
    let g = sample();
    assert_eq!(g.neighbors4(1, 0), vec![(0, 0), (2, 0), (1, 1)]);
    assert_eq!(g.neighbors4(1, 1), vec![(1, 0), (0, 1), (2, 1)]);
}

#[test]
fn neighbors_at_the_right_edge() {
    let g = sample();
    assert_eq!(g.neighbors4(2, 0), vec![(1, 0), (2, 1)]);
    assert_eq!(g.neighbors4(2, 1), vec![(2, 0), (1, 1)]);
}

#[test]
fn sums_and_fill() {
    let mut g = sample();
    assert_eq!(g.sum(), 21);
    g.fill(7);
    assert_eq!(g.count(7), 6);
}
