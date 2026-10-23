use ring::Ring;

#[test]
fn fills_then_overwrites_oldest() {
    let mut r = Ring::new(3);
    assert_eq!(r.push(1), None);
    r.push(2);
    r.push(3);
    assert!(r.is_full());
    assert_eq!(r.push(4), Some(1));
    assert_eq!(r.to_vec(), vec![2, 3, 4]);
}

#[test]
fn get_past_the_end_is_none_even_when_full() {
    let mut r = Ring::new(3);
    r.extend([1, 2, 3]);
    assert_eq!(r.get(2), Some(&3));
    assert_eq!(r.get(3), None);
}

#[test]
fn get_past_the_end_when_partly_full() {
    let mut r = Ring::new(4);
    r.extend([1, 2]);
    assert_eq!(r.get(2), None);
}

#[test]
fn pop_front_wraps_around() {
    let mut r = Ring::new(3);
    r.extend([1, 2, 3]);
    assert_eq!(r.pop_front(), Some(1));
    r.push(4);
    assert_eq!(r.pop_front(), Some(2));
    r.push(5);
    assert_eq!(r.pop_front(), Some(3));
    assert_eq!(r.pop_front(), Some(4));
    r.push(6);
    assert_eq!(r.pop_front(), Some(5));
    assert_eq!(r.pop_front(), Some(6));
    assert_eq!(r.pop_front(), None);
    assert!(r.is_empty());
}

#[test]
fn front_back_and_clear() {
    let mut r = Ring::new(2);
    r.extend(["a", "b", "c"]);
    assert_eq!(r.front(), Some(&"b"));
    assert_eq!(r.back(), Some(&"c"));
    r.clear();
    assert_eq!(r.front(), None);
    assert_eq!(r.len(), 0);
}
