use bitset::BitSet;

#[test]
fn small_sets_have_room() {
    let mut s = BitSet::new(10);
    assert!(s.set(9));
    assert!(s.contains(9));
    assert!(!s.set(10));
}

#[test]
fn set_across_word_boundaries() {
    let mut s = BitSet::new(130);
    for n in [0, 63, 64, 65, 129] {
        assert!(s.set(n));
    }
    assert_eq!(s.to_vec(), vec![0, 63, 64, 65, 129]);
    assert_eq!(s.count(), 5);
}

#[test]
fn clear_removes_only_that_member() {
    let mut s = BitSet::from_slice(70, &[1, 2, 66]);
    assert!(s.clear(2));
    assert_eq!(s.to_vec(), vec![1, 66]);
    assert!(!s.contains(2));
    s.clear(2);
    assert_eq!(s.count(), 2);
}

#[test]
fn toggle_and_empty() {
    let mut s = BitSet::new(8);
    assert!(s.is_empty());
    s.toggle(3);
    assert!(s.contains(3));
    s.toggle(3);
    assert!(s.is_empty());
}

#[test]
fn union_and_intersection() {
    let a = BitSet::from_slice(100, &[1, 5, 70]);
    let b = BitSet::from_slice(100, &[5, 70, 99]);
    let mut u = a.clone();
    u.union_with(&b);
    assert_eq!(u.to_vec(), vec![1, 5, 70, 99]);
    let mut i = a.clone();
    i.intersect_with(&b);
    assert_eq!(i.to_vec(), vec![5, 70]);
    assert_eq!(i.first(), Some(5));
}
