//! references.yaml schema v1.0.0
//!
//! Central schema for reference tracking and verification.

use serde::{Deserialize, Serialize};

/// Root structure for references.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesFile {
    pub meta: Meta,
    pub references: Vec<Reference>,
}

/// Metadata about the references file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// ISO date when file was created
    pub created: String,
    /// ISO datetime of last verification run (null if never)
    pub last_verified: Option<String>,
    /// Tool used for verification
    pub tool: String,
    /// Total number of references
    pub total_links: usize,
}

/// A single reference entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    /// URL to verify
    pub url: String,
    /// Human-readable title
    pub title: String,
    /// Categories for filtering (e.g., `["research", "wikipedia"]`)
    pub categories: Vec<String>,
    /// Files that cite this reference
    pub cited_in: Vec<String>,
    /// Verification status
    pub status: Status,
    /// ISO datetime of last verification (null if pending)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
    /// Notes (redirect target URL, error message, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Reference verification status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not yet verified
    Pending,
    /// 200 response, content accessible
    Ok,
    /// 404, 5xx, DNS failure, connection error
    Dead,
    /// Ended up on different domain (link rot)
    Redirect,
    /// 200 but content blocked by paywall
    Paywall,
    /// 200 but login required to view content
    Login,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Pending => write!(f, "pending"),
            Status::Ok => write!(f, "ok"),
            Status::Dead => write!(f, "dead"),
            Status::Redirect => write!(f, "redirect"),
            Status::Paywall => write!(f, "paywall"),
            Status::Login => write!(f, "login"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_display() {
        assert_eq!(Status::Pending.to_string(), "pending");
        assert_eq!(Status::Ok.to_string(), "ok");
        assert_eq!(Status::Dead.to_string(), "dead");
        assert_eq!(Status::Redirect.to_string(), "redirect");
        assert_eq!(Status::Paywall.to_string(), "paywall");
        assert_eq!(Status::Login.to_string(), "login");
    }

    #[test]
    fn test_deserialize_status() {
        let json = r#""ok""#;
        let status: Status = serde_json::from_str(json).unwrap();
        assert_eq!(status, Status::Ok);
    }

    #[test]
    fn test_serialize_reference() {
        let reference = Reference {
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            categories: vec!["test".to_string()],
            cited_in: vec!["README.md".to_string()],
            status: Status::Pending,
            verified: None,
            notes: None,
        };
        let yaml = serde_yaml::to_string(&reference).unwrap();
        assert!(yaml.contains("url: https://example.com"));
        assert!(yaml.contains("status: pending"));
        // Optional fields should not appear when None
        assert!(!yaml.contains("verified:"));
        assert!(!yaml.contains("notes:"));
    }

    #[test]
    fn test_full_file_roundtrip() {
        let file = ReferencesFile {
            meta: Meta {
                created: "2025-12-15".to_string(),
                last_verified: None,
                tool: "ref".to_string(),
                total_links: 1,
            },
            references: vec![Reference {
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
                categories: vec!["test".to_string()],
                cited_in: vec!["README.md".to_string()],
                status: Status::Ok,
                verified: Some("2025-12-15T10:00:00Z".to_string()),
                notes: None,
            }],
        };
        let yaml = serde_yaml::to_string(&file).unwrap();
        let parsed: ReferencesFile = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.meta.total_links, 1);
        assert_eq!(parsed.references[0].status, Status::Ok);
    }
}
