use serde::{Deserialize, Serialize};

/// Defines whether a capability can be read, written, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessMode {
    ReadOnly,
    ReadWrite,
}

/// A capability declares what a protocol adapter supports.
///
/// Each capability has a unique name (e.g., "layout", "tags", "workspace")
/// and an access mode that determines whether scripts can modify it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// Unique identifier for this capability (e.g., "layout", "workspace")
    pub name: String,
    /// Whether this capability is read-only or read-write
    pub access: AccessMode,
    /// Optional description for documentation
    pub description: Option<String>,
}

impl Capability {
    /// Create a new read-only capability.
    pub fn read_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            access: AccessMode::ReadOnly,
            description: None,
        }
    }

    /// Create a new read-write capability.
    pub fn read_write(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            access: AccessMode::ReadWrite,
            description: None,
        }
    }

    /// Add a description to this capability.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Check if this capability allows writes.
    pub fn is_writable(&self) -> bool {
        matches!(self.access, AccessMode::ReadWrite)
    }
}

/// A manifest of all capabilities supported by a protocol adapter.
#[derive(Debug, Clone, Default)]
pub struct CapabilityManifest {
    capabilities: Vec<Capability>,
}

impl CapabilityManifest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(mut self, cap: Capability) -> Self {
        self.capabilities.push(cap);
        self
    }

    pub fn find(&self, name: &str) -> Option<&Capability> {
        self.capabilities.iter().find(|c| c.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.capabilities.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }
}

impl FromIterator<Capability> for CapabilityManifest {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self {
            capabilities: iter.into_iter().collect(),
        }
    }
}
