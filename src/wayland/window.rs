//! Window lifecycle states and explicit transition machine.

use crate::error::WaylandError;

/// Formal window lifecycle states for Wayland surfaces and XDG Shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowState {
    /// Window surface is unmapped or not yet created.
    #[default]
    Unmapped,
    /// Globals bound and surface requested, awaiting initial configure.
    Initializing,
    /// Received initial configure event from compositor, ready for rendering.
    Configured,
    /// Surface actively rendering and presenting frames to compositor.
    Active,
    /// Window close requested or surface destroyed.
    Closed,
}

impl WindowState {
    /// Human-readable label for error diagnostics and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmapped => "Unmapped",
            Self::Initializing => "Initializing",
            Self::Configured => "Configured",
            Self::Active => "Active",
            Self::Closed => "Closed",
        }
    }

    /// Validates whether transitioning from `self` to `next` is permitted.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Unmapped, Self::Initializing)
                | (Self::Initializing, Self::Configured)
                | (Self::Configured, Self::Active)
                | (Self::Active, Self::Active)
                | (Self::Active, Self::Configured)
                | (Self::Configured, Self::Configured)
                | (Self::Initializing, Self::Initializing)
                | (_, Self::Closed)
        )
    }

    /// Transitions to the next state, or returns [`WaylandError::InvalidStateTransition`].
    ///
    /// # Errors
    /// Returns an error if the transition violates the protocol state machine.
    pub fn transition_to(&mut self, next: Self) -> Result<(), WaylandError> {
        if self.can_transition_to(next) {
            *self = next;
            Ok(())
        } else {
            Err(WaylandError::InvalidStateTransition {
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}
