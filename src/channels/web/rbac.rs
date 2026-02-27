//! Role-Based Access Control (RBAC) for the web gateway.
//!
//! Defines a four-tier role hierarchy (Viewer < User < Admin < Owner) and a
//! permission enum that maps each gateway operation to the minimum role required.

use serde::{Deserialize, Serialize};

/// User roles ordered by privilege (lowest to highest).
///
/// The derive ordering matches the declaration order, so `Viewer < User < Admin < Owner`.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only access to chat history, jobs, memory, etc.
    Viewer,
    /// Viewer + can send messages, approve tool calls, trigger routines.
    #[default]
    User,
    /// User + can install/remove extensions/skills, manage settings, cancel/restart jobs.
    Admin,
    /// Admin + can shutdown gateway, manage pairing, export/import settings.
    Owner,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Viewer => write!(f, "viewer"),
            Self::User => write!(f, "user"),
            Self::Admin => write!(f, "admin"),
            Self::Owner => write!(f, "owner"),
        }
    }
}

/// Permissions that can be checked against a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    // Viewer-level
    ViewChat,
    ViewJobs,
    ViewMemory,
    ViewLogs,
    ViewExtensions,
    ViewSkills,
    ViewSettings,
    ViewRoutines,
    ViewChannels,
    ViewGatewayStatus,

    // User-level
    SendMessage,
    ApproveToolCall,
    SubmitAuthToken,
    TriggerRoutine,

    // Admin-level
    WriteMemory,
    InstallExtension,
    RemoveExtension,
    ActivateExtension,
    InstallSkill,
    RemoveSkill,
    ModifySettings,
    ManageSessions,
    CancelJob,
    RestartJob,
    ToggleRoutine,
    ModifyRoutine,
    ChangeLogLevel,

    // Owner-level
    ShutdownGateway,
    ManagePairing,
    ExportSettings,
    ImportSettings,
}

impl Role {
    /// Check whether this role has a given permission.
    pub fn has_permission(self, perm: Permission) -> bool {
        self >= perm.minimum_role()
    }
}

impl Permission {
    /// The minimum role required for this permission.
    pub fn minimum_role(self) -> Role {
        use Permission::*;
        match self {
            ViewChat | ViewJobs | ViewMemory | ViewLogs | ViewExtensions | ViewSkills
            | ViewSettings | ViewRoutines | ViewChannels | ViewGatewayStatus => Role::Viewer,

            SendMessage | ApproveToolCall | SubmitAuthToken | TriggerRoutine => Role::User,

            WriteMemory | InstallExtension | RemoveExtension | ActivateExtension | InstallSkill
            | RemoveSkill | ModifySettings | ManageSessions | CancelJob | RestartJob
            | ToggleRoutine | ModifyRoutine | ChangeLogLevel => Role::Admin,

            ShutdownGateway | ManagePairing | ExportSettings | ImportSettings => Role::Owner,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_ordering() {
        assert!(Role::Viewer < Role::User);
        assert!(Role::User < Role::Admin);
        assert!(Role::Admin < Role::Owner);
    }

    #[test]
    fn test_viewer_permissions() {
        let role = Role::Viewer;
        assert!(role.has_permission(Permission::ViewChat));
        assert!(role.has_permission(Permission::ViewJobs));
        assert!(role.has_permission(Permission::ViewGatewayStatus));
        // Cannot send messages
        assert!(!role.has_permission(Permission::SendMessage));
        assert!(!role.has_permission(Permission::InstallExtension));
        assert!(!role.has_permission(Permission::ShutdownGateway));
    }

    #[test]
    fn test_user_permissions() {
        let role = Role::User;
        // Inherits viewer
        assert!(role.has_permission(Permission::ViewChat));
        // Own permissions
        assert!(role.has_permission(Permission::SendMessage));
        assert!(role.has_permission(Permission::ApproveToolCall));
        assert!(role.has_permission(Permission::TriggerRoutine));
        // Cannot admin
        assert!(!role.has_permission(Permission::InstallExtension));
        assert!(!role.has_permission(Permission::ModifySettings));
        assert!(!role.has_permission(Permission::ShutdownGateway));
    }

    #[test]
    fn test_admin_permissions() {
        let role = Role::Admin;
        // Inherits user
        assert!(role.has_permission(Permission::SendMessage));
        // Own permissions
        assert!(role.has_permission(Permission::InstallExtension));
        assert!(role.has_permission(Permission::RemoveExtension));
        assert!(role.has_permission(Permission::ModifySettings));
        assert!(role.has_permission(Permission::CancelJob));
        assert!(role.has_permission(Permission::ChangeLogLevel));
        // Cannot owner
        assert!(!role.has_permission(Permission::ShutdownGateway));
        assert!(!role.has_permission(Permission::ManagePairing));
        assert!(!role.has_permission(Permission::ExportSettings));
    }

    #[test]
    fn test_owner_permissions() {
        let role = Role::Owner;
        // Can do everything
        assert!(role.has_permission(Permission::ViewChat));
        assert!(role.has_permission(Permission::SendMessage));
        assert!(role.has_permission(Permission::InstallExtension));
        assert!(role.has_permission(Permission::ShutdownGateway));
        assert!(role.has_permission(Permission::ManagePairing));
        assert!(role.has_permission(Permission::ExportSettings));
        assert!(role.has_permission(Permission::ImportSettings));
    }

    #[test]
    fn test_default_role_is_user() {
        assert_eq!(Role::default(), Role::User);
    }

    #[test]
    fn test_role_serde_roundtrip() {
        let role = Role::Admin;
        let json = serde_json::to_string(&role).expect("serialize");
        assert_eq!(json, "\"admin\"");
        let back: Role = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Role::Admin);
    }

    #[test]
    fn test_role_display() {
        assert_eq!(format!("{}", Role::Viewer), "viewer");
        assert_eq!(format!("{}", Role::Owner), "owner");
    }
}
