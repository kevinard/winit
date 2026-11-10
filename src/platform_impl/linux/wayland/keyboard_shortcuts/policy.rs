//! Focused-seat acknowledgement policy, independent of protocol dispatch.
/// Actual compositor permission and current keyboard focus, not request success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Unfocused,
    Pending,
    Active,
    Inactive,
    Closed,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    Pending,
    Allowed,
    Denied,
}
pub(crate) fn focused_status(permissions: impl IntoIterator<Item = Option<Permission>>) -> Status {
    let mut focused = false;
    let mut pending = false;
    let mut denied = false;
    for permission in permissions {
        focused = true;
        match permission {
            Some(Permission::Allowed) => {}
            Some(Permission::Denied) => denied = true,
            _ => pending = true,
        }
    }
    if !focused {
        Status::Unfocused
    } else if denied {
        Status::Inactive
    } else if pending {
        Status::Pending
    } else {
        Status::Active
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_focused_keyboard_seat_must_acknowledge_inhibition() {
        assert_eq!(focused_status([]), Status::Unfocused);
        assert_eq!(focused_status([None]), Status::Pending);
        assert_eq!(focused_status([Some(Permission::Pending)]), Status::Pending);
        assert_eq!(
            focused_status([Some(Permission::Allowed), None]),
            Status::Pending
        );
        assert_eq!(
            focused_status([Some(Permission::Allowed), Some(Permission::Denied)]),
            Status::Inactive
        );
        assert_eq!(
            focused_status([Some(Permission::Pending), Some(Permission::Denied)]),
            Status::Inactive
        );
        assert_eq!(
            focused_status([Some(Permission::Allowed), Some(Permission::Allowed)]),
            Status::Active
        );
    }
}
