use context_capsule::restore::{
    SavedRect, SnapSlot, snap_rect,
};

fn work_area() -> SavedRect {
    SavedRect {
        left: -1920,
        top: 40,
        right: 0,
        bottom: 1080,
    }
}

#[test]
fn every_snap_slot_stays_inside_the_current_work_area() {
    let area = work_area();
    let slots = [
        SnapSlot::LeftHalf,
        SnapSlot::RightHalf,
        SnapSlot::TopHalf,
        SnapSlot::BottomHalf,
        SnapSlot::TopLeftQuarter,
        SnapSlot::TopRightQuarter,
        SnapSlot::BottomLeftQuarter,
        SnapSlot::BottomRightQuarter,
        SnapSlot::LeftThird,
        SnapSlot::CenterThird,
        SnapSlot::RightThird,
        SnapSlot::LeftTwoThirds,
        SnapSlot::RightTwoThirds,
    ];

    for slot in slots {
        let rect = snap_rect(area, slot);
        assert!(rect.left >= area.left, "{slot:?} left edge escaped work area");
        assert!(rect.top >= area.top, "{slot:?} top edge escaped work area");
        assert!(rect.right <= area.right, "{slot:?} right edge escaped work area");
        assert!(rect.bottom <= area.bottom, "{slot:?} bottom edge escaped work area");
        assert!(rect.width() > 0, "{slot:?} produced an empty width");
        assert!(rect.height() > 0, "{slot:?} produced an empty height");
    }
}

#[test]
fn half_and_quarter_snap_slots_tile_the_work_area_without_gaps() {
    let area = work_area();
    let left = snap_rect(area, SnapSlot::LeftHalf);
    let right = snap_rect(area, SnapSlot::RightHalf);
    assert_eq!(left.left, area.left);
    assert_eq!(left.right, right.left);
    assert_eq!(right.right, area.right);
    assert_eq!(left.top, area.top);
    assert_eq!(left.bottom, area.bottom);

    let top_left = snap_rect(area, SnapSlot::TopLeftQuarter);
    let top_right = snap_rect(area, SnapSlot::TopRightQuarter);
    let bottom_left = snap_rect(area, SnapSlot::BottomLeftQuarter);
    let bottom_right = snap_rect(area, SnapSlot::BottomRightQuarter);
    assert_eq!(top_left.right, top_right.left);
    assert_eq!(top_left.bottom, bottom_left.top);
    assert_eq!(top_right.bottom, bottom_right.top);
    assert_eq!(bottom_left.right, bottom_right.left);
    assert_eq!(top_left.left, area.left);
    assert_eq!(top_right.right, area.right);
    assert_eq!(bottom_left.bottom, area.bottom);
    assert_eq!(bottom_right.bottom, area.bottom);
}

#[test]
fn thirds_cover_the_full_width_even_when_width_is_not_divisible_by_three() {
    let area = SavedRect {
        left: 0,
        top: 0,
        right: 1919,
        bottom: 1000,
    };
    let left = snap_rect(area, SnapSlot::LeftThird);
    let center = snap_rect(area, SnapSlot::CenterThird);
    let right = snap_rect(area, SnapSlot::RightThird);
    assert_eq!(left.left, area.left);
    assert_eq!(left.right, center.left);
    assert_eq!(center.right, right.left);
    assert_eq!(right.right, area.right);

    let left_two = snap_rect(area, SnapSlot::LeftTwoThirds);
    let right_two = snap_rect(area, SnapSlot::RightTwoThirds);
    assert_eq!(left_two.left, area.left);
    assert_eq!(right_two.right, area.right);
    assert_eq!(left_two.right, right.left);
    assert_eq!(right_two.left, center.left);
}
