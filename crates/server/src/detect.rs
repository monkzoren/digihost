//! Working out how a repository wants to be deployed.
//!
//! An operator should not need DigiHost's vocabulary to register an
//! application: they point at a repository, DigiHost looks at what is in it
//! and proposes a strategy, an entrypoint and a port. Everything proposed is
//! editable — a guess that cannot be corrected is worse than no guess — and a
//! guess the code is not confident about says so instead of presenting itself
//! as fact.

use serde::Serialize;

/// What DigiHost thinks a repository is, and why.
#[derive(Debug, Serialize, PartialEq)]
pub struct Detected {
    pub strategy: &'static str,
    /// Shown to the operator, so the guess is inspectable rather than magic.
    pub because: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// False when nothing matched and this is only a fallback.
    pub confident: bool,
}

/// Decide from the names of the files at the repository root.
///
/// Root-only on purpose: it is one cheap API call, and every convention that
/// matters here lives at the top level. Order matters — a repository with
/// both a compose file and a Dockerfile is a compose project.
pub fn from_root_files(files: &[String]) -> Detected {
    let has = |name: &str| files.iter().any(|f| f.eq_ignore_ascii_case(name));
    let any_ending =
        |suffix: &str| files.iter().any(|f| f.to_ascii_lowercase().ends_with(suffix));

    if has("docker-compose.yml")
        || has("docker-compose.yaml")
        || has("compose.yml")
        || has("compose.yaml")
    {
        return Detected {
            strategy: "Docker Compose",
            because: "a compose file at the repository root".to_string(),
            entrypoint: None,
            port: None,
            confident: true,
        };
    }

    if has("dockerfile") {
        return Detected {
            strategy: "Dockerfile",
            because: "a Dockerfile at the repository root".to_string(),
            entrypoint: None,
            port: None,
            confident: true,
        };
    }

    if any_ending(".csproj") || any_ending(".sln") {
        return Detected {
            strategy: "IIS site swap",
            because: "a .NET project".to_string(),
            entrypoint: None,
            port: Some(80),
            confident: true,
        };
    }

    if has("package.json") {
        return Detected {
            strategy: "systemd unit",
            because: "a Node project".to_string(),
            entrypoint: Some("node .".to_string()),
            port: Some(3000),
            confident: true,
        };
    }

    if has("index.html") {
        return Detected {
            strategy: "Static files",
            because: "an index.html at the repository root".to_string(),
            entrypoint: None,
            port: None,
            confident: true,
        };
    }

    Detected {
        strategy: "Static files",
        because: "nothing recognisable — publishing the files as-is is the safe default"
            .to_string(),
        entrypoint: None,
        port: None,
        confident: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn compose_beats_dockerfile() {
        // A repository often has both; the compose file is the one that
        // describes how to run it.
        let d = from_root_files(&files(&["Dockerfile", "docker-compose.yml", "src"]));
        assert_eq!(d.strategy, "Docker Compose");
        assert!(d.confident);
    }

    #[test]
    fn a_lone_dockerfile_is_buildable() {
        let d = from_root_files(&files(&["Dockerfile", "app.py"]));
        assert_eq!(d.strategy, "Dockerfile");
        assert!(d.confident);
    }

    #[test]
    fn recognises_common_project_shapes() {
        assert_eq!(from_root_files(&files(&["Api.csproj"])).strategy, "IIS site swap");
        assert_eq!(from_root_files(&files(&["App.sln", "src"])).strategy, "IIS site swap");
        assert_eq!(from_root_files(&files(&["package.json"])).strategy, "systemd unit");
        assert_eq!(
            from_root_files(&files(&["index.html", "styles.css"])).strategy,
            "Static files"
        );
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(from_root_files(&files(&["DOCKERFILE"])).strategy, "Dockerfile");
        assert_eq!(from_root_files(&files(&["Index.HTML"])).strategy, "Static files");
    }

    #[test]
    fn an_unrecognised_repository_falls_back_without_pretending() {
        let d = from_root_files(&files(&["README.md", "LICENSE"]));
        assert_eq!(d.strategy, "Static files");
        assert!(!d.confident, "a fallback must not claim confidence");
    }

    #[test]
    fn node_projects_get_a_usable_starting_point() {
        let d = from_root_files(&files(&["package.json"]));
        assert!(d.entrypoint.is_some(), "propose something runnable");
        assert_eq!(d.port, Some(3000));
    }
}
