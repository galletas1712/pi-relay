#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(pub u64);

impl TurnId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(pub u64);

impl EventId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = *next;
        next.0 += 1;
        current
    }
}
